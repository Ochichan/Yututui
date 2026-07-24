use std::io::Cursor;

use quick_xml::NsReader;
use quick_xml::events::{BytesStart, Event};
use quick_xml::name::{Namespace, ResolveResult};

use super::WebDavError;

const MAX_XML_DEPTH: usize = 64;
const MAX_HREF_BYTES: usize = 8 * 1024;
const MAX_ETAG_BYTES: usize = 1024;
const MAX_STATUS_BYTES: usize = 128;
const MAX_LENGTH_BYTES: usize = 32;

#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct RawResource {
    pub href: String,
    pub etag: Option<String>,
    pub content_length: Option<u64>,
    pub is_collection: bool,
}

#[derive(Default)]
struct ResponseFields {
    href: String,
    status: String,
    successful_propstat: bool,
    etag: Option<String>,
    content_length: Option<u64>,
    is_collection: bool,
}

#[derive(Default)]
struct PropstatFields {
    status: String,
    etag: String,
    content_length: String,
    is_collection: bool,
}

#[derive(PartialEq, Eq)]
struct StackElement {
    local: Vec<u8>,
    dav: bool,
}

/// Parse an already body-bounded WebDAV Multi-Status document.
///
/// `quick-xml` advances one event at a time and never constructs a DOM. The caller applies the
/// separate 4 MiB response cap before entering this function; this layer additionally caps XML
/// depth, field lengths, and the number of `<response>` elements.
pub(super) fn parse_multistatus(
    bytes: &[u8],
    max_resources: usize,
) -> Result<Vec<RawResource>, WebDavError> {
    if max_resources == 0 || max_resources > super::MAX_PROPFIND_RESOURCES {
        return Err(WebDavError::ResourceLimitExceeded);
    }

    let mut reader = NsReader::from_reader(Cursor::new(bytes));
    reader.config_mut().trim_text(false);
    reader.config_mut().check_end_names = true;
    reader.config_mut().expand_empty_elements = true;

    let mut stack = Vec::<StackElement>::new();
    let mut response: Option<ResponseFields> = None;
    let mut propstat: Option<PropstatFields> = None;
    let mut resources = Vec::new();
    let mut response_count = 0_usize;
    let mut root_seen = false;
    let mut event_buffer = Vec::new();

    loop {
        let (namespace, event) = reader
            .read_resolved_event_into(&mut event_buffer)
            .map_err(|_| WebDavError::InvalidXml)?;
        let dav = matches!(
            namespace,
            ResolveResult::Bound(Namespace(namespace)) if namespace == b"DAV:"
        );
        match event {
            Event::Start(start) => {
                let local = local_name(&start)?;
                if stack.len() >= MAX_XML_DEPTH {
                    return Err(WebDavError::InvalidXml);
                }
                if stack.is_empty() {
                    if root_seen || !dav || local.as_slice() != b"multistatus" {
                        return Err(WebDavError::InvalidXml);
                    }
                    root_seen = true;
                } else if dav && local.as_slice() == b"response" {
                    if !path_is(&stack, &[b"multistatus"]) {
                        return Err(WebDavError::InvalidXml);
                    }
                    if response.is_some() {
                        return Err(WebDavError::InvalidXml);
                    }
                    response_count = response_count
                        .checked_add(1)
                        .ok_or(WebDavError::ResourceLimitExceeded)?;
                    if response_count > max_resources {
                        return Err(WebDavError::ResourceLimitExceeded);
                    }
                    response = Some(ResponseFields::default());
                } else if dav && local.as_slice() == b"propstat" {
                    if !path_is(&stack, &[b"multistatus", b"response"]) {
                        return Err(WebDavError::InvalidXml);
                    }
                    if response.is_none() || propstat.is_some() {
                        return Err(WebDavError::InvalidXml);
                    }
                    propstat = Some(PropstatFields::default());
                } else if dav
                    && local.as_slice() == b"collection"
                    && propstat.is_some()
                    && path_is(
                        &stack,
                        &[
                            b"multistatus",
                            b"response",
                            b"propstat",
                            b"prop",
                            b"resourcetype",
                        ],
                    )
                {
                    propstat.as_mut().expect("checked above").is_collection = true;
                }
                stack.push(StackElement { local, dav });
            }
            Event::End(end) => {
                let local = end.local_name();
                let Some(open) = stack.pop() else {
                    return Err(WebDavError::InvalidXml);
                };
                if open.local.as_slice() != local.as_ref() || open.dav != dav {
                    return Err(WebDavError::InvalidXml);
                }
                if open.dav && open.local.as_slice() == b"propstat" {
                    finish_propstat(
                        response.as_mut().ok_or(WebDavError::InvalidXml)?,
                        propstat.take().ok_or(WebDavError::InvalidXml)?,
                    )?;
                } else if open.dav && open.local.as_slice() == b"response" {
                    if propstat.is_some() {
                        return Err(WebDavError::InvalidXml);
                    }
                    if let Some(resource) =
                        finish_response(response.take().ok_or(WebDavError::InvalidXml)?)?
                    {
                        resources.push(resource);
                    }
                }
            }
            Event::Text(text) => {
                let decoded = text.decode().map_err(|_| WebDavError::InvalidXml)?;
                let decoded =
                    quick_xml::escape::unescape(&decoded).map_err(|_| WebDavError::InvalidXml)?;
                append_text(
                    &stack,
                    response.as_mut(),
                    propstat.as_mut(),
                    decoded.as_ref(),
                )?;
            }
            Event::GeneralRef(reference) => {
                let decoded = reference.decode().map_err(|_| WebDavError::InvalidXml)?;
                let encoded = format!("&{decoded};");
                let resolved =
                    quick_xml::escape::unescape(&encoded).map_err(|_| WebDavError::InvalidXml)?;
                append_text(
                    &stack,
                    response.as_mut(),
                    propstat.as_mut(),
                    resolved.as_ref(),
                )?;
            }
            Event::CData(_) | Event::DocType(_) => {
                // DAV href/status/property values do not need CDATA or custom declarations.
                // Rejecting them keeps alternate-representation tricks out of path handling.
                return Err(WebDavError::InvalidXml);
            }
            Event::Decl(_) | Event::Comment(_) | Event::PI(_) => {}
            Event::Eof => break,
            Event::Empty(_) => unreachable!("expand_empty_elements turns Empty into Start + End"),
        }
        event_buffer.clear();
    }

    if !root_seen || !stack.is_empty() || response.is_some() || propstat.is_some() {
        return Err(WebDavError::InvalidXml);
    }
    Ok(resources)
}

fn local_name(start: &BytesStart<'_>) -> Result<Vec<u8>, WebDavError> {
    let local = start.local_name();
    let bytes = local.as_ref();
    if bytes.is_empty()
        || bytes.len() > 64
        || !bytes
            .iter()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_'))
    {
        return Err(WebDavError::InvalidXml);
    }
    Ok(bytes.to_ascii_lowercase())
}

fn append_text(
    stack: &[StackElement],
    response: Option<&mut ResponseFields>,
    propstat: Option<&mut PropstatFields>,
    text: &str,
) -> Result<(), WebDavError> {
    let Some(current) = stack.last() else {
        return if text.trim().is_empty() {
            Ok(())
        } else {
            Err(WebDavError::InvalidXml)
        };
    };
    let Some(response) = response else {
        return if text.trim().is_empty() {
            Ok(())
        } else {
            Err(WebDavError::InvalidXml)
        };
    };
    match (current.dav, current.local.as_slice(), propstat) {
        (true, b"href", None) if path_is(stack, &[b"multistatus", b"response", b"href"]) => {
            append_bounded(&mut response.href, text, MAX_HREF_BYTES)
        }
        (true, b"status", None) if path_is(stack, &[b"multistatus", b"response", b"status"]) => {
            append_bounded(&mut response.status, text, MAX_STATUS_BYTES)
        }
        (true, b"status", Some(fields))
            if path_is(
                stack,
                &[b"multistatus", b"response", b"propstat", b"status"],
            ) =>
        {
            append_bounded(&mut fields.status, text, MAX_STATUS_BYTES)
        }
        (true, b"getetag", Some(fields))
            if path_is(
                stack,
                &[
                    b"multistatus",
                    b"response",
                    b"propstat",
                    b"prop",
                    b"getetag",
                ],
            ) =>
        {
            append_bounded(&mut fields.etag, text, MAX_ETAG_BYTES)
        }
        (true, b"getcontentlength", Some(fields))
            if path_is(
                stack,
                &[
                    b"multistatus",
                    b"response",
                    b"propstat",
                    b"prop",
                    b"getcontentlength",
                ],
            ) =>
        {
            append_bounded(&mut fields.content_length, text, MAX_LENGTH_BYTES)
        }
        _ if text.trim().is_empty() => Ok(()),
        _ => Ok(()),
    }
}

fn path_is(stack: &[StackElement], expected: &[&[u8]]) -> bool {
    stack.len() == expected.len()
        && stack
            .iter()
            .zip(expected)
            .all(|(actual, expected)| actual.dav && actual.local.as_slice() == *expected)
}

fn append_bounded(target: &mut String, text: &str, max_bytes: usize) -> Result<(), WebDavError> {
    if target.len().saturating_add(text.len()) > max_bytes {
        return Err(WebDavError::ResourceLimitExceeded);
    }
    target.push_str(text);
    Ok(())
}

fn finish_propstat(
    response: &mut ResponseFields,
    fields: PropstatFields,
) -> Result<(), WebDavError> {
    if !is_success_status(&fields.status)? {
        return Ok(());
    }
    response.successful_propstat = true;
    merge_optional_string(&mut response.etag, fields.etag)?;
    if !fields.content_length.trim().is_empty() {
        let raw = fields.content_length.trim();
        if !raw.bytes().all(|byte| byte.is_ascii_digit()) {
            return Err(WebDavError::InvalidXml);
        }
        let length = raw.parse::<u64>().map_err(|_| WebDavError::InvalidXml)?;
        match response.content_length {
            Some(existing) if existing != length => return Err(WebDavError::InvalidXml),
            Some(_) => {}
            None => response.content_length = Some(length),
        }
    }
    response.is_collection |= fields.is_collection;
    Ok(())
}

fn merge_optional_string(target: &mut Option<String>, raw: String) -> Result<(), WebDavError> {
    let value = raw.trim();
    if value.is_empty() {
        return Ok(());
    }
    match target {
        Some(existing) if existing != value => Err(WebDavError::InvalidXml),
        Some(_) => Ok(()),
        None => {
            *target = Some(value.to_owned());
            Ok(())
        }
    }
}

fn finish_response(fields: ResponseFields) -> Result<Option<RawResource>, WebDavError> {
    if fields.href.trim().is_empty() {
        return Err(WebDavError::InvalidXml);
    }
    if !fields.status.trim().is_empty() && !is_success_status(&fields.status)? {
        return Err(WebDavError::InvalidXml);
    }
    if !fields.successful_propstat && fields.status.trim().is_empty() {
        return Err(WebDavError::InvalidXml);
    }
    Ok(Some(RawResource {
        href: fields.href.trim().to_owned(),
        etag: fields.etag,
        content_length: fields.content_length,
        is_collection: fields.is_collection,
    }))
}

fn is_success_status(raw: &str) -> Result<bool, WebDavError> {
    let mut parts = raw.split_ascii_whitespace();
    let protocol = parts.next().ok_or(WebDavError::InvalidXml)?;
    let status = parts.next().ok_or(WebDavError::InvalidXml)?;
    if !protocol.eq_ignore_ascii_case("HTTP/1.1") && !protocol.eq_ignore_ascii_case("HTTP/1.0") {
        return Err(WebDavError::InvalidXml);
    }
    if status.len() != 3 || !status.bytes().all(|byte| byte.is_ascii_digit()) {
        return Err(WebDavError::InvalidXml);
    }
    let status_code = status.parse::<u16>().map_err(|_| WebDavError::InvalidXml)?;
    Ok((200..300).contains(&status_code))
}
