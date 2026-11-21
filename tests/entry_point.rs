#![cfg(target_os = "macos")]

use std::{ffi::OsString, process::ExitCode};

#[test]
fn library_entry_handles_cli_target_errors() {
    let directory = tempfile::tempdir().unwrap();
    let missing = directory.path().join("missing folder");

    let exit = pawcommander::run([OsString::from("open"), missing.into_os_string()]);

    assert_eq!(exit, ExitCode::from(3));
}
