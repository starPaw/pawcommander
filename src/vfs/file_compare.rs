//! Compare regular local or SFTP files without writing either source.
mod lines;
#[cfg(test)]
mod tests;
#[cfg(all(test, target_os = "macos"))]
pub(super) use tests::file_comparison_round_trip;

use super::{
    compare::side_error,
    read_scope::{CheckedFile, ReadScope},
    transfer::canonical,
    Location,
};
use crate::{
    file_error::{AccessError, MutationError, ScanError},
    operations::{Cancellation, Progress, TransferError},
    preview::decode_comparison_text,
};
use std::{
    io::{self, Read},
    path::Path,
};

pub(crate) use lines::{DiffKind, DiffRow, TextDifference};

const TEXT_BYTES: u64 = 2 * 1024 * 1024;
const BLOCK: usize = 64 * 1024;

#[derive(Debug)]
pub(crate) struct FileComparison {
    pub(crate) sizes: [u64; 2],
    pub(crate) first_difference: Option<u64>,
    pub(crate) difference: FileDifference,
}

#[derive(Debug)]
pub(crate) enum FileDifference {
    Equal,
    Binary,
    Text(TextDifference),
    TextSizeLimit,
    TextLineLimit,
}

pub(crate) fn compare_files(
    paths: &[Location; 2],
    progress: &mut Progress,
) -> Result<FileComparison, TransferError> {
    let cancel = progress.cancellation();
    cancel.check()?;
    progress.item(paths[0].path());
    let first_root = FileInput::open(&paths[0]).map_err(|error| side_error(0, error))?;
    let mut left = first_root
        .file(&cancel)
        .map_err(|error| side_error(0, error))?;
    cancel.check()?;
    progress.item(paths[1].path());
    let second_root = FileInput::open(&paths[1]).map_err(|error| side_error(1, error))?;
    let mut right = second_root
        .file(&cancel)
        .map_err(|error| side_error(1, error))?;
    let result = compare_readers([left.stream(), right.stream()], progress, &cancel)?;
    first_root
        .verify(&mut left, &cancel)
        .map_err(|error| side_error(0, error))?;
    second_root
        .verify(&mut right, &cancel)
        .map_err(|error| side_error(1, error))?;
    Ok(result)
}

struct FileInput<'a> {
    file: &'a Location,
    parent: Location,
    scope: ReadScope,
    name: &'a Path,
}

impl<'a> FileInput<'a> {
    fn open(path: &'a Location) -> Result<Self, TransferError> {
        if path.is_archive() {
            return Err(MutationError::ReadOnlyArchive(path.clone()).into());
        }
        let unsupported = || ScanError::ExpectedRegularFile(path.clone());
        let parent = path.parent().ok_or_else(unsupported)?;
        let name = path
            .path()
            .file_name()
            .map(Path::new)
            .ok_or_else(unsupported)?;
        let containing_folder = |cause| folder_error(path, cause);
        let scope = ReadScope::open(&parent).map_err(containing_folder)?;
        #[cfg(target_os = "macos")]
        if scope.descriptor.is_some() {
            // Confirm that the opened directory and the current path identify the same folder.
            super::guarded_directory::Directory::from_scope(&scope).map_err(containing_folder)?;
        }
        Ok(Self {
            file: path,
            parent,
            scope,
            name,
        })
    }

    fn file(&self, cancel: &Cancellation) -> Result<CheckedFile<'_>, TransferError> {
        self.scope.require_file(self.name, cancel)
    }

    fn verify(
        &self,
        input: &mut CheckedFile<'_>,
        cancel: &Cancellation,
    ) -> Result<(), TransferError> {
        input.verify(cancel)?;
        let containing_folder = |cause| folder_error(self.file, cause);
        // The descriptor can still read a folder that has moved away from the selected path.
        self.scope.verify_root().map_err(containing_folder)?;
        let captured = self.scope.parent.location.path();
        if self.parent.path() != captured
            && canonical(&self.parent).map_err(containing_folder)? != captured
        {
            return Err(containing_folder(
                ScanError::FolderUnavailable(self.parent.clone()).into(),
            ));
        }
        Ok(())
    }
}

fn folder_error(file: &Location, cause: TransferError) -> TransferError {
    if matches!(cause, TransferError::Cancelled) {
        return cause;
    }
    ScanError::ComparisonFolder {
        file: file.clone(),
        cause: Box::new(cause),
    }
    .into()
}

fn compare_readers<R: Read>(
    inputs: [(&Location, u64, &mut R); 2],
    progress: &mut Progress,
    cancel: &Cancellation,
) -> Result<FileComparison, TransferError> {
    let [(left_path, left_size, left), (right_path, right_size, right)] = inputs;
    let sizes = [left_size, right_size];
    let (first_difference, difference) = if sizes.iter().all(|&size| size <= TEXT_BYTES) {
        let left = read_small(left_path, left_size, left, progress)
            .map_err(|error| side_error(0, error))?;
        let right = read_small(right_path, right_size, right, progress)
            .map_err(|error| side_error(1, error))?;
        let first = difference_at(&left, &right);
        let kind = if first.is_none() {
            FileDifference::Equal
        } else if let (Some(left), Some(right)) = (
            decode_comparison_text(&left, false),
            decode_comparison_text(&right, false),
        ) {
            match lines::compare(&left, &right, cancel)? {
                Some(diff) => FileDifference::Text(diff),
                None => FileDifference::TextLineLimit,
            }
        } else {
            FileDifference::Binary
        };
        (first.map(|index| index as u64), kind)
    } else {
        let (first, looks_like_text) =
            stream_difference([left, right], sizes, cancel).map_err(|error| match error {
                StreamDifferenceError::Cancelled => TransferError::Cancelled,
                StreamDifferenceError::Read { right, source } => {
                    let location = if right { right_path } else { left_path };
                    side_error(
                        usize::from(right),
                        AccessError::read(location, source).into(),
                    )
                }
            })?;
        let kind = if first.is_none() {
            FileDifference::Equal
        } else if looks_like_text {
            FileDifference::TextSizeLimit
        } else {
            FileDifference::Binary
        };
        (first, kind)
    };
    Ok(FileComparison {
        sizes,
        first_difference,
        difference,
    })
}

fn read_small(
    source: &Location,
    size: u64,
    input: &mut impl Read,
    progress: &mut Progress,
) -> Result<Vec<u8>, TransferError> {
    let mut bytes = Vec::with_capacity(size as usize);
    let read = progress
        .read_stream(source.path(), size, input, |input| {
            input.read_to_end(&mut bytes)?;
            Ok(())
        })
        .map_err(|error| AccessError::read(source, error))?;
    if read != size {
        return Err(ScanError::ShortComparisonRead {
            file: source.clone(),
            expected: size,
            actual: read,
        }
        .into());
    }
    Ok(bytes)
}

pub(super) enum StreamDifferenceError {
    Cancelled,
    Read { right: bool, source: io::Error },
}

pub(super) fn stream_difference(
    inputs: [&mut impl Read; 2],
    sizes: [u64; 2],
    cancel: &Cancellation,
) -> Result<(Option<u64>, bool), StreamDifferenceError> {
    let [left, right] = inputs;
    let [mut a, mut b] = [[0; BLOCK]; 2];
    let [first_a, first_b] = sizes.map(|size| size.min(BLOCK as u64) as usize);
    let check = || cancel.check().map_err(|_| StreamDifferenceError::Cancelled);
    check()?;
    left.read_exact(&mut a[..first_a])
        .map_err(|source| StreamDifferenceError::Read {
            right: false,
            source,
        })?;
    check()?;
    right
        .read_exact(&mut b[..first_b])
        .map_err(|source| StreamDifferenceError::Read {
            right: true,
            source,
        })?;
    check()?;
    let looks_like_text = decode_comparison_text(&a[..first_a], sizes[0] > first_a as u64)
        .is_some()
        && decode_comparison_text(&b[..first_b], sizes[1] > first_b as u64).is_some();
    if let Some(index) = difference_at(&a[..first_a], &b[..first_b]) {
        return Ok((Some(index as u64), looks_like_text));
    }
    let common = sizes[0].min(sizes[1]);
    let mut position = first_a as u64;
    while position < common {
        let count = (common - position).min(BLOCK as u64) as usize;
        check()?;
        left.read_exact(&mut a[..count])
            .map_err(|source| StreamDifferenceError::Read {
                right: false,
                source,
            })?;
        check()?;
        right
            .read_exact(&mut b[..count])
            .map_err(|source| StreamDifferenceError::Read {
                right: true,
                source,
            })?;
        check()?;
        if let Some(index) = difference_at(&a[..count], &b[..count]) {
            return Ok((Some(position + index as u64), looks_like_text));
        }
        position += count as u64;
    }
    Ok(((sizes[0] != sizes[1]).then_some(common), looks_like_text))
}

fn difference_at(left: &[u8], right: &[u8]) -> Option<usize> {
    left.iter()
        .zip(right)
        .position(|(a, b)| a != b)
        .or_else(|| (left.len() != right.len()).then_some(left.len().min(right.len())))
}
