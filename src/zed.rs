//! Launch Zed using its SSH URL interface. Authentication remains owned by
//! Zed/OpenSSH; PawCommander's sessions and credential vault are never exported.
use crate::operations::TransferError;
use crate::vfs::{self, Location};
use std::io;
#[cfg(target_os = "macos")]
use std::io::Read;
use std::{
    path::{Path, PathBuf},
    process::{Command, ExitStatus, Stdio},
};

const TEXT_SAMPLE: usize = 8 * 1024;
#[cfg(target_os = "macos")]
const STDERR_LIMIT: usize = 8 * 1024;

pub(crate) enum OpenRequest {
    Local,
    Ssh,
}

#[derive(Debug)]
pub(crate) enum OpenError {
    Archive,
    ResolveLocal(io::Error),
    ProxyJump,
    PathEncoding,
    RemotePath,
    Host(Option<url::ParseError>),
    UserOrPort,
    User,
    Port,
    BaseUrl(url::ParseError),
    Address,
    CliMissing,
    ReadText(Box<TransferError>),
    NotText,
    Launch {
        program: PathBuf,
        source: io::Error,
    },
    Rejected {
        status: ExitStatus,
        #[cfg(target_os = "macos")]
        stderr: io::Result<CliStderr>,
    },
    #[cfg(target_os = "macos")]
    WorkerStart(io::Error),
    #[cfg(target_os = "macos")]
    WorkerStopped,
}

#[cfg(target_os = "macos")]
#[derive(Debug)]
pub(crate) struct CliStderr {
    pub(crate) bytes: Vec<u8>,
    pub(crate) truncated: bool,
}

pub(crate) fn open(location: &Location, text_only: bool) -> Result<OpenRequest, OpenError> {
    let argument = argument(location)?;
    let cli = crate::tools::zed_cli().ok_or(OpenError::CliMissing)?;
    if text_only && location.remote() {
        let (stat, bytes) = vfs::preview_data(location, TEXT_SAMPLE, &Default::default())
            .map_err(|error| OpenError::ReadText(Box::new(error)))?;
        if stat.is_dir()
            || !is_text(
                &bytes[..bytes.len().min(TEXT_SAMPLE)],
                bytes.len() > TEXT_SAMPLE,
            )
        {
            return Err(OpenError::NotText);
        }
    }
    launch(&cli, &argument)?;
    Ok(if location.remote() {
        OpenRequest::Ssh
    } else {
        OpenRequest::Local
    })
}

fn argument(location: &Location) -> Result<std::ffi::OsString, OpenError> {
    let Location::Sftp { endpoint, path } = location else {
        let path = location.local().ok_or(OpenError::Archive)?;
        return Ok(if path.is_absolute() {
            path.to_path_buf()
        } else {
            std::env::current_dir()
                .map_err(OpenError::ResolveLocal)?
                .join(path)
        }
        .into_os_string());
    };
    if endpoint.jump.is_some() {
        return Err(OpenError::ProxyJump);
    }
    let path = path.to_str().ok_or(OpenError::PathEncoding)?;
    if !path.starts_with('/') || path.contains(['\0', '\\']) {
        return Err(OpenError::RemotePath);
    }
    let host = endpoint.host.trim_matches(['[', ']']);
    if host.is_empty()
        || host.starts_with('-')
        || !host
            .chars()
            .all(|c| c.is_alphanumeric() || matches!(c, '.' | '-' | '_' | ':' | '%'))
    {
        return Err(OpenError::Host(None));
    }
    if endpoint.user.is_empty()
        || endpoint.user.starts_with('-')
        || endpoint.user.chars().any(char::is_control)
        || endpoint.port == 0
    {
        return Err(OpenError::UserOrPort);
    }
    let mut url = url::Url::parse("ssh://localhost/").map_err(OpenError::BaseUrl)?;
    let host = if host.contains(':') {
        format!("[{host}]")
    } else {
        host.into()
    };
    url.set_host(Some(&host))
        .map_err(|source| OpenError::Host(Some(source)))?;
    let user =
        percent_encoding::utf8_percent_encode(&endpoint.user, percent_encoding::NON_ALPHANUMERIC)
            .to_string();
    url.set_username(&user).map_err(|()| OpenError::User)?;
    url.set_port(Some(endpoint.port))
        .map_err(|()| OpenError::Port)?;
    // Zed percent-decodes the URL path before opening it. Encode '%' literally
    // once; do not repurpose query parameters as SSH options or carry secrets.
    // Zed interprets a numeric ':line[:column]' suffix after URL decoding.
    // Supply an explicit position only when the filename itself has that form.
    let path_with_position;
    let path = if path
        .rsplit_once(':')
        .is_some_and(|(_, tail)| !tail.is_empty() && tail.bytes().all(|b| b.is_ascii_digit()))
    {
        path_with_position = format!("{path}:1:1");
        &path_with_position
    } else {
        path
    };
    url.path_segments_mut()
        .map_err(|()| OpenError::Address)?
        .clear()
        .extend(path.split('/').skip(1));
    Ok(url.to_string().into())
}

fn launch(cli: &Path, argument: &std::ffi::OsStr) -> Result<(), OpenError> {
    // No shell: quotes, spaces, metacharacters and leading '-' stay data.
    let mut command = command(cli, argument);
    #[cfg(target_os = "macos")]
    let diagnostics = tempfile::NamedTempFile::new().and_then(|file| {
        // The app can outlive its CLI. Read a file instead of waiting for EOF
        // on a pipe that the app may inherit. Reopen for a separate offset.
        command.stderr(file.reopen()?);
        Ok(file)
    });
    let status = command.status().map_err(|source| OpenError::Launch {
        program: cli.to_owned(),
        source,
    })?;
    if status.success() {
        return Ok(());
    }
    Err(OpenError::Rejected {
        status,
        #[cfg(target_os = "macos")]
        stderr: diagnostics.and_then(|file| {
            let mut bytes = Vec::new();
            file.as_file()
                .take((STDERR_LIMIT + 1) as u64)
                .read_to_end(&mut bytes)?;
            let truncated = bytes.len() > STDERR_LIMIT;
            bytes.truncate(STDERR_LIMIT);
            Ok(CliStderr { bytes, truncated })
        }),
    })
}

fn command(cli: &Path, argument: &std::ffi::OsStr) -> Command {
    let mut command = Command::new(cli);
    command
        .arg("--")
        .arg(argument)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    command
}

fn is_text(bytes: &[u8], truncated: bool) -> bool {
    // UTF-16 with a BOM is common for text exported by Windows tools.
    if bytes.starts_with(&[0xff, 0xfe]) || bytes.starts_with(&[0xfe, 0xff]) {
        let little = bytes.starts_with(&[0xff, 0xfe]);
        let mut units = bytes[2..]
            .as_chunks::<2>()
            .0
            .iter()
            .map(|p| {
                if little {
                    u16::from_le_bytes([p[0], p[1]])
                } else {
                    u16::from_be_bytes([p[0], p[1]])
                }
            })
            .collect::<Vec<_>>();
        if truncated
            && units
                .last()
                .is_some_and(|unit| (0xd800..=0xdbff).contains(unit))
        {
            units.pop();
        }
        return (truncated || bytes.len().is_multiple_of(2))
            && std::char::decode_utf16(units).all(|c| match c {
                Ok(c) => text_character(c),
                Err(_) => false,
            });
    }
    let text = match std::str::from_utf8(bytes) {
        Ok(text) => text,
        Err(error) if truncated && error.error_len().is_none() => {
            std::str::from_utf8(&bytes[..error.valid_up_to()]).unwrap_or("")
        }
        Err(_) => return false,
    };
    text.chars().all(text_character)
}

fn text_character(c: char) -> bool {
    !c.is_control() || matches!(c, '\n' | '\r' | '\t' | '\u{c}')
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::vfs::Endpoint;
    use percent_encoding::percent_decode_str;

    fn remote(host: &str, path: &str) -> Location {
        Location::Sftp {
            endpoint: Endpoint {
                host: host.into(),
                user: "alice".into(),
                port: 2222,
                key: Some("/private/key with spaces".into()),
                jump: None,
            },
            path: path.into(),
        }
    }

    #[test]
    fn ssh_url_preserves_endpoint_and_exact_path_without_credentials() {
        for host in ["example.com", "127.0.0.1", "::1", "[2001:db8::1]"] {
            let path = "/папка/100% #?.txt";
            let mut location = remote(host, path);
            if let Location::Sftp { endpoint, .. } = &mut location {
                endpoint.user = "alice.name+test%40".into();
            }
            let arg = argument(&location).unwrap();
            let arg = arg.to_str().unwrap();
            assert!(arg.starts_with("ssh://"));
            assert!(!arg.contains("private/key"));
            let url = url::Url::parse(arg).unwrap();
            assert_eq!(url.port(), Some(2222));
            assert_eq!(
                percent_decode_str(url.username()).decode_utf8().unwrap(),
                "alice.name+test%40"
            );
            assert_eq!(percent_decode_str(url.path()).decode_utf8().unwrap(), path);
            assert!(url.password().is_none());
            assert!(url.query().is_none());
            assert!(url.fragment().is_none());
            assert!(!arg.contains("sftp://"));
        }
    }

    #[test]
    fn rejects_invalid_ssh_coordinates_and_avoids_filename_line_ambiguity() {
        for host in [
            "",
            "-oProxyCommand=bad",
            "host name",
            "user@host",
            "host/path",
            "bad;command",
        ] {
            assert!(
                matches!(
                    argument(&remote(host, "/file.txt")),
                    Err(OpenError::Host(None))
                ),
                "{host}"
            );
        }
        assert!(matches!(
            argument(&remote("[:::]", "/file.txt")),
            Err(OpenError::Host(Some(url::ParseError::InvalidIpv6Address)))
        ));
        for path in ["relative", "/file\0name", "/windows\\path"] {
            assert!(matches!(
                argument(&remote("server", path)),
                Err(OpenError::RemotePath)
            ));
        }
        let mut location = remote("server", "/file.txt");
        if let Location::Sftp { endpoint, .. } = &mut location {
            endpoint.port = 0;
        }
        assert!(matches!(argument(&location), Err(OpenError::UserOrPort)));
        let url = url::Url::parse(
            argument(&remote("server", "/log:12:34"))
                .unwrap()
                .to_str()
                .unwrap(),
        )
        .unwrap();
        assert_eq!(
            percent_decode_str(url.path()).decode_utf8().unwrap(),
            "/log:12:34:1:1"
        );
        let local = Location::Local("-file with spaces;$(command).txt".into());
        let local_arg = argument(&local).unwrap();
        assert!(Path::new(&local_arg).is_absolute());
        assert!(local_arg
            .to_string_lossy()
            .ends_with("-file with spaces;$(command).txt"));
    }

    #[test]
    fn recognizes_text_by_content_and_handles_truncated_unicode() {
        for bytes in [
            b"".as_slice(),
            b"#!/bin/sh\necho ok\n",
            b"KEY=value\r\n",
            "Привет 😀\tworld\n".as_bytes(),
            b"\xef\xbb\xbf{}",
        ] {
            assert!(is_text(bytes, false));
        }
        for bytes in [
            b"\x89PNG\r\n\x1a\n".as_slice(),
            b"\0hello",
            b"\xff\xd8\xff",
            b"a\x01b",
        ] {
            assert!(!is_text(bytes, false));
        }
        let mut bytes = "Hello Привет".as_bytes().to_vec();
        bytes.pop();
        assert!(is_text(&bytes, true));
        assert!(!is_text(&bytes, false));
        for little in [false, true] {
            let mut text = if little {
                vec![0xff, 0xfe]
            } else {
                vec![0xfe, 0xff]
            };
            for unit in "😀 Привет 😀".encode_utf16() {
                text.extend(if little {
                    unit.to_le_bytes()
                } else {
                    unit.to_be_bytes()
                });
            }
            assert!(is_text(&text, false));
            text.truncate(text.len() - 2); // UTF-16 sample ends at a high surrogate.
            assert!(is_text(&text, true));
            assert!(!is_text(&text, false));
        }
    }

    #[cfg(unix)]
    #[test]
    fn launcher_passes_one_literal_argument_without_a_shell_and_reports_failures() {
        use std::os::unix::fs::PermissionsExt;
        let root = tempfile::tempdir().unwrap();
        let cli = root.path().join("fake zed;not a shell");
        std::fs::write(&cli, "#!/bin/sh\nprintf '%s\\n' \"$#\" \"$@\"\n").unwrap();
        std::fs::set_permissions(&cli, std::fs::Permissions::from_mode(0o700)).unwrap();
        let value = "ssh://alice@server:2222/path with 'quotes';$(touch should-not-exist)";
        let output = command(&cli, value.as_ref())
            .current_dir(root.path())
            .stdout(Stdio::piped())
            .output()
            .unwrap();
        assert!(output.status.success());
        assert_eq!(
            String::from_utf8(output.stdout).unwrap(),
            format!("2\n--\n{value}\n")
        );
        assert!(!root.path().join("should-not-exist").exists());
        std::fs::write(
            &cli,
            "#!/bin/sh\nprintf 'original CLI error\\n' >&2\nexit 7\n",
        )
        .unwrap();
        let OpenError::Rejected {
            status,
            #[cfg(target_os = "macos")]
            stderr,
        } = launch(&cli, value.as_ref()).unwrap_err()
        else {
            panic!("expected a Zed process rejection");
        };
        assert_eq!(status.code(), Some(7));
        #[cfg(target_os = "macos")]
        {
            let stderr = stderr.unwrap();
            assert_eq!(stderr.bytes, b"original CLI error\n");
            assert!(!stderr.truncated);
        }
        let missing = root.path().join("missing-zed");
        assert!(matches!(
            launch(&missing, value.as_ref()),
            Err(OpenError::Launch { program, source })
                if program == missing && source.raw_os_error() == Some(libc::ENOENT)
        ));
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn rejection_keeps_a_bounded_stderr_prefix() {
        use std::os::unix::fs::PermissionsExt;
        let root = tempfile::tempdir().unwrap();
        let cli = root.path().join("zed");
        std::fs::write(
            &cli,
            format!(
                "#!/bin/sh\n/usr/bin/head -c {} /dev/zero >&2\nexit 19\n",
                STDERR_LIMIT + 19
            ),
        )
        .unwrap();
        std::fs::set_permissions(&cli, std::fs::Permissions::from_mode(0o700)).unwrap();
        let OpenError::Rejected { status, stderr } =
            launch(&cli, "/fixture/path".as_ref()).unwrap_err()
        else {
            panic!("expected a Zed process rejection");
        };
        let stderr = stderr.unwrap();
        assert_eq!(status.code(), Some(19));
        assert_eq!(stderr.bytes, vec![0; STDERR_LIMIT]);
        assert!(stderr.truncated);
    }
}
