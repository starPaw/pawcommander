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

/// Visit each direct DAV:response as it closes, retaining at most one response tree.
fn read_responses(
    body: &[u8],
    cancelled: &impl Fn() -> bool,
    mut visit: impl FnMut(&Node) -> Result<(), WebDavError>,
) -> Result<(), WebDavError> {
    if body.len() > MAX_BODY {
        return Err(WebDavError::ResponseTooLarge);
    }
    let mut reader = NsReader::from_reader(body);
    reader.config_mut().expand_empty_elements = true;
    let mut stack: Vec<Node> = Vec::new();
    let mut root_closed = false;
    let mut result = Ok(());
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
                if stack.is_empty() && (root_closed || name.as_deref() != Some("multistatus")) {
                    return Err(WebDavError::InvalidResponse);
                }
                stack.push(Node {
                    name,
                    ..Node::default()
                });
            }
            Event::End(_) => {
                let node = stack.pop().ok_or(WebDavError::InvalidResponse)?;
                let in_response = stack.get(1).is_some_and(|child| child.named("response"));
                match stack.as_mut_slice() {
                    [] => root_closed = true,
                    [_] if node.named("response") && result.is_ok() => result = visit(&node),
                    [.., parent] if in_response && result.is_ok() => parent.children.push(node),
                    _ => (),
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
    if !root_closed || !stack.is_empty() {
        return Err(WebDavError::InvalidResponse);
    }
    // Invalid XML anywhere in the document takes precedence over a per-resource error.
    result
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

// The collection's own response establishes its type and contributes no child entry.
fn parse_response(endpoint: &Endpoint, response: &Node) -> Result<Option<Entry>, WebDavError> {
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
    match name {
        Some(name) => Ok(Some(Entry {
            name,
            address: url.into(),
            is_dir,
            size,
            etag,
        })),
        None if is_dir => Ok(None),
        None => Err(WebDavError::NotCollection),
    }
}

pub(super) fn parse_listing(
    endpoint: &Endpoint,
    body: &[u8],
    cancelled: &impl Fn() -> bool,
) -> Result<Vec<Entry>, WebDavError> {
    let mut entries = Vec::new();
    let mut seen = HashSet::new();
    let mut self_collection = false;
    read_responses(body, cancelled, |response| {
        if let Some(entry) = parse_response(endpoint, response)? {
            if !seen.insert(entry.name.clone()) {
                return Err(WebDavError::InvalidResponse);
            }
            if entries.len() >= MAX_ENTRIES {
                return Err(WebDavError::TooManyEntries);
            }
            entries.push(entry);
        } else {
            self_collection = true;
        }
        Ok(())
    })?;
    if !self_collection {
        return Err(WebDavError::NotCollection);
    }
    drop(seen);
    entries
        .sort_by_cached_key(|entry| (!entry.is_dir, entry.name.to_lowercase(), entry.name.clone()));
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
    fn only_direct_responses_contribute_entries_and_the_collection_may_come_last() {
        let file = child("/dav/hello", "<resourcetype/>");
        let body = format!(
            "<multistatus xmlns='DAV:'><extension>{file}</extension>{file}{}</multistatus>",
            self_node()
        );
        let entries = parse_listing(&endpoint(), body.as_bytes(), &|| false).unwrap();
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].name, "hello");
    }

    #[test]
    fn folders_sort_first_then_unicode_names_with_original_case_breaking_ties() {
        let mut children = String::new();
        for name in ["žena", "árka", "Árka", "folder", "A"] {
            let kind = if name == "folder" {
                "<resourcetype><collection/></resourcetype>"
            } else {
                "<resourcetype/>"
            };
            children.push_str(&child(&format!("/dav/{name}"), kind));
        }
        let entries = parse_listing(&endpoint(), &document(&children), &|| false).unwrap();
        let names: Vec<_> = entries.iter().map(|entry| entry.name.as_str()).collect();
        assert_eq!(names, ["folder", "A", "Árka", "árka", "žena"]);
    }

    #[test]
    fn invalid_document_tail_rejects_even_previously_parsed_responses() {
        let mut body = document(&child("/dav/hello", "<resourcetype/>"));
        body.extend_from_slice(b"<multistatus xmlns='DAV:'/>");
        assert_eq!(
            parse_listing(&endpoint(), &body, &|| false),
            Err(WebDavError::InvalidResponse)
        );

        let mut body = document(&child("/another/file", "<resourcetype/>"));
        assert_eq!(
            parse_listing(&endpoint(), &body, &|| false),
            Err(WebDavError::UnsafeHref)
        );
        body.extend_from_slice(b"unclosed document tail");
        assert_eq!(
            parse_listing(&endpoint(), &body, &|| false),
            Err(WebDavError::InvalidResponse)
        );
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
