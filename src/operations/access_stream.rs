use super::TransferError;
use crate::{
    file_error::{AccessAction, AccessError},
    vfs::Location,
};
use std::io::{self, Read, Write};

/// Preserve the operation and path through code that uses the standard I/O traits.
pub(crate) fn access_io(location: &Location, action: AccessAction, source: io::Error) -> io::Error {
    if TransferError::is_cancelled_io(&source) {
        return source;
    }
    io::Error::new(source.kind(), AccessError::stream(location, action, source))
}

pub(crate) struct AccessStream<'a, T> {
    inner: T,
    location: &'a Location,
}

impl<'a, T> AccessStream<'a, T> {
    pub(crate) fn new(inner: T, location: &'a Location) -> Self {
        Self { inner, location }
    }
}

impl<T: Read> Read for AccessStream<'_, T> {
    fn read(&mut self, buffer: &mut [u8]) -> io::Result<usize> {
        self.inner
            .read(buffer)
            .map_err(|source| access_io(self.location, AccessAction::ReadFile, source))
    }
}

impl<T: Write> Write for AccessStream<'_, T> {
    fn write(&mut self, buffer: &[u8]) -> io::Result<usize> {
        let written = self
            .inner
            .write(buffer)
            .map_err(|source| access_io(self.location, AccessAction::WriteFile, source))?;
        if written == 0 && !buffer.is_empty() {
            return Err(access_io(
                self.location,
                AccessAction::WriteFile,
                io::ErrorKind::WriteZero.into(),
            ));
        }
        Ok(written)
    }

    fn flush(&mut self) -> io::Result<()> {
        self.inner
            .flush()
            .map_err(|source| access_io(self.location, AccessAction::FlushFile, source))
    }
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;
    use crate::{
        file_error::{AccessCause, Failure},
        operations::Progress,
    };

    struct Fault;

    impl Read for Fault {
        fn read(&mut self, _: &mut [u8]) -> io::Result<usize> {
            Err(io::Error::from_raw_os_error(libc::EIO))
        }
    }

    impl Write for Fault {
        fn write(&mut self, _: &[u8]) -> io::Result<usize> {
            Err(io::Error::from_raw_os_error(libc::EACCES))
        }

        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }

    #[test]
    fn copy_reports_the_path_and_operation_of_each_original_io_failure() {
        let source = Location::Local("/fixture/source".into());
        let target = Location::Local("/fixture/target".into());
        let mut progress = Progress::default();
        let read = progress
            .copy(
                source.path(),
                Some(1),
                &mut AccessStream::new(Fault, &source),
                &mut io::sink(),
            )
            .unwrap_err();
        let write = progress
            .copy(
                source.path(),
                Some(1),
                &mut b"x".as_slice(),
                &mut AccessStream::new(Fault, &target),
            )
            .unwrap_err();
        for (error, path, action, errno) in [
            (read, source, AccessAction::ReadFile, libc::EIO),
            (write, target, AccessAction::WriteFile, libc::EACCES),
        ] {
            let TransferError::Failed(Failure::Access(failure)) = TransferError::from(error) else {
                panic!("expected typed access failure");
            };
            assert_eq!(failure.location, path);
            assert_eq!(failure.action, action);
            assert!(
                matches!(failure.cause, AccessCause::Local(source) if source.raw_os_error() == Some(errno))
            );
        }
    }
}
