use super::{Entry, WebDavError};
use std::borrow::Cow;
use url::Url;

#[derive(Clone, Debug)]
pub(crate) struct Endpoint {
    current: Url,
    segments: Vec<String>,
    pub(super) user: String,
}

impl Endpoint {
    pub(crate) fn parse(address: &str, user: &str) -> Result<Self, WebDavError> {
        let address = address.trim();
        check_href_text(address).map_err(|_| WebDavError::InvalidAddress)?;
        let mut current = Url::parse(address).map_err(|_| WebDavError::InvalidAddress)?;
        if current.scheme() != "https"
            || current.host_str().is_none()
            || !current.username().is_empty()
            || current.password().is_some()
            || current.query().is_some()
            || current.fragment().is_some()
            || current.port() == Some(0)
        {
            return Err(WebDavError::InvalidAddress);
        }
        let segments = decoded_segments(&current)
            .map(|part| part.map(Cow::into_owned))
            .collect::<Result<Vec<_>, _>>()
            .map_err(|_| WebDavError::InvalidAddress)?;
        ensure_collection_slash(&mut current);
        let user = user.trim();
        if user.is_empty() || user.contains(':') || user.chars().any(char::is_control) {
            return Err(WebDavError::InvalidUser);
        }
        Ok(Self {
            current,
            segments,
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
        let name = name.ok_or(WebDavError::UnsafeHref)?;
        ensure_collection_slash(&mut current);
        let mut segments = self.segments.clone();
        segments.push(name);
        Ok(Self {
            segments,
            current,
            user: self.user.clone(),
        })
    }

    pub(super) fn resolve_href(&self, href: &str) -> Result<(Url, Option<String>), WebDavError> {
        check_href_text(href)?;
        let url = self
            .current
            .join(href)
            .map_err(|_| WebDavError::UnsafeHref)?;
        if url.origin() != self.current.origin()
            || !url.username().is_empty()
            || url.password().is_some()
            || url.query().is_some()
            || url.fragment().is_some()
        {
            return Err(WebDavError::UnsafeHref);
        }
        let name = self.member_name(&url)?;
        Ok((url, name))
    }

    fn member_name(&self, url: &Url) -> Result<Option<String>, WebDavError> {
        // Each endpoint is either the configured collection or a verified direct child.
        // Matching its full path therefore also keeps the result inside the original root.
        let mut segments = decoded_segments(url);
        for expected in &self.segments {
            if segments.next().transpose()?.as_deref() != Some(expected.as_str()) {
                return Err(WebDavError::UnsafeHref);
            }
        }
        let name = segments.next().transpose()?.map(Cow::into_owned);
        if segments.next().is_some() {
            return Err(WebDavError::UnsafeHref);
        }
        Ok(name)
    }
}

fn ensure_collection_slash(url: &mut Url) {
    if !url.path().ends_with('/') {
        url.set_path(&format!("{}/", url.path()));
    }
}

// HTTPS paths start with '/'. Ignore its leading separator and the collection's
// optional trailing slash, but preserve empty interior segments for validation.
fn decoded_segments(url: &Url) -> impl Iterator<Item = Result<Cow<'_, str>, WebDavError>> {
    url.path().split_terminator('/').skip(1).map(decode_segment)
}

fn decode_segment(segment: &str) -> Result<Cow<'_, str>, WebDavError> {
    if segment.is_empty() {
        return Err(WebDavError::UnsafeHref);
    }
    let decoded = percent_encoding::percent_decode_str(segment)
        .decode_utf8()
        .map_err(|_| WebDavError::UnsafeHref)?;
    if matches!(decoded.as_ref(), "." | "..")
        || decoded.contains(['/', '\\'])
        || decoded.chars().any(char::is_control)
    {
        return Err(WebDavError::UnsafeHref);
    }
    Ok(decoded)
}

fn check_href_text(href: &str) -> Result<(), WebDavError> {
    if href.is_empty() {
        return Err(WebDavError::UnsafeHref);
    }
    // Check before URL joining, which would normalize away traversal segments.
    for segment in href.split('/').filter(|segment| !segment.is_empty()) {
        decode_segment(segment)?;
    }
    Ok(())
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
            "https://example.test/dav//",
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
    fn nested_collection_matches_decoded_components() {
        let root = Endpoint::parse("https://example.test/%64av", "test").unwrap();
        let folder = root
            .child(&Entry {
                name: "space name".into(),
                address: "/dav/space%20name".into(),
                is_dir: true,
                size: None,
                etag: None,
            })
            .unwrap();
        assert_eq!(folder.address(), "https://example.test/dav/space%20name/");
        assert_eq!(folder.resolve_href("/%64av/space%20name").unwrap().1, None);
        assert_eq!(
            folder.resolve_href("file").unwrap().1.as_deref(),
            Some("file")
        );
        for href in [
            "/dav/",
            "/dav/space%20names/file",
            "sub/file",
            "/dav/space%20name//",
        ] {
            assert_eq!(folder.resolve_href(href), Err(WebDavError::UnsafeHref));
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
        assert_eq!(
            child.resolve_href("/dav/user/sibling/"),
            Err(WebDavError::UnsafeHref)
        );
        assert!(child.resolve_href("../sibling/").is_err());
        let mut file = directory;
        file.is_dir = false;
        assert_eq!(
            endpoint.child(&file).unwrap_err(),
            WebDavError::NotCollection
        );
    }
}
