use crate::{
    operations::Cancellation,
    vfs::{self, Location},
};
use memchr::memmem;
use std::{
    fs::File,
    io::{self, Read, Seek, SeekFrom},
    ops::Range,
    time::{Duration, SystemTime},
};

pub(super) const READ_BLOCK: usize = 64 * 1024;

#[derive(Clone, Copy, PartialEq, Eq)]
pub(super) struct Version {
    pub size: Option<u64>,
    changed: Option<SystemTime>,
    identity: Option<(u64, u64)>,
}

pub(super) enum Source {
    Local(File),
    Sftp(ssh2::File),
}

impl Source {
    pub(super) fn open(location: &Location) -> Result<Self, String> {
        if let Some(path) = location.local() {
            return super::open_regular(path).map(Self::Local);
        }
        vfs::open_preview_stream(location).map(Self::Sftp)
    }

    pub(super) fn version(&mut self) -> io::Result<Version> {
        match self {
            Self::Local(file) => {
                let metadata = file.metadata()?;
                #[cfg(unix)]
                let identity = {
                    use std::os::unix::fs::MetadataExt;
                    Some((metadata.dev(), metadata.ino()))
                };
                #[cfg(not(unix))]
                let identity = None;
                Ok(Version {
                    size: Some(metadata.len()),
                    changed: metadata.modified().ok(),
                    identity,
                })
            }
            Self::Sftp(file) => {
                let stat = file.stat().map_err(io::Error::other)?;
                Ok(Version {
                    size: stat.size,
                    changed: stat.mtime.and_then(|time| {
                        SystemTime::UNIX_EPOCH.checked_add(Duration::from_secs(time))
                    }),
                    identity: None,
                })
            }
        }
    }

    pub(super) fn seek_to(&mut self, byte: u64) -> io::Result<u64> {
        match self {
            Self::Local(file) => file.seek(SeekFrom::Start(byte)),
            Self::Sftp(file) => file.seek(SeekFrom::Start(byte)),
        }
    }
}

impl Read for Source {
    fn read(&mut self, bytes: &mut [u8]) -> io::Result<usize> {
        match self {
            Self::Local(file) => file.read(bytes),
            Self::Sftp(file) => file.read(bytes),
        }
    }
}

pub(super) struct Window<'a> {
    source: &'a mut Source,
    cancel: &'a Cancellation,
    size: Option<u64>,
    offset: u64,
}

impl<'a> Window<'a> {
    pub(super) fn new(source: &'a mut Source, size: Option<u64>, cancel: &'a Cancellation) -> Self {
        Self {
            source,
            cancel,
            size,
            offset: 0,
        }
    }

    #[cfg(test)]
    pub(super) fn offset(&self) -> u64 {
        self.offset
    }

    pub(super) fn read_at(&mut self, byte: u64, buffer: &mut [u8]) -> io::Result<usize> {
        self.cancel.check()?;
        let limit = self
            .size
            .unwrap_or(u64::MAX)
            .saturating_sub(byte)
            .min(buffer.len() as u64) as usize;
        if limit == 0 {
            return Ok(0);
        }
        if self.offset != byte {
            self.source.seek_to(byte)?;
            self.offset = byte;
        }
        let mut used = 0;
        while used < limit {
            self.cancel.check()?;
            match self.source.read(&mut buffer[used..limit]) {
                Ok(0) => break,
                Ok(count) => {
                    used += count;
                    self.offset += count as u64;
                }
                Err(error) if error.kind() == io::ErrorKind::Interrupted => {}
                Err(error) => return Err(error),
            }
        }
        Ok(used)
    }

    pub(super) fn read_exact_at<'b>(
        &mut self,
        byte: u64,
        buffer: &'b mut [u8],
    ) -> io::Result<&'b [u8]> {
        if self.read_at(byte, buffer)? != buffer.len() {
            return Err(io::ErrorKind::UnexpectedEof.into());
        }
        Ok(buffer)
    }

    /// The callback receives the bytes passed before the match. Text previews
    /// use it to keep the original line number without rereading the file.
    pub(super) fn find_forward(
        &mut self,
        range: Range<u64>,
        finder: &memmem::Finder<'_>,
        alignment: usize,
        mut passed: impl FnMut(&[u8]),
    ) -> io::Result<Option<u64>> {
        let overlap = finder.needle().len() - alignment;
        let mut buffer = vec![0; READ_BLOCK + overlap];
        let mut byte = range.start;
        let mut carried = 0;
        while byte + (carried as u64) < range.end {
            let offset = byte + carried as u64;
            let wanted = (range.end - offset).min(READ_BLOCK as u64) as usize;
            let fresh = self.read_at(offset, &mut buffer[carried..carried + wanted])?;
            let used = carried + fresh;
            if let Some(index) = aligned_find(&buffer[..used], finder, alignment) {
                passed(&buffer[..index]);
                return Ok(Some(byte + index as u64));
            }
            if fresh < wanted {
                break;
            }
            carried = overlap.min(used);
            let consumed = used - carried;
            passed(&buffer[..consumed]);
            byte += consumed as u64;
            buffer.copy_within(consumed..used, 0);
        }
        Ok(None)
    }

    pub(super) fn find_backward(
        &mut self,
        range: Range<u64>,
        finder: &memmem::FinderRev<'_>,
        alignment: usize,
        mut passed: impl FnMut(&[u8]),
    ) -> io::Result<Option<u64>> {
        let overlap = (finder.needle().len() - alignment) as u64;
        let mut buffer = vec![0; READ_BLOCK + overlap as usize];
        let mut end = range.end;
        while end > range.start {
            let byte = end.saturating_sub(READ_BLOCK as u64).max(range.start);
            let block = (end - byte) as usize;
            let tail = overlap.min(range.end - end) as usize;
            let bytes = self.read_exact_at(byte, &mut buffer[..block + tail])?;
            if let Some(index) = aligned_rfind(bytes, finder, alignment) {
                // The overlap is shorter than the query, so each match starts
                // within this block.
                passed(&bytes[index..block]);
                return Ok(Some(byte + index as u64));
            }
            passed(&bytes[..block]);
            end = byte;
        }
        Ok(None)
    }
}

fn aligned_find(bytes: &[u8], finder: &memmem::Finder<'_>, width: usize) -> Option<usize> {
    let mut offset = 0;
    while let Some(relative) = finder.find(&bytes[offset..]) {
        let index = offset + relative;
        if index.is_multiple_of(width) {
            return Some(index);
        }
        offset = index + 1;
    }
    None
}

fn aligned_rfind(bytes: &[u8], finder: &memmem::FinderRev<'_>, width: usize) -> Option<usize> {
    let mut end = bytes.len();
    while let Some(index) = finder.rfind(&bytes[..end]) {
        if index.is_multiple_of(width) {
            return Some(index);
        }
        end = index + finder.needle().len() - 1;
    }
    None
}
