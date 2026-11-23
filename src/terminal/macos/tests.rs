use super::*;
use std::ffi::OsString;
use std::os::unix::ffi::OsStringExt;
use std::path::PathBuf;
use std::process::{Output, Stdio};
use std::time::{Duration, Instant};

#[test]
fn terminal_passes_local_folder_as_one_native_argument() {
    let path = OsString::from_vec(b"/tmp/saved folder ' & % \n\xff".to_vec());
    let command = document_command(Path::new(&path));
    assert_eq!(command.get_program(), "/usr/bin/open");
    assert_eq!(
        command.get_args().collect::<Vec<_>>(),
        [
            OsStr::new("-b"),
            OsStr::new("com.apple.Terminal"),
            OsStr::new("--"),
            path.as_os_str(),
        ]
    );
}

#[test]
fn terminal_preserves_a_missing_folder_errno_and_the_open_rejection() {
    let root = tempfile::tempdir().unwrap();
    let missing = Location::Local(root.path().join("missing folder"));
    assert!(matches!(
        open(&missing),
        Err(OpenError::ReadFolder(error)) if error.raw_os_error() == Some(libc::ENOENT)
    ));

    let mut reject = Command::new("/bin/sh");
    reject.args(["-c", r"printf 'fixture: \377\n' >&2; exit 73"]);
    assert!(matches!(
        system_open::run_command(reject),
        Err(system_open::Error::Rejected { status, stderr })
            if status.code() == Some(73) && stderr == b"fixture: \xff\n"
    ));
}

#[test]
fn terminal_launcher_is_private_preserves_bytes_and_removes_itself() {
    let arg = OsString::from_vec(b"value '\" * & \\ % \n\xff".to_vec());
    let key = OsStr::new("key ' with %h ${USER}");
    let mut command = Command::new("/bin/sh");
    command
        .args([
            "-c",
            r#"printf '%s\0' "$PAWCOMMANDER_TERMINAL_IDENTITY_FILE" "$1""#,
            "fixture",
        ])
        .arg(&arg)
        .env(IDENTITY_PATH, key);
    let launcher = create_launcher(&command).unwrap();
    assert_eq!(
        launcher.as_file().metadata().unwrap().permissions().mode() & 0o777,
        0o700
    );
    let output = Command::new(launcher.path()).output().unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(
        output.stdout,
        [key.as_bytes(), b"\0", arg.as_bytes(), b"\0"].concat()
    );
    assert!(!launcher.path().exists());
}

#[test]
fn terminal_ssh_uses_explicit_coordinates_and_validates_folder() {
    let mut endpoint = Endpoint {
        host: "[::1]".into(),
        port: 2222,
        user: "guest@example.test".into(),
        key: None,
        jump: None,
    };
    let command = ssh_command(&endpoint, Path::new("/root/draft ' é\n")).unwrap();
    let args = command.get_args().collect::<Vec<_>>();
    assert_eq!(
        &args[..7],
        ["-tt", "-p", "2222", "-l", "guest@example.test", "--", "::1"]
    );
    assert_eq!(args.len(), 8);
    assert_eq!(command.get_envs().count(), 0);
    assert!(matches!(
        ssh_command(&endpoint, Path::new("relative")),
        Err(SshCommandError::Folder)
    ));
    assert!(matches!(
        ssh_command(&endpoint, Path::new("/bad\0path")),
        Err(SshCommandError::Folder)
    ));
    endpoint.host = "-option".into();
    assert!(matches!(
        ssh_command(&endpoint, Path::new("/")),
        Err(SshCommandError::Host)
    ));
    endpoint.host = "localhost".into();
    endpoint.user = "first\nsecond".into();
    assert!(matches!(
        ssh_command(&endpoint, Path::new("/")),
        Err(SshCommandError::UserOrPort)
    ));
}

#[test]
#[ignore = "requires disposable OpenSSH server; run PAWCOMMANDER_TEST_SFTP_TERMINAL=1 bash scripts/test-sftp.sh"]
fn openssh_terminal_starts_interactively_in_the_exact_folder() {
    let root = PathBuf::from(std::env::var_os("PAWCOMMANDER_TEST_SFTP_ROOT").unwrap());
    let endpoint = Endpoint {
        host: std::env::var("PAWCOMMANDER_TEST_SFTP_HOST").unwrap(),
        port: std::env::var("PAWCOMMANDER_TEST_SFTP_PORT")
            .unwrap()
            .parse()
            .unwrap(),
        user: std::env::var("PAWCOMMANDER_TEST_SFTP_USER").unwrap(),
        key: Some(PathBuf::from(
            std::env::var_os("PAWCOMMANDER_TEST_TERMINAL_KEY").unwrap(),
        )),
        jump: None,
    };
    let folder = root.join("reports ' \" é & [draft] ! %h ${USER}\n");
    std::fs::create_dir(&folder).unwrap();
    let known_hosts = root.join("known_hosts");
    let output = run_terminal_ssh(&endpoint, &folder, &known_hosts);
    assert!(
        output.status.success(),
        "{}: {}",
        output.status,
        String::from_utf8_lossy(&output.stderr)
    );
    let expected =
        folder
            .as_os_str()
            .as_bytes()
            .iter()
            .fold(String::from("PAW-CWD="), |mut out, byte| {
                write!(out, "{byte:02x}").unwrap();
                out
            });
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(stdout.contains(&expected), "{stdout}");
    assert!(stdout.contains("PAW-INTERACTIVE=1"), "{stdout}");
    assert!(stdout.contains("PAW-TTY=1"), "{stdout}");

    let missing = run_terminal_ssh(&endpoint, &folder.join("missing"), &known_hosts);
    assert_eq!(missing.status.code(), Some(1));
    assert!(!String::from_utf8_lossy(&missing.stdout).contains("PAW-INTERACTIVE=1"));
    let unknown_host = run_terminal_ssh(&endpoint, &folder, &root.join("empty_known_hosts"));
    assert_eq!(unknown_host.status.code(), Some(255));
    assert!(!String::from_utf8_lossy(&unknown_host.stdout).contains("PAW-INTERACTIVE=1"));
}

fn run_terminal_ssh(endpoint: &Endpoint, path: &Path, hosts: &Path) -> Output {
    let command = ssh_command(endpoint, path).unwrap();
    let mut fixture = Command::new(command.get_program());
    fixture
        .args([
            "-F",
            "/dev/null",
            "-o",
            "IdentityAgent=none",
            "-o",
            "IdentitiesOnly=yes",
            "-o",
            "BatchMode=yes",
            "-o",
            "StrictHostKeyChecking=yes",
            "-o",
            "GlobalKnownHostsFile=/dev/null",
            "-o",
            "ConnectTimeout=4",
            "-o",
        ])
        .arg(format!("UserKnownHostsFile={}", hosts.display()))
        .args(command.get_args());
    for (name, value) in command.get_envs() {
        fixture.env(name, value.unwrap());
    }
    let launcher = create_launcher(&fixture).unwrap();
    let mut child = Command::new(launcher.path())
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    let _ = child.stdin.take().unwrap().write_all(br#"value=$(printf '%s' "$PWD" | /usr/bin/od -An -v -tx1 | /usr/bin/tr -d '[:space:]'); printf 'PAW-CWD=%s\n' "$value"
case "$-" in *i*) printf 'PAW-INTERACTIVE=%d\n' 1;; esac
if test -t 0; then printf 'PAW-TTY=%d\n' 1; fi
exit 0
"#);
    let deadline = Instant::now() + Duration::from_secs(10);
    while child.try_wait().unwrap().is_none() {
        if Instant::now() >= deadline {
            let _ = child.kill();
            let output = child.wait_with_output().unwrap();
            panic!(
                "disposable terminal SSH did not finish: {} {}",
                String::from_utf8_lossy(&output.stdout),
                String::from_utf8_lossy(&output.stderr)
            );
        }
        std::thread::sleep(Duration::from_millis(10));
    }
    assert!(!launcher.path().exists());
    child.wait_with_output().unwrap()
}
