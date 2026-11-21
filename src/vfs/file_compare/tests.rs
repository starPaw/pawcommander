use super::*;
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
    let error = compare_files(&paths, &mut Progress::default())
        .unwrap_err()
        .to_string();
    assert_eq!(
        error,
        format!("{} · {}", text!("Правая панель"), text!("Файл отсутствует"))
    );

    fs::write(paths[1].path(), b"xyz").unwrap();
    let cancel = Cancellation::default();
    cancel.request();
    assert!(matches!(
        compare_files(&paths, &mut Progress::with_cancellation(cancel)),
        Err(TransferError::Cancelled)
    ));
}
