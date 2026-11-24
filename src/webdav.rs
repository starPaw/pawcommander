//! Read-only WebDAV browsing over verified HTTPS. Secrets remain in memory and
//! are passed to curl through stdin, never through process arguments.
mod listing;
use listing::parse_listing;
use std::{
    fmt,
    io::{Read, Write},
    process::{Command, Stdio},
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc,
    },
    thread,
    time::Duration,
};
use url::Url;
use zeroize::Zeroizing;

const MAX_BODY: usize = 4 * 1024 * 1024;
const MAX_ENTRIES: usize = 10_000;
const STATUS_MARKER: &[u8] = b"\nPAWCOMMANDER_WEBDAV_STATUS:";
const PROPFIND: &str = "<?xml version=\"1.0\" encoding=\"UTF-8\"?><d:propfind xmlns:d=\"DAV:\"><d:prop><d:resourcetype/><d:getcontentlength/><d:getetag/></d:prop></d:propfind>";

#[derive(Clone, Debug)]
pub(crate) struct Endpoint {
    root: Url,
    current: Url,
    user: String,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct Entry {
    pub(crate) name: String,
    pub(crate) address: String,
    pub(crate) is_dir: bool,
    pub(crate) size: Option<u64>,
    pub(crate) etag: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum WebDavError {
    InvalidAddress,
    InvalidUser,
    InvalidPassword,
    Cancelled,
    StartFailed,
    Io,
    RequestFailed(Option<i32>),
    HttpStatus(u16),
    ResponseTooLarge,
    InvalidResponse,
    UnsafeHref,
    TooManyEntries,
    NotCollection,
}

impl fmt::Display for WebDavError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::InvalidAddress => f.write_str(
                "Enter an HTTPS WebDAV collection address without credentials, query or fragment.",
            ),
            Self::InvalidUser => {
                f.write_str("Enter a username without a colon or control characters.")
            }
            Self::InvalidPassword => {
                f.write_str("The password contains unsupported control characters.")
            }
            Self::Cancelled => f.write_str("WebDAV browsing was cancelled."),
            Self::StartFailed => f.write_str("Could not start the WebDAV connection worker."),
            Self::Io => f.write_str("Could not communicate with the WebDAV connection worker."),
            Self::RequestFailed(Some(60)) => {
                f.write_str("The WebDAV server certificate could not be verified.")
            }
            Self::RequestFailed(Some(28)) => f.write_str("The WebDAV request timed out."),
            Self::RequestFailed(_) => f.write_str("The WebDAV request failed."),
            Self::HttpStatus(status) => write!(f, "The WebDAV server returned HTTP {status}."),
            Self::ResponseTooLarge => {
                f.write_str("The WebDAV listing exceeds the supported response size.")
            }
            Self::InvalidResponse => f.write_str("The server returned an invalid WebDAV listing."),
            Self::UnsafeHref => {
                f.write_str("The listing contains an address outside the requested collection.")
            }
            Self::TooManyEntries => f.write_str("The WebDAV listing contains too many entries."),
            Self::NotCollection => {
                f.write_str("The WebDAV address does not identify a collection.")
            }
        }
    }
}
impl std::error::Error for WebDavError {}

impl Endpoint {
    pub(crate) fn parse(address: &str, user: &str) -> Result<Self, WebDavError> {
        let address = address.trim();
        check_href_text(address).map_err(|_| WebDavError::InvalidAddress)?;
        let mut root = Url::parse(address).map_err(|_| WebDavError::InvalidAddress)?;
        if root.scheme() != "https"
            || root.host_str().is_none()
            || !root.username().is_empty()
            || root.password().is_some()
            || root.query().is_some()
            || root.fragment().is_some()
            || root.port() == Some(0)
        {
            return Err(WebDavError::InvalidAddress);
        }
        decoded_segments(&root).map_err(|_| WebDavError::InvalidAddress)?;
        if !root.path().ends_with('/') {
            let path = format!("{}/", root.path());
            root.set_path(&path);
        }
        let user = user.trim();
        if user.is_empty() || user.contains(':') || user.chars().any(char::is_control) {
            return Err(WebDavError::InvalidUser);
        }
        Ok(Self {
            current: root.clone(),
            root,
            user: user.into(),
        })
    }

    pub(crate) fn address(&self) -> &str {
        self.current.as_str()
    }
    pub(crate) fn child(&self, entry: &Entry) -> Result<Self, WebDavError> {
        if !entry.is_dir {
            return Err(WebDavError::NotCollection);
        }
        let (mut current, name) = self.resolve_href(&entry.address)?;
        if name.is_none() {
            return Err(WebDavError::UnsafeHref);
        }
        if !current.path().ends_with('/') {
            let path = format!("{}/", current.path());
            current.set_path(&path);
        }
        Ok(Self {
            root: self.root.clone(),
            current,
            user: self.user.clone(),
        })
    }

    fn resolve_href(&self, href: &str) -> Result<(Url, Option<String>), WebDavError> {
        check_href_text(href)?;
        let url = self
            .current
            .join(href)
            .map_err(|_| WebDavError::UnsafeHref)?;
        if url.origin() != self.root.origin()
            || !url.username().is_empty()
            || url.password().is_some()
            || url.query().is_some()
            || url.fragment().is_some()
        {
            return Err(WebDavError::UnsafeHref);
        }
        let root = decoded_segments(&self.root)?;
        let current = decoded_segments(&self.current)?;
        let segments = decoded_segments(&url)?;
        if !segments.starts_with(&root) || !segments.starts_with(&current) {
            return Err(WebDavError::UnsafeHref);
        }
        match segments.len().checked_sub(current.len()) {
            Some(0) => Ok((url, None)),
            Some(1) => Ok((url, segments.last().cloned())),
            _ => Err(WebDavError::UnsafeHref),
        }
    }
}

fn check_href_text(href: &str) -> Result<(), WebDavError> {
    if href.is_empty() || href.contains('\\') || href.chars().any(char::is_control) {
        return Err(WebDavError::UnsafeHref);
    }
    // Inspect before URL joining, which would normalize away traversal segments.
    for segment in href.split('/') {
        let decoded = percent_encoding::percent_decode_str(segment)
            .decode_utf8()
            .map_err(|_| WebDavError::UnsafeHref)?;
        if matches!(decoded.as_ref(), "." | "..")
            || decoded.contains(['/', '\\'])
            || decoded.chars().any(char::is_control)
        {
            return Err(WebDavError::UnsafeHref);
        }
    }
    Ok(())
}

fn decoded_segments(url: &Url) -> Result<Vec<String>, WebDavError> {
    let mut parts: Vec<_> = url
        .path_segments()
        .ok_or(WebDavError::UnsafeHref)?
        .collect();
    if parts.last() == Some(&"") {
        parts.pop();
    }
    parts
        .into_iter()
        .map(|part| {
            let decoded = percent_encoding::percent_decode_str(part)
                .decode_utf8()
                .map_err(|_| WebDavError::UnsafeHref)?;
            if decoded.is_empty()
                || matches!(decoded.as_ref(), "." | "..")
                || decoded.contains(['/', '\\'])
                || decoded.chars().any(char::is_control)
            {
                return Err(WebDavError::UnsafeHref);
            }
            Ok(decoded.into_owned())
        })
        .collect()
}

/// Called on a worker. Closing the browser cancels and reaps the request process.
pub(crate) fn browse(
    endpoint: &Endpoint,
    password: &str,
    cancelled: &impl Fn() -> bool,
) -> Result<Vec<Entry>, WebDavError> {
    if cancelled() {
        return Err(WebDavError::Cancelled);
    }
    if password.chars().any(char::is_control) {
        return Err(WebDavError::InvalidPassword);
    }
    let mut config = Zeroizing::new(String::from("user = \""));
    for ch in endpoint
        .user
        .chars()
        .chain(std::iter::once(':'))
        .chain(password.chars())
    {
        if matches!(ch, '\\' | '"') {
            config.push('\\');
        }
        config.push(ch);
    }
    config.push_str("\"\n");
    let mut command = Command::new("/usr/bin/curl");
    command
        .args([
            "--disable",
            "--globoff",
            "--config",
            "-",
            "--silent",
            "--show-error",
            "--proto",
            "=https",
            "--proto-redir",
            "=https",
            "--max-redirs",
            "0",
            "--connect-timeout",
            "10",
            "--max-time",
            "30",
            "--max-filesize",
            "4194304",
            "--request",
            "PROPFIND",
            "--header",
            "Depth: 1",
            "--header",
            "Content-Type: application/xml; charset=utf-8",
            "--data-binary",
            PROPFIND,
            "--write-out",
            "\nPAWCOMMANDER_WEBDAV_STATUS:%{http_code}",
            "--url",
            endpoint.address(),
        ])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null());
    let mut child = command.spawn().map_err(|_| WebDavError::StartFailed)?;
    let write_result = child
        .stdin
        .take()
        .ok_or(WebDavError::Io)
        .and_then(|mut input| {
            input
                .write_all(config.as_bytes())
                .map_err(|_| WebDavError::Io)
        });
    if let Err(error) = write_result {
        let _ = child.kill();
        let _ = child.wait();
        return Err(error);
    }
    let output = match child.stdout.take() {
        Some(output) => output,
        None => {
            let _ = child.kill();
            let _ = child.wait();
            return Err(WebDavError::Io);
        }
    };
    let too_large = Arc::new(AtomicBool::new(false));
    let flag = too_large.clone();
    let reader = thread::Builder::new()
        .name("webdav-response".into())
        .spawn(move || read_bounded(output, MAX_BODY + STATUS_MARKER.len() + 3, &flag));
    let reader = match reader {
        Ok(reader) => reader,
        Err(_) => {
            let _ = child.kill();
            let _ = child.wait();
            return Err(WebDavError::StartFailed);
        }
    };
    let outcome = loop {
        if cancelled() {
            break Err(WebDavError::Cancelled);
        }
        if too_large.load(Ordering::Acquire) {
            break Err(WebDavError::ResponseTooLarge);
        }
        match child.try_wait() {
            Ok(Some(status)) => break Ok(status),
            Ok(None) => thread::sleep(Duration::from_millis(10)),
            Err(_) => break Err(WebDavError::Io),
        }
    };
    if outcome.is_err() {
        let _ = child.kill();
    }
    let _ = child.wait();
    let body = reader.join().map_err(|_| WebDavError::Io)?;
    let status = outcome?;
    let mut body = body?;
    if !status.success() {
        if status.code() == Some(63) {
            return Err(WebDavError::ResponseTooLarge);
        }
        return Err(WebDavError::RequestFailed(status.code()));
    }
    let suffix = STATUS_MARKER.len() + 3;
    if body.len() < suffix {
        return Err(WebDavError::InvalidResponse);
    }
    let start = body.len() - suffix;
    if &body[start..start + STATUS_MARKER.len()] != STATUS_MARKER {
        return Err(WebDavError::InvalidResponse);
    }
    let status = std::str::from_utf8(&body[start + STATUS_MARKER.len()..])
        .ok()
        .and_then(|code| code.parse().ok())
        .ok_or(WebDavError::InvalidResponse)?;
    body.truncate(start);
    if status != 207 {
        return Err(WebDavError::HttpStatus(status));
    }
    if cancelled() {
        return Err(WebDavError::Cancelled);
    }
    parse_listing(endpoint, &body, cancelled)
}

fn read_bounded(
    mut input: impl Read,
    limit: usize,
    exceeded: &AtomicBool,
) -> Result<Vec<u8>, WebDavError> {
    let mut body = Vec::new();
    let mut chunk = [0_u8; 8192];
    loop {
        let count = input.read(&mut chunk).map_err(|_| WebDavError::Io)?;
        if count == 0 {
            return Ok(body);
        }
        if count > limit.saturating_sub(body.len()) {
            exceeded.store(true, Ordering::Release);
            return Err(WebDavError::ResponseTooLarge);
        }
        body.extend_from_slice(&chunk[..count]);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn endpoint() -> Endpoint {
        Endpoint::parse("https://example.test/dav/user/", "test").unwrap()
    }

    #[test]
    fn endpoint_requires_https_and_separate_credentials() {
        for address in [
            "http://example.test/dav/",
            "https://user:pass@example.test/dav/",
            "https://example.test/dav/?token=secret",
            "https://example.test/dav/#x",
            "https://example.test/dav/%2e%2e/",
            "https://example.test/dav/%2f/",
        ] {
            assert_eq!(
                Endpoint::parse(address, "user").unwrap_err(),
                WebDavError::InvalidAddress
            );
        }
        assert!(Endpoint::parse("https://example.test/", "user").is_ok());
        assert!(Endpoint::parse("https://example.test/dav", "user")
            .unwrap()
            .address()
            .ends_with('/'));
        assert_eq!(
            Endpoint::parse("https://example.test/dav/", "u:ser").unwrap_err(),
            WebDavError::InvalidUser
        );
    }

    #[test]
    fn href_must_be_immediate_child_on_same_origin_and_under_root() {
        let endpoint = endpoint();
        for href in [
            "https://other.test/dav/user/file",
            "http://example.test/dav/user/file",
            "//other.test/dav/user/file",
            "/dav/other/file",
            "../file",
            "%2e%2e/file",
            "sub/file",
            "file?token=secret",
            "file#fragment",
            "a%2fb",
            "a%5cb",
            "https://user:pass@example.test/dav/user/file",
            "a\\b",
            "a%00b",
        ] {
            assert!(endpoint.resolve_href(href).is_err(), "{href}");
        }
        for (href, name) in [
            ("/dav/user/space%20name.txt", "space name.txt"),
            ("https://example.test/dav/user/a%26b%27.txt", "a&b'.txt"),
            ("%5B2026%5D/", "[2026]"),
        ] {
            assert_eq!(
                endpoint.resolve_href(href).unwrap().1.as_deref(),
                Some(name)
            );
        }
    }

    #[test]
    fn child_preserves_root_and_rejects_file_navigation() {
        let endpoint = endpoint();
        let directory = Entry {
            name: "sub".into(),
            address: "https://example.test/dav/user/sub/".into(),
            is_dir: true,
            size: None,
            etag: None,
        };
        let child = endpoint.child(&directory).unwrap();
        assert_eq!(child.address(), directory.address);
        assert_eq!(child.root, endpoint.root);
        assert!(child.resolve_href("../sibling/").is_err());
        let mut file = directory;
        file.is_dir = false;
        assert_eq!(
            endpoint.child(&file).unwrap_err(),
            WebDavError::NotCollection
        );
    }

    #[test]
    fn bounded_response_refuses_extra_bytes() {
        let exceeded = AtomicBool::new(false);
        assert_eq!(
            read_bounded(&b"12345"[..], 4, &exceeded),
            Err(WebDavError::ResponseTooLarge)
        );
        assert!(exceeded.load(Ordering::Acquire));
        assert_eq!(
            read_bounded(&b"1234"[..], 4, &AtomicBool::new(false)).unwrap(),
            b"1234"
        );
    }

    #[test]
    fn cancellation_and_invalid_secret_do_not_start_network() {
        assert_eq!(
            browse(&endpoint(), "pw", &|| true),
            Err(WebDavError::Cancelled)
        );
        assert_eq!(
            browse(&endpoint(), "pw\n", &|| false),
            Err(WebDavError::InvalidPassword)
        );
    }

    /// Disposable HTTPS fixture only. Credentials are never written to test output.
    #[test]
    #[ignore = "requires disposable HTTPS WebDAV fixture"]
    fn disposable_https_browse() {
        let url = std::env::var("PAWCOMMANDER_TEST_WEBDAV_URL").expect("fixture URL");
        let user = std::env::var("PAWCOMMANDER_TEST_WEBDAV_USER").expect("fixture user");
        let password = Zeroizing::new(
            std::env::var("PAWCOMMANDER_TEST_WEBDAV_PASSWORD").expect("fixture password"),
        );
        let endpoint = Endpoint::parse(&url, &user).unwrap();
        let entries = browse(&endpoint, &password, &|| false).unwrap();
        assert!(entries
            .iter()
            .any(|entry| entry.name == "hello.txt" && !entry.is_dir));
        assert!(entries.iter().any(|entry| entry.name == "space name.txt"));
        let sub = entries
            .iter()
            .find(|entry| entry.name == "subfolder" && entry.is_dir)
            .unwrap();
        let children = browse(&endpoint.child(sub).unwrap(), &password, &|| false).unwrap();
        assert!(children.iter().any(|entry| entry.name == "nested.txt"));
        assert_eq!(
            browse(&endpoint, "incorrect-disposable-password", &|| false),
            Err(WebDavError::HttpStatus(401))
        );
    }
}
