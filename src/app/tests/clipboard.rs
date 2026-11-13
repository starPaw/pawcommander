use super::*;

#[test]
fn unrevisioned_file_clipboard_uses_observed_contents() {
    let file_list = FileList {
        paths: vec![PathBuf::from("first.txt")],
        cut: true,
        revision: None,
    };
    let mut different = file_list.clone();
    different.paths.push(PathBuf::from("second.txt"));
    let clipboard = Clipboard::new(Vec::new(), true, ClipboardOrigin::System(file_list.clone()));
    assert!(clipboard.matches(Some(&file_list)));
    assert!(!clipboard.matches(Some(&different)));
    assert!(!clipboard.matches(None));
}
