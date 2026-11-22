use super::*;
use crate::file_error::{AccessAction, AccessCause, Failure};
use std::fs;

pub(crate) fn file_comparison_round_trip(paths: &[Location; 2]) {
    let contents = [
        b"heading\nold entry\ncontext\n".as_slice(),
        b"heading\nnew entry\ncontext\n",
    ];
    for (path, bytes) in paths.iter().zip(contents) {
        fs::write(path.path(), bytes).unwrap();
    }
    let compared = compare_files(paths, &mut Progress::default()).unwrap();
    assert_eq!(compared.first_difference, Some(8));
    let FileDifference::Text(text) = compared.difference else {
        panic!("expected a textual comparison");
    };
    assert_eq!((text.added, text.removed), (1, 1));
    let changed: Vec<_> = text
        .lines
        .iter()
        .filter(|line| line.kind != DiffKind::Same)
        .map(|line| (line.kind, line.left, line.right, line.text.as_str()))
        .collect();
    assert_eq!(
        changed,
        [
            (DiffKind::Removed, Some(2), None, "old entry"),
            (DiffKind::Added, None, Some(2), "new entry"),
        ]
    );
    for (path, bytes) in paths.iter().zip(contents) {
        assert_eq!(fs::read(path.path()).unwrap(), bytes);
    }

    let binary = [[0, 1, 255, 2], [0, 1, 255, 4]];
    for (path, bytes) in paths.iter().zip(binary) {
        fs::write(path.path(), bytes).unwrap();
    }
    let compared = compare_files(paths, &mut Progress::default()).unwrap();
    assert_eq!(compared.sizes, [4, 4]);
    assert_eq!(compared.first_difference, Some(3));
    assert!(matches!(compared.difference, FileDifference::Binary));
    for (path, bytes) in paths.iter().zip(binary) {
        assert_eq!(fs::read(path.path()).unwrap(), bytes);
    }

    #[cfg(target_os = "macos")]
    if paths[1].remote() {
        sftp_comparison_failures(paths);
    }
}

fn pane_failure(error: TransferError, right: bool) -> TransferError {
    let TransferError::Failed(Failure::ComparisonPane {
        right: actual,
        cause,
    }) = error
    else {
        panic!("expected an error attached to the comparison pane, got {error:?}");
    };
    assert_eq!(actual, right);
    *cause
}

fn access_failure(error: TransferError, right: bool) -> Box<AccessError> {
    let cause = pane_failure(error, right);
    let TransferError::Failed(Failure::Access(cause)) = cause else {
        panic!("expected the original access error, got {cause:?}");
    };
    cause
}

#[cfg(target_os = "macos")]
fn sftp_comparison_failures(paths: &[Location; 2]) {
    use std::os::unix::fs::PermissionsExt;

    let right = paths[1].path();
    let held = right.with_extension("held");
    fs::rename(right, &held).unwrap();
    let result = compare_files(paths, &mut Progress::default());
    fs::rename(held, right).unwrap();
    let cause = access_failure(result.unwrap_err(), true);
    let canonical_path = paths[1].with_path(fs::canonicalize(right).unwrap());
    assert_eq!(cause.location, canonical_path);
    assert_eq!(cause.action, AccessAction::ReadMetadata);
    assert!(matches!(&cause.cause, AccessCause::Sftp(error)
        if error.code() == ssh2::ErrorCode::SFTP(libssh2_sys::LIBSSH2_FX_NO_SUCH_FILE)
    ));

    // SAFETY: geteuid reads the process identity without arguments.
    if unsafe { libc::geteuid() } != 0 {
        let permissions = fs::metadata(right).unwrap().permissions();
        fs::set_permissions(right, fs::Permissions::from_mode(0o000)).unwrap();
        let result = compare_files(paths, &mut Progress::default());
        fs::set_permissions(right, permissions).unwrap();
        let cause = access_failure(result.unwrap_err(), true);
        assert_eq!(cause.location, canonical_path);
        assert_eq!(cause.action, AccessAction::OpenFile);
        assert!(matches!(&cause.cause, AccessCause::Sftp(error)
            if error.code() == ssh2::ErrorCode::SFTP(libssh2_sys::LIBSSH2_FX_PERMISSION_DENIED)
        ));
    }
    assert_eq!(fs::read(paths[0].path()).unwrap(), [0, 1, 255, 2]);
    assert_eq!(fs::read(right).unwrap(), [0, 1, 255, 4]);
}

#[test]
fn file_comparison_distinguishes_text_and_binary_changes() {
    let root = tempfile::tempdir().unwrap();
    file_comparison_round_trip(
        &["left.txt", "right.txt"].map(|name| root.path().join(name).into()),
    );
}

#[test]
fn file_comparison_distinguishes_decoded_lines_from_file_bytes() {
    let root = tempfile::tempdir().unwrap();
    let paths = ["utf-8.txt", "utf-16.txt"].map(|name| Location::Local(root.path().join(name)));
    fs::write(paths[0].path(), "Привет\nснова\n").unwrap();
    let utf16: Vec<_> = [0xff, 0xfe]
        .into_iter()
        .chain(
            "Привет\r\nснова\r\n"
                .encode_utf16()
                .flat_map(u16::to_le_bytes),
        )
        .collect();
    fs::write(paths[1].path(), utf16).unwrap();
    let result = compare_files(&paths, &mut Progress::default()).unwrap();
    assert_eq!(result.first_difference, Some(0));
    let FileDifference::Text(diff) = result.difference else {
        panic!("expected UTF-16 text");
    };
    assert_eq!((diff.added, diff.removed), (0, 0));
    assert_eq!(diff.lines.len(), 2);
    assert_eq!(diff.lines[0].text, "Привет");

    fs::write(paths[1].path(), fs::read(paths[0].path()).unwrap()).unwrap();
    let equal = compare_files(&paths, &mut Progress::default()).unwrap();
    assert_eq!(equal.first_difference, None);
    assert!(matches!(equal.difference, FileDifference::Equal));
}

#[test]
fn file_comparison_streams_large_files_and_reports_an_exact_offset() {
    let root = tempfile::tempdir().unwrap();
    let paths = ["left", "right"].map(|name| Location::Local(root.path().join(name)));
    let bytes = vec![b'a'; TEXT_BYTES as usize + 17];
    let mut changed = bytes.clone();
    let offset = 3 * BLOCK - 1;
    changed[offset] = b'b';
    fs::write(paths[0].path(), &bytes).unwrap();
    fs::write(paths[1].path(), &changed).unwrap();
    let result = compare_files(&paths, &mut Progress::default()).unwrap();
    assert_eq!(result.first_difference, Some(offset as u64));
    assert!(matches!(result.difference, FileDifference::TextSizeLimit));
    fs::write(paths[1].path(), &bytes[..bytes.len() - 1]).unwrap();
    let length_only = compare_files(&paths, &mut Progress::default()).unwrap();
    assert_eq!(length_only.first_difference, Some(bytes.len() as u64 - 1));
}

#[test]
fn file_comparison_propagates_cancellation_and_names_the_missing_side() {
    let root = tempfile::tempdir().unwrap();
    let paths = ["left", "right"].map(|name| Location::Local(root.path().join(name)));
    fs::write(paths[0].path(), b"abc").unwrap();
    let error = compare_files(&paths, &mut Progress::default()).unwrap_err();
    let cause = access_failure(error, true);
    assert_eq!(
        cause.location,
        Location::Local(fs::canonicalize(root.path()).unwrap().join("right"))
    );
    assert_eq!(cause.action, AccessAction::ReadMetadata);
    assert!(matches!(&cause.cause, AccessCause::Local(error)
        if error.kind() == io::ErrorKind::NotFound));
    #[cfg(target_os = "macos")]
    assert!(matches!(&cause.cause, AccessCause::Local(error)
        if error.raw_os_error() == Some(libc::ENOENT)));

    fs::write(paths[1].path(), b"xyz").unwrap();
    let cancel = Cancellation::default();
    cancel.request();
    assert!(matches!(
        compare_files(&paths, &mut Progress::with_cancellation(cancel)),
        Err(TransferError::Cancelled)
    ));
}

#[cfg(target_os = "macos")]
#[test]
fn buffered_and_streaming_failures_keep_the_actual_side_path_and_errno() {
    struct Reader {
        input: io::Cursor<Vec<u8>>,
        fail_at: u64,
    }

    impl Read for Reader {
        fn read(&mut self, bytes: &mut [u8]) -> io::Result<usize> {
            if self.input.position() >= self.fail_at {
                return Err(io::Error::from_raw_os_error(libc::EIO));
            }
            self.input.read(bytes)
        }
    }

    let paths = ["/comparison-fixture/left", "/comparison-fixture/right"]
        .map(|path| Location::Local(path.into()));
    for (size, fail) in [(32, 1), (TEXT_BYTES + 1, 0)] {
        let [mut left, mut right] = std::array::from_fn(|side| Reader {
            input: io::Cursor::new(vec![b'a'; size as usize]),
            fail_at: if side == fail {
                if size <= TEXT_BYTES {
                    0
                } else {
                    BLOCK as u64
                }
            } else {
                u64::MAX
            },
        });
        let error = compare_readers(
            [(&paths[0], size, &mut left), (&paths[1], size, &mut right)],
            &mut Progress::default(),
            &Cancellation::default(),
        )
        .unwrap_err();
        let cause = access_failure(error, fail != 0);
        assert_eq!(cause.location, paths[fail]);
        assert_eq!(cause.action, AccessAction::ReadFile);
        assert!(matches!(&cause.cause, AccessCause::Local(error)
            if error.raw_os_error() == Some(libc::EIO)
        ));
    }
}

#[test]
fn buffered_short_read_carries_the_expected_and_observed_lengths() {
    let files = ["/comparison-fixture/left", "/comparison-fixture/right"]
        .map(|path| Location::Local(path.into()));
    let error = compare_readers(
        [
            (&files[0], 4, &mut &b"abc"[..]),
            (&files[1], 4, &mut &b"abcd"[..]),
        ],
        &mut Progress::default(),
        &Cancellation::default(),
    )
    .unwrap_err();
    assert!(
        matches!(pane_failure(error, false), TransferError::Failed(Failure::Scan(error))
            if matches!(error.as_ref(), ScanError::ShortComparisonRead { file, expected: 4, actual: 3 }
                if file == &files[0])
        )
    );
}

#[cfg(target_os = "macos")]
#[test]
fn verifies_the_selected_folder_after_reading_an_open_descriptor() {
    use std::os::unix::fs::symlink;

    for alias in [false, true] {
        let temp = tempfile::tempdir().unwrap();
        let original = fs::canonicalize(temp.path()).unwrap().join("original");
        let other = original.with_file_name("other");
        fs::create_dir(&original).unwrap();
        fs::create_dir(&other).unwrap();
        fs::write(original.join("selected"), b"original bytes").unwrap();
        fs::write(other.join("selected"), b"other content!").unwrap();
        let selected_folder = if alias {
            let link = original.with_file_name("alias");
            symlink(&original, &link).unwrap();
            link
        } else {
            original.clone()
        };
        let selected = Location::Local(selected_folder.join("selected"));
        let input = FileInput::open(&selected).unwrap();
        let cancel = Cancellation::default();
        let mut file = input.file(&cancel).unwrap();
        let moved = original.with_file_name("moved");
        if alias {
            fs::remove_file(&selected_folder).unwrap();
            symlink(&other, &selected_folder).unwrap();
        } else {
            fs::rename(&original, &moved).unwrap();
            fs::rename(&other, &original).unwrap();
        }
        let mut bytes = Vec::new();
        file.stream().2.read_to_end(&mut bytes).unwrap();
        assert_eq!(bytes, b"original bytes");
        let error = input.verify(&mut file, &cancel).unwrap_err();
        assert!(matches!(error, TransferError::Failed(Failure::Scan(error))
            if matches!(error.as_ref(), ScanError::ComparisonFolder { file, cause }
                if file == &selected
                    && matches!(cause.as_ref(), TransferError::Failed(Failure::Scan(problem))
                        if matches!(problem.as_ref(), ScanError::FolderUnavailable(path)
                            if path.path() == selected_folder)))
        ));
        let held = if alias { &original } else { &moved };
        assert_eq!(fs::read(held.join("selected")).unwrap(), b"original bytes");
        assert_eq!(fs::read(selected.path()).unwrap(), b"other content!");
    }
}
