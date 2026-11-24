use super::{Endpoint, Entry, WebDavError, MAX_BODY, MAX_ENTRIES};
use quick_xml::{events::Event, name::ResolveResult, NsReader};
use std::collections::HashSet;

#[derive(Default)]
struct Node {
    name: Option<String>,
    text: String,
    children: Vec<Node>,
}
impl Node {
    fn named(&self, name: &str) -> bool {
        self.name.as_deref() == Some(name)
    }
    fn child(&self, name: &str) -> Option<&Node> {
        self.children.iter().find(|node| node.named(name))
    }
    fn value(&self, name: &str) -> Option<&str> {
        self.child(name).map(|node| node.text.trim())
    }
}

fn parse_xml(body: &[u8], cancelled: &impl Fn() -> bool) -> Result<Node, WebDavError> {
    if body.len() > MAX_BODY {
        return Err(WebDavError::ResponseTooLarge);
    }
    let mut reader = NsReader::from_reader(body);
    reader.config_mut().expand_empty_elements = true;
    let mut stack: Vec<Node> = Vec::new();
    let mut root = None;
    let mut nodes = 0;
    loop {
        if cancelled() {
            return Err(WebDavError::Cancelled);
        }
        let (ns, event) = reader
            .read_resolved_event()
            .map_err(|_| WebDavError::InvalidResponse)?;
        match event {
            Event::Start(element) => {
                nodes += 1;
                if nodes > 100_000 || stack.len() >= 32 {
                    return Err(WebDavError::InvalidResponse);
                }
                let name = match ns {
                    ResolveResult::Bound(ns) if ns.as_ref() == b"DAV:" => Some(
                        String::from_utf8(element.local_name().as_ref().to_vec())
                            .map_err(|_| WebDavError::InvalidResponse)?,
                    ),
                    ResolveResult::Unknown(_) => return Err(WebDavError::InvalidResponse),
                    _ => None,
                };
                stack.push(Node {
                    name,
                    ..Node::default()
                });
            }
            Event::End(_) => {
                let node = stack.pop().ok_or(WebDavError::InvalidResponse)?;
                if let Some(parent) = stack.last_mut() {
                    parent.children.push(node);
                } else if root.replace(node).is_some() {
                    return Err(WebDavError::InvalidResponse);
                }
            }
            Event::Text(text) => {
                let text = text.decode().map_err(|_| WebDavError::InvalidResponse)?;
                if let Some(node) = stack.last_mut() {
                    node.text.push_str(&text);
                } else if !text.trim().is_empty() {
                    return Err(WebDavError::InvalidResponse);
                }
            }
            Event::CData(text) => {
                let node = stack.last_mut().ok_or(WebDavError::InvalidResponse)?;
                node.text
                    .push_str(&text.decode().map_err(|_| WebDavError::InvalidResponse)?);
            }
            Event::GeneralRef(reference) => {
                let node = stack.last_mut().ok_or(WebDavError::InvalidResponse)?;
                if let Some(ch) = reference
                    .resolve_char_ref()
                    .map_err(|_| WebDavError::InvalidResponse)?
                {
                    node.text.push(ch);
                } else {
                    let name = reference
                        .decode()
                        .map_err(|_| WebDavError::InvalidResponse)?;
                    node.text.push_str(
                        quick_xml::escape::resolve_predefined_entity(&name)
                            .ok_or(WebDavError::InvalidResponse)?,
                    );
                }
            }
            Event::DocType(_) => return Err(WebDavError::InvalidResponse),
            Event::Eof => break,
            _ => (),
        }
    }
    if !stack.is_empty() {
        return Err(WebDavError::InvalidResponse);
    }
    root.filter(|node| node.named("multistatus"))
        .ok_or(WebDavError::InvalidResponse)
}

fn successful(status: &str) -> bool {
    let mut fields = status.split_whitespace();
    fields
        .next()
        .is_some_and(|protocol| protocol.starts_with("HTTP/"))
        && fields
            .next()
            .and_then(|value| value.parse::<u16>().ok())
            .is_some_and(|status| (200..300).contains(&status))
}

pub(super) fn parse_listing(
    endpoint: &Endpoint,
    body: &[u8],
    cancelled: &impl Fn() -> bool,
) -> Result<Vec<Entry>, WebDavError> {
    let root = parse_xml(body, cancelled)?;
    let mut entries = Vec::new();
    let mut seen = HashSet::new();
    let mut self_collection = false;
    for response in root.children.iter().filter(|node| node.named("response")) {
        if cancelled() {
            return Err(WebDavError::Cancelled);
        }
        let href = response
            .value("href")
            .filter(|href| !href.is_empty())
            .ok_or(WebDavError::InvalidResponse)?;
        let (url, name) = endpoint.resolve_href(href)?;
        if response
            .value("status")
            .is_some_and(|status| !successful(status))
        {
            return Err(WebDavError::InvalidResponse);
        }
        let mut resource_type = None;
        let mut size = None;
        let mut etag = None;
        for propstat in response
            .children
            .iter()
            .filter(|node| node.named("propstat"))
        {
            if !propstat.value("status").is_some_and(successful) {
                continue;
            }
            let props = propstat.child("prop").ok_or(WebDavError::InvalidResponse)?;
            if let Some(kind) = props.child("resourcetype") {
                resource_type = Some(kind.child("collection").is_some());
            }
            if let Some(value) = props.value("getcontentlength") {
                size = Some(
                    value
                        .parse::<u64>()
                        .map_err(|_| WebDavError::InvalidResponse)?,
                );
            }
            if let Some(value) = props.value("getetag").filter(|value| !value.is_empty()) {
                etag = Some(value.into());
            }
        }
        let is_dir = resource_type.ok_or(WebDavError::InvalidResponse)?;
        let Some(name) = name else {
            if !is_dir {
                return Err(WebDavError::NotCollection);
            }
            self_collection = true;
            continue;
        };
        if !seen.insert(name.clone()) {
            return Err(WebDavError::InvalidResponse);
        }
        if entries.len() >= MAX_ENTRIES {
            return Err(WebDavError::TooManyEntries);
        }
        entries.push(Entry {
            name,
            address: url.into(),
            is_dir,
            size,
            etag,
        });
    }
    if !self_collection {
        return Err(WebDavError::NotCollection);
    }
    entries.sort_by(|a, b| {
        b.is_dir
            .cmp(&a.is_dir)
            .then_with(|| a.name.to_lowercase().cmp(&b.name.to_lowercase()))
            .then_with(|| a.name.cmp(&b.name))
    });
    Ok(entries)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn endpoint() -> Endpoint {
        Endpoint::parse("https://example.test/dav/", "test").unwrap()
    }
    fn self_node() -> &'static str {
        "<response><href>/dav/</href><propstat><prop><resourcetype><collection/></resourcetype></prop><status>HTTP/1.1 200 OK</status></propstat></response>"
    }
    fn document(child: &str) -> Vec<u8> {
        format!(
            "<multistatus xmlns=\"DAV:\">{}{child}</multistatus>",
            self_node()
        )
        .into_bytes()
    }
    fn child(href: &str, props: &str) -> String {
        format!("<response><href>{href}</href><propstat><prop>{props}</prop><status>HTTP/1.1 200 OK</status></propstat></response>")
    }

    #[test]
    fn namespace_aware_listing_decodes_url_names_and_entities() {
        let body = br#"<?xml version="1.0"?><x:multistatus xmlns:x="DAV:" xmlns:bad="urn:other">
        <x:response><x:href>/dav/</x:href><x:propstat><x:prop><x:resourcetype><x:collection/></x:resourcetype></x:prop><x:status>HTTP/1.1 200 OK</x:status></x:propstat></x:response>
        <x:response><x:href>/dav/space%20a&amp;b%27.txt</x:href><x:propstat><x:prop><x:resourcetype/><x:getcontentlength>42</x:getcontentlength><x:getetag>&quot;v1&quot;</x:getetag><bad:collection/></x:prop><x:status>HTTP/1.1 200 OK</x:status></x:propstat></x:response>
        </x:multistatus>"#;
        let entries = parse_listing(&endpoint(), body, &|| false).unwrap();
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].name, "space a&b'.txt");
        assert!(!entries[0].is_dir);
        assert_eq!(entries[0].size, Some(42));
        assert_eq!(entries[0].etag.as_deref(), Some("\"v1\""));
    }

    #[test]
    fn unsuccessful_propstats_are_not_used_and_absent_optional_props_are_allowed() {
        let body = document("<response><href>/dav/file</href><propstat><prop><resourcetype/></prop><status>HTTP/1.1 200 OK</status></propstat><propstat><prop><getcontentlength>not-a-number</getcontentlength><resourcetype><collection/></resourcetype></prop><status>HTTP/1.1 404 Not Found</status></propstat></response>");
        let entries = parse_listing(&endpoint(), &body, &|| false).unwrap();
        assert!(!entries[0].is_dir);
        assert_eq!(entries[0].size, None);
        assert_eq!(entries[0].etag, None);
    }

    #[test]
    fn listing_rejects_invalid_structure_entities_and_unsafe_children() {
        let bodies = [
            b"<multistatus xmlns='urn:wrong'/>".to_vec(),
            b"<!DOCTYPE multistatus [<!ENTITY p 'secret'>]><multistatus xmlns='DAV:'>&p;</multistatus>".to_vec(),
            b"<multistatus xmlns='DAV:'><response>".to_vec(),
            document(&child("https://other.test/dav/file", "<resourcetype/>")),
            document(&child("/dav/file", "<resourcetype/><getcontentlength>-1</getcontentlength>")),
            document(&format!("{}{}", child("/dav/file", "<resourcetype/>"), child("/dav/%66ile", "<resourcetype/>"))),
            document("<response><href>/dav/file</href><propstat><prop><resourcetype/></prop><status>HTTP/1.1 403 Forbidden</status></propstat></response>"),
            b"<multistatus xmlns='DAV:'/>".to_vec(),
        ];
        for body in bodies {
            assert!(parse_listing(&endpoint(), &body, &|| false).is_err());
        }
    }

    #[test]
    fn listing_cancellation_and_size_are_bounded() {
        assert_eq!(
            parse_listing(&endpoint(), &document(""), &|| true),
            Err(WebDavError::Cancelled)
        );
        assert_eq!(
            parse_listing(&endpoint(), &vec![b' '; MAX_BODY + 1], &|| false),
            Err(WebDavError::ResponseTooLarge)
        );
        let children = (0..=MAX_ENTRIES)
            .map(|index| child(&format!("/dav/file-{index}"), "<resourcetype/>"))
            .collect::<String>();
        assert_eq!(
            parse_listing(&endpoint(), &document(&children), &|| false),
            Err(WebDavError::TooManyEntries)
        );
        let deep = format!(
            "<multistatus xmlns='DAV:'>{}{}</multistatus>",
            "<unknown>".repeat(32),
            "</unknown>".repeat(32)
        );
        assert_eq!(
            parse_listing(&endpoint(), deep.as_bytes(), &|| false),
            Err(WebDavError::InvalidResponse)
        );
    }
}
