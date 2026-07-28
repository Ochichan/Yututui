use std::sync::Arc;

use age::secrecy::SecretString;
use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::Mutex;

use super::*;
use crate::sync::{DeviceSecretMaterial, encrypt_json_to_recipients};

fn credential() -> VaultCredential {
    VaultCredential::password("sync-user", SecretString::from("sync-password")).unwrap()
}

fn encrypted_object() -> EncryptedObject {
    let device = DeviceSecretMaterial::generate_for("webdav-test-device").unwrap();
    let recipients = vec![device.public_identity().age_recipient];
    encrypt_json_to_recipients(&serde_json::json!({"protected": true}), &recipients).unwrap()
}

async fn read_request(stream: &mut TcpStream) -> Vec<u8> {
    let mut bytes = Vec::new();
    let mut buffer = [0_u8; 4096];
    let header_end = loop {
        let read = stream.read(&mut buffer).await.unwrap();
        assert!(read > 0, "request ended before its headers");
        bytes.extend_from_slice(&buffer[..read]);
        assert!(bytes.len() <= 2 * 1024 * 1024, "test request exceeded cap");
        if let Some(position) = bytes.windows(4).position(|part| part == b"\r\n\r\n") {
            break position + 4;
        }
    };
    let headers = String::from_utf8_lossy(&bytes[..header_end]);
    let content_length = headers
        .lines()
        .find_map(|line| {
            let (name, value) = line.split_once(':')?;
            name.eq_ignore_ascii_case("content-length")
                .then(|| value.trim().parse::<usize>().unwrap())
        })
        .unwrap_or(0);
    while bytes.len() < header_end + content_length {
        let read = stream.read(&mut buffer).await.unwrap();
        assert!(read > 0, "request ended before its body");
        bytes.extend_from_slice(&buffer[..read]);
    }
    bytes
}

async fn respond(stream: &mut TcpStream, head: &str, body: &[u8]) {
    stream.write_all(head.as_bytes()).await.unwrap();
    stream.write_all(body).await.unwrap();
    stream.shutdown().await.unwrap();
}

async fn respond_capability_propfind(listener: &TcpListener) {
    let (mut stream, _) = listener.accept().await.unwrap();
    let request = read_request(&mut stream).await;
    let request = String::from_utf8_lossy(&request).to_ascii_lowercase();
    assert!(request.starts_with("propfind /vault/yututui/v2/capability http/1.1\r\n"));
    assert!(request.contains("\r\ndepth: 1\r\n"));
    let body = br#"<d:multistatus xmlns:d="DAV:"><d:response><d:href>/vault/yututui/v2/capability/</d:href><d:propstat><d:prop><d:resourcetype><d:collection/></d:resourcetype></d:prop><d:status>HTTP/1.1 200 OK</d:status></d:propstat></d:response></d:multistatus>"#;
    respond(
        &mut stream,
        &format!(
            "HTTP/1.1 207 Multi-Status\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
            body.len()
        ),
        body,
    )
    .await;
}

fn endpoint(listener: &TcpListener, path: &str) -> String {
    format!("http://{}{path}", listener.local_addr().unwrap())
}

fn request_body(request: &[u8]) -> &[u8] {
    let header_end = request
        .windows(4)
        .position(|part| part == b"\r\n\r\n")
        .expect("request has header terminator")
        + 4;
    &request[header_end..]
}

#[test]
fn endpoint_and_entity_tags_are_strict() {
    assert!(matches!(
        WebDavClient::new("ftp://example.test/vault"),
        Err(WebDavError::UnsupportedScheme)
    ));
    assert!(matches!(
        WebDavClient::new("https://user:secret@example.test/vault"),
        Err(WebDavError::EndpointCredentials)
    ));
    assert!(matches!(
        WebDavClient::new("https://example.test/vault?token=secret"),
        Err(WebDavError::InvalidEndpoint)
    ));
    assert!(matches!(
        WebDavClient::new("http://example.test/vault"),
        Err(WebDavError::UnsupportedScheme)
    ));
    assert!(WebDavClient::new("http://127.0.0.1:8080/vault").is_ok());
    assert!(WebDavClient::new("http://[::1]:8080/vault").is_ok());
    assert!(matches!(
        WebDavClient::new("http://localhost:8080/vault"),
        Err(WebDavError::UnsupportedScheme)
    ));

    let strong = EntityTag::parse("\"revision-1\"").unwrap();
    assert!(!strong.is_weak());
    assert_eq!(strong.as_str(), "\"revision-1\"");
    assert!(EntityTag::parse("W/\"revision-1\"").unwrap().is_weak());
    assert_eq!(
        EntityTag::parse("revision-1").unwrap_err(),
        WebDavError::InvalidEntityTag
    );
    assert_eq!(
        EntityTag::parse("\"bad\u{7f}\"").unwrap_err(),
        WebDavError::InvalidEntityTag
    );
}

#[test]
fn recursive_list_request_and_depth_budgets_fail_closed() {
    assert_eq!(
        next_list_request(MAX_LIST_PROPFIND_REQUESTS, MAX_LIST_PROPFIND_REQUESTS),
        Err(WebDavError::ResourceLimitExceeded)
    );
    assert_eq!(
        next_list_depth(MAX_LIST_DEPTH),
        Err(WebDavError::ResourceLimitExceeded)
    );
}

#[test]
fn transport_preserves_redacted_remote_failure_categories() {
    assert_eq!(
        map_vault_error(WebDavError::AuthenticationRequired, TransportOperation::Get),
        VaultError::RemoteAuthentication
    );
    assert_eq!(
        map_vault_error(WebDavError::CertificateFailed, TransportOperation::Get),
        VaultError::RemoteCertificate
    );
    assert_eq!(
        map_vault_error(WebDavError::RequestFailed, TransportOperation::Get),
        VaultError::RemoteUnavailable
    );
    assert_eq!(
        map_vault_error(WebDavError::MethodUnsupported, TransportOperation::Put),
        VaultError::RemoteUnsupported
    );
}

#[test]
fn multistatus_parser_merges_successful_properties_and_decodes_entities() {
    let body = br#"<?xml version="1.0"?>
<d:multistatus xmlns:d="DAV:">
  <d:response>
    <d:href>/vault/devices/device-one/head.age</d:href>
    <d:propstat>
      <d:prop>
        <d:getetag>&quot;head-7&quot;</d:getetag>
        <d:getcontentlength>321</d:getcontentlength>
        <d:resourcetype/>
      </d:prop>
      <d:status>HTTP/1.1 200 OK</d:status>
    </d:propstat>
    <d:propstat>
      <d:prop><d:getetag>&quot;ignored&quot;</d:getetag></d:prop>
      <d:status>HTTP/1.1 404 Not Found</d:status>
    </d:propstat>
  </d:response>
</d:multistatus>"#;

    let resources = xml::parse_multistatus(body, 10).unwrap();

    assert_eq!(
        resources,
        vec![xml::RawResource {
            href: "/vault/devices/device-one/head.age".to_owned(),
            etag: Some("\"head-7\"".to_owned()),
            content_length: Some(321),
            is_collection: false,
        }]
    );
}

#[test]
fn multistatus_parser_enforces_count_depth_and_dtd_limits() {
    let two_resources = br#"<d:multistatus xmlns:d="DAV:">
<d:response><d:href>/vault/a</d:href><d:status>HTTP/1.1 200 OK</d:status></d:response>
<d:response><d:href>/vault/b</d:href><d:status>HTTP/1.1 200 OK</d:status></d:response>
</d:multistatus>"#;
    assert_eq!(
        xml::parse_multistatus(two_resources, 1).unwrap_err(),
        WebDavError::ResourceLimitExceeded
    );

    let doctype = br#"<!DOCTYPE x [<!ENTITY y "z">]>
<multistatus><response><href>&y;</href><status>HTTP/1.1 200 OK</status></response></multistatus>"#;
    assert_eq!(
        xml::parse_multistatus(doctype, 10).unwrap_err(),
        WebDavError::InvalidXml
    );

    let mut deep = String::new();
    for _ in 0..65 {
        deep.push_str("<x>");
    }
    for _ in 0..65 {
        deep.push_str("</x>");
    }
    assert_eq!(
        xml::parse_multistatus(deep.as_bytes(), 10).unwrap_err(),
        WebDavError::InvalidXml
    );

    let wrong_namespace = br#"<x:multistatus xmlns:x="urn:not-dav">
<x:response><x:href>/vault/manifest</x:href><x:status>HTTP/1.1 200 OK</x:status></x:response>
</x:multistatus>"#;
    assert_eq!(
        xml::parse_multistatus(wrong_namespace, 10).unwrap_err(),
        WebDavError::InvalidXml
    );
}

#[tokio::test]
#[cfg_attr(
    windows,
    ignore = "GitHub Windows loopback can abort or stall this raw-socket fixture"
)]
async fn options_detects_required_methods_without_following_automatically() {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = endpoint(&listener, "/vault");
    let captured = Arc::new(Mutex::new(Vec::new()));
    let server_capture = Arc::clone(&captured);
    let server = tokio::spawn(async move {
        let (mut stream, _) = listener.accept().await.unwrap();
        let request = read_request(&mut stream).await;
        *server_capture.lock().await = request;
        respond(
            &mut stream,
            "HTTP/1.1 200 OK\r\nDAV: 1, 2\r\nAllow: OPTIONS, PROPFIND, MKCOL, GET, PUT\r\nContent-Length: 0\r\nConnection: close\r\n\r\n",
            &[],
        )
        .await;
    });

    let capabilities = WebDavClient::new(&url)
        .unwrap()
        .options(&credential())
        .await
        .unwrap();

    assert!(capabilities.supports_encrypted_sync());
    assert!(capabilities.dav_class_2);
    let request = String::from_utf8_lossy(&captured.lock().await).to_ascii_lowercase();
    assert!(request.starts_with("options /vault/ http/1.1\r\n"));
    assert!(request.contains("authorization: basic "));
    server.await.unwrap();
}

#[tokio::test]
#[cfg_attr(
    windows,
    ignore = "GitHub Windows loopback can abort or stall this raw-socket fixture"
)]
async fn authenticated_redirects_must_stay_on_the_exact_origin() {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let server = tokio::spawn(async move {
        let (mut stream, _) = listener.accept().await.unwrap();
        read_request(&mut stream).await;
        respond(
            &mut stream,
            "HTTP/1.1 302 Found\r\nLocation: http://127.0.0.1:9/stolen\r\nContent-Length: 0\r\nConnection: close\r\n\r\n",
            &[],
        )
        .await;
    });
    let error = WebDavClient::new(&format!("http://{address}/vault"))
        .unwrap()
        .options(&credential())
        .await
        .unwrap_err();
    assert_eq!(error, WebDavError::CrossOriginRedirect);
    server.await.unwrap();

    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let captured = Arc::new(Mutex::new(Vec::new()));
    let server_capture = Arc::clone(&captured);
    let server = tokio::spawn(async move {
        let (mut first, _) = listener.accept().await.unwrap();
        read_request(&mut first).await;
        respond(
            &mut first,
            "HTTP/1.1 307 Temporary Redirect\r\nLocation: /vault/canonical/\r\nContent-Length: 0\r\nConnection: close\r\n\r\n",
            &[],
        )
        .await;
        let (mut second, _) = listener.accept().await.unwrap();
        let request = read_request(&mut second).await;
        *server_capture.lock().await = request;
        respond(
            &mut second,
            "HTTP/1.1 200 OK\r\nDAV: 1\r\nAllow: OPTIONS, PROPFIND, MKCOL, GET, PUT\r\nContent-Length: 0\r\nConnection: close\r\n\r\n",
            &[],
        )
        .await;
    });
    let capabilities = WebDavClient::new(&format!("http://{address}/vault"))
        .unwrap()
        .options(&credential())
        .await
        .unwrap();
    assert!(capabilities.supports_encrypted_sync());
    let request = String::from_utf8_lossy(&captured.lock().await).to_ascii_lowercase();
    assert!(request.starts_with("options /vault/canonical/ http/1.1\r\n"));
    assert!(request.contains("authorization: basic "));
    server.await.unwrap();
}

#[tokio::test]
#[cfg_attr(
    windows,
    ignore = "GitHub Windows loopback can abort or stall this raw-socket fixture"
)]
async fn propfind_is_body_and_resource_bounded() {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let server = tokio::spawn(async move {
        let (mut stream, _) = listener.accept().await.unwrap();
        let request = read_request(&mut stream).await;
        let request = String::from_utf8_lossy(&request).to_ascii_lowercase();
        assert!(request.starts_with("propfind /vault/devices http/1.1\r\n"));
        assert!(request.contains("\r\ndepth: 1\r\n"));
        let body = br#"<?xml version="1.0"?>
<d:multistatus xmlns:d="DAV:">
 <d:response><d:href>/vault/devices/</d:href><d:propstat><d:prop><d:resourcetype><d:collection/></d:resourcetype></d:prop><d:status>HTTP/1.1 200 OK</d:status></d:propstat></d:response>
 <d:response><d:href>/vault/devices/device-one</d:href><d:propstat><d:prop><d:getetag>&quot;device-one&quot;</d:getetag><d:getcontentlength>99</d:getcontentlength></d:prop><d:status>HTTP/1.1 200 OK</d:status></d:propstat></d:response>
</d:multistatus>"#;
        respond(
            &mut stream,
            &format!(
                "HTTP/1.1 207 Multi-Status\r\nContent-Type: application/xml\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                body.len()
            ),
            body,
        )
        .await;
    });
    let key = ObjectKey::new("devices").unwrap();
    let resources = WebDavClient::new(&format!("http://{address}/vault"))
        .unwrap()
        .propfind(&key, PropfindDepth::One, &credential(), 10)
        .await
        .unwrap();
    assert_eq!(resources.len(), 2);
    assert_eq!(resources[0].key.as_ref().unwrap().as_str(), "devices");
    assert!(resources[0].is_collection);
    assert_eq!(
        resources[1].key.as_ref().unwrap().as_str(),
        "devices/device-one"
    );
    assert_eq!(
        resources[1].etag.as_ref().unwrap().as_str(),
        "\"device-one\""
    );
    server.await.unwrap();

    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let server = tokio::spawn(async move {
        let (mut stream, _) = listener.accept().await.unwrap();
        read_request(&mut stream).await;
        respond(
            &mut stream,
            &format!(
                "HTTP/1.1 207 Multi-Status\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                MAX_PROPFIND_BYTES + 1
            ),
            &[],
        )
        .await;
    });
    let error = WebDavClient::new(&format!("http://{address}/vault"))
        .unwrap()
        .propfind(&key, PropfindDepth::One, &credential(), 10)
        .await
        .unwrap_err();
    assert_eq!(error, WebDavError::ResponseTooLarge);
    server.await.unwrap();
}

#[tokio::test]
#[cfg_attr(
    windows,
    ignore = "GitHub Windows loopback can abort or stall this raw-socket fixture"
)]
async fn conditional_put_maps_412_without_retrying() {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let server = tokio::spawn(async move {
        let (mut stream, _) = listener.accept().await.unwrap();
        let request = read_request(&mut stream).await;
        let request = String::from_utf8_lossy(&request).to_ascii_lowercase();
        assert!(request.starts_with("put /vault/manifest http/1.1\r\n"));
        assert!(request.contains("\r\nif-none-match: *\r\n"));
        respond(
            &mut stream,
            "HTTP/1.1 412 Precondition Failed\r\nContent-Length: 0\r\nConnection: close\r\n\r\n",
            &[],
        )
        .await;
    });
    let key = ObjectKey::new("manifest").unwrap();
    let error = WebDavClient::new(&format!("http://{address}/vault"))
        .unwrap()
        .put(
            &key,
            &encrypted_object(),
            ObjectCondition::CreateOnly,
            &credential(),
        )
        .await
        .unwrap_err();
    assert_eq!(error, WebDavError::PreconditionFailed);
    server.await.unwrap();
}

#[tokio::test]
#[cfg_attr(
    windows,
    ignore = "GitHub Windows loopback can abort or stall this raw-socket fixture"
)]
async fn conditional_put_rejects_statuses_that_violate_the_requested_precondition() {
    for (condition, status_line) in [
        (ObjectCondition::CreateOnly, "HTTP/1.1 204 No Content"),
        (
            ObjectCondition::Match("\"before\"".to_owned()),
            "HTTP/1.1 201 Created",
        ),
    ] {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let server = tokio::spawn(async move {
            let (mut stream, _) = listener.accept().await.unwrap();
            read_request(&mut stream).await;
            respond(
                &mut stream,
                &format!("{status_line}\r\nETag: \"after\"\r\nContent-Length: 0\r\nConnection: close\r\n\r\n"),
                &[],
            )
            .await;
        });
        let error = WebDavClient::new(&format!("http://{address}/vault"))
            .unwrap()
            .put(
                &ObjectKey::new("manifest").unwrap(),
                &encrypted_object(),
                condition,
                &credential(),
            )
            .await
            .unwrap_err();
        assert_eq!(error, WebDavError::MethodUnsupported);
        server.await.unwrap();
    }
}

#[tokio::test]
#[cfg_attr(
    windows,
    ignore = "GitHub Windows loopback can abort or stall this raw-socket fixture"
)]
async fn setup_and_fresh_pair_join_probe_conditional_writes_before_enabling_sync() {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = endpoint(&listener, "/vault");
    let server = tokio::spawn(async move {
        let (mut options, _) = listener.accept().await.unwrap();
        let request = read_request(&mut options).await;
        assert!(String::from_utf8_lossy(&request).starts_with("OPTIONS /vault/ HTTP/1.1"));
        respond(
            &mut options,
            "HTTP/1.1 200 OK\r\nDAV: 1\r\nAllow: OPTIONS, PROPFIND, MKCOL, GET, PUT\r\nContent-Length: 0\r\nConnection: close\r\n\r\n",
            &[],
        )
        .await;

        for expected in [
            "/vault/yututui",
            "/vault/yututui/v2",
            "/vault/yututui/v2/capability",
        ] {
            let (mut stream, _) = listener.accept().await.unwrap();
            let request = read_request(&mut stream).await;
            assert!(
                String::from_utf8_lossy(&request)
                    .starts_with(&format!("MKCOL {expected} HTTP/1.1\r\n"))
            );
            respond(
                &mut stream,
                "HTTP/1.1 201 Created\r\nContent-Length: 0\r\nConnection: close\r\n\r\n",
                &[],
            )
            .await;
        }

        let marker_path = "/vault/yututui/v2/capability/conditional-put-v1.age";
        let (mut missing, _) = listener.accept().await.unwrap();
        let request = read_request(&mut missing).await;
        assert!(
            String::from_utf8_lossy(&request)
                .starts_with(&format!("GET {marker_path} HTTP/1.1\r\n"))
        );
        respond(
            &mut missing,
            "HTTP/1.1 404 Not Found\r\nContent-Length: 0\r\nConnection: close\r\n\r\n",
            &[],
        )
        .await;

        let (mut create, _) = listener.accept().await.unwrap();
        let request = read_request(&mut create).await;
        let request_text = String::from_utf8_lossy(&request).to_ascii_lowercase();
        assert!(request_text.starts_with(&format!("put {marker_path} http/1.1\r\n")));
        assert!(request_text.contains("\r\nif-none-match: *\r\n"));
        let marker = request_body(&request).to_vec();
        respond(
            &mut create,
            "HTTP/1.1 201 Created\r\nETag: \"probe-1\"\r\nContent-Length: 0\r\nConnection: close\r\n\r\n",
            &[],
        )
        .await;

        let (mut repeat, _) = listener.accept().await.unwrap();
        let request = read_request(&mut repeat).await;
        assert!(
            String::from_utf8_lossy(&request)
                .to_ascii_lowercase()
                .contains("\r\nif-none-match: *\r\n")
        );
        assert_eq!(request_body(&request), marker);
        respond(
            &mut repeat,
            "HTTP/1.1 412 Precondition Failed\r\nContent-Length: 0\r\nConnection: close\r\n\r\n",
            &[],
        )
        .await;

        let (mut mismatch, _) = listener.accept().await.unwrap();
        let request = read_request(&mut mismatch).await;
        let request_text = String::from_utf8_lossy(&request).to_ascii_lowercase();
        assert!(request_text.contains("\r\nif-match: \"yututui-probe-"));
        assert_eq!(request_body(&request), marker);
        respond(
            &mut mismatch,
            "HTTP/1.1 412 Precondition Failed\r\nContent-Length: 0\r\nConnection: close\r\n\r\n",
            &[],
        )
        .await;

        let (mut readback, _) = listener.accept().await.unwrap();
        read_request(&mut readback).await;
        respond(
            &mut readback,
            &format!(
                "HTTP/1.1 200 OK\r\nETag: \"probe-1\"\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                marker.len()
            ),
            &marker,
        )
        .await;

        let (mut matched, _) = listener.accept().await.unwrap();
        let request = read_request(&mut matched).await;
        assert!(
            String::from_utf8_lossy(&request)
                .to_ascii_lowercase()
                .contains("\r\nif-match: \"probe-1\"\r\n")
        );
        assert_eq!(request_body(&request), marker);
        respond(
            &mut matched,
            "HTTP/1.1 204 No Content\r\nETag: \"probe-2\"\r\nContent-Length: 0\r\nConnection: close\r\n\r\n",
            &[],
        )
        .await;

        let (mut final_readback, _) = listener.accept().await.unwrap();
        read_request(&mut final_readback).await;
        respond(
            &mut final_readback,
            &format!(
                "HTTP/1.1 200 OK\r\nETag: \"probe-2\"\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                marker.len()
            ),
            &marker,
        )
        .await;

        respond_capability_propfind(&listener).await;
    });

    let capabilities = tokio::task::spawn_blocking(move || {
        let credential = credential();
        BlockingWebDavTransport::new(&url, &credential)
            .unwrap()
            .probe_capabilities()
            .unwrap()
    })
    .await
    .unwrap();
    assert!(capabilities.supports_encrypted_sync());
    server.await.unwrap();
}

#[tokio::test]
#[cfg_attr(
    windows,
    ignore = "GitHub Windows loopback can abort or stall this raw-socket fixture"
)]
async fn preexisting_capability_marker_still_requires_a_valid_matched_write() {
    let existing_marker = encrypted_object().as_bytes().to_vec();
    let server_marker = existing_marker.clone();
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = endpoint(&listener, "/vault");
    let server = tokio::spawn(async move {
        let (mut options, _) = listener.accept().await.unwrap();
        read_request(&mut options).await;
        respond(
            &mut options,
            "HTTP/1.1 200 OK\r\nDAV: 1\r\nAllow: OPTIONS, PROPFIND, MKCOL, GET, PUT\r\nContent-Length: 0\r\nConnection: close\r\n\r\n",
            &[],
        )
        .await;

        for _ in 0..3 {
            let (mut collection, _) = listener.accept().await.unwrap();
            read_request(&mut collection).await;
            respond(
                &mut collection,
                "HTTP/1.1 405 Method Not Allowed\r\nContent-Length: 0\r\nConnection: close\r\n\r\n",
                &[],
            )
            .await;
        }

        let (mut existing, _) = listener.accept().await.unwrap();
        read_request(&mut existing).await;
        respond(
            &mut existing,
            &format!(
                "HTTP/1.1 200 OK\r\nETag: \"probe-existing\"\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                server_marker.len()
            ),
            &server_marker,
        )
        .await;

        let (mut create, _) = listener.accept().await.unwrap();
        let request = read_request(&mut create).await;
        assert!(
            String::from_utf8_lossy(&request)
                .to_ascii_lowercase()
                .contains("\r\nif-none-match: *\r\n")
        );
        let replacement = request_body(&request).to_vec();
        respond(
            &mut create,
            "HTTP/1.1 412 Precondition Failed\r\nContent-Length: 0\r\nConnection: close\r\n\r\n",
            &[],
        )
        .await;

        let (mut mismatch, _) = listener.accept().await.unwrap();
        let request = read_request(&mut mismatch).await;
        assert!(
            String::from_utf8_lossy(&request)
                .to_ascii_lowercase()
                .contains("\r\nif-match: \"yututui-probe-")
        );
        respond(
            &mut mismatch,
            "HTTP/1.1 412 Precondition Failed\r\nContent-Length: 0\r\nConnection: close\r\n\r\n",
            &[],
        )
        .await;

        let (mut current, _) = listener.accept().await.unwrap();
        let request = read_request(&mut current).await;
        assert!(
            String::from_utf8_lossy(&request)
                .starts_with("GET /vault/yututui/v2/capability/conditional-put-v1.age HTTP/1.1")
        );
        respond(
            &mut current,
            &format!(
                "HTTP/1.1 200 OK\r\nETag: \"probe-existing\"\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                server_marker.len()
            ),
            &server_marker,
        )
        .await;

        let (mut matched, _) = listener.accept().await.unwrap();
        let request = read_request(&mut matched).await;
        assert!(
            String::from_utf8_lossy(&request)
                .to_ascii_lowercase()
                .contains("\r\nif-match: \"probe-existing\"\r\n")
        );
        assert_eq!(request_body(&request), replacement);
        respond(
            &mut matched,
            "HTTP/1.1 204 No Content\r\nETag: \"probe-replaced\"\r\nContent-Length: 0\r\nConnection: close\r\n\r\n",
            &[],
        )
        .await;

        let (mut final_readback, _) = listener.accept().await.unwrap();
        read_request(&mut final_readback).await;
        respond(
            &mut final_readback,
            &format!(
                "HTTP/1.1 200 OK\r\nETag: \"probe-replaced\"\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                replacement.len()
            ),
            &replacement,
        )
        .await;

        respond_capability_propfind(&listener).await;
    });

    let capabilities = tokio::task::spawn_blocking(move || {
        let credential = credential();
        BlockingWebDavTransport::new(&url, &credential)
            .unwrap()
            .probe_capabilities()
            .unwrap()
    })
    .await
    .unwrap();
    assert!(capabilities.supports_encrypted_sync());
    assert!(!existing_marker.is_empty());
    server.await.unwrap();
}

#[tokio::test]
#[cfg_attr(
    windows,
    ignore = "GitHub Windows loopback can abort or stall this raw-socket fixture"
)]
async fn capability_probe_rejects_a_server_that_rejects_valid_matched_writes() {
    let marker = encrypted_object().as_bytes().to_vec();
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = endpoint(&listener, "/vault");
    let server = tokio::spawn(async move {
        let (mut options, _) = listener.accept().await.unwrap();
        read_request(&mut options).await;
        respond(
            &mut options,
            "HTTP/1.1 200 OK\r\nDAV: 1\r\nAllow: OPTIONS, PROPFIND, MKCOL, GET, PUT\r\nContent-Length: 0\r\nConnection: close\r\n\r\n",
            &[],
        )
        .await;
        for _ in 0..3 {
            let (mut collection, _) = listener.accept().await.unwrap();
            read_request(&mut collection).await;
            respond(
                &mut collection,
                "HTTP/1.1 405 Method Not Allowed\r\nContent-Length: 0\r\nConnection: close\r\n\r\n",
                &[],
            )
            .await;
        }

        let (mut existing, _) = listener.accept().await.unwrap();
        read_request(&mut existing).await;
        respond(
            &mut existing,
            &format!(
                "HTTP/1.1 200 OK\r\nETag: \"probe-existing\"\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                marker.len()
            ),
            &marker,
        )
        .await;

        let (mut create, _) = listener.accept().await.unwrap();
        read_request(&mut create).await;
        respond(
            &mut create,
            "HTTP/1.1 412 Precondition Failed\r\nContent-Length: 0\r\nConnection: close\r\n\r\n",
            &[],
        )
        .await;

        let (mut mismatch, _) = listener.accept().await.unwrap();
        let request = read_request(&mut mismatch).await;
        assert!(
            String::from_utf8_lossy(&request)
                .to_ascii_lowercase()
                .contains("\r\nif-match: \"yututui-probe-")
        );
        respond(
            &mut mismatch,
            "HTTP/1.1 412 Precondition Failed\r\nContent-Length: 0\r\nConnection: close\r\n\r\n",
            &[],
        )
        .await;

        let (mut current, _) = listener.accept().await.unwrap();
        read_request(&mut current).await;
        respond(
            &mut current,
            &format!(
                "HTTP/1.1 200 OK\r\nETag: \"probe-existing\"\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                marker.len()
            ),
            &marker,
        )
        .await;

        let (mut rejected, _) = listener.accept().await.unwrap();
        let request = read_request(&mut rejected).await;
        assert!(
            String::from_utf8_lossy(&request)
                .to_ascii_lowercase()
                .contains("\r\nif-match: \"probe-existing\"\r\n")
        );
        respond(
            &mut rejected,
            "HTTP/1.1 412 Precondition Failed\r\nContent-Length: 0\r\nConnection: close\r\n\r\n",
            &[],
        )
        .await;
    });

    let error = tokio::task::spawn_blocking(move || {
        let credential = credential();
        BlockingWebDavTransport::new(&url, &credential)
            .unwrap()
            .probe_capabilities()
            .unwrap_err()
    })
    .await
    .unwrap();
    assert_eq!(error, WebDavError::MethodUnsupported);
    server.await.unwrap();
}

#[tokio::test]
#[cfg_attr(
    windows,
    ignore = "GitHub Windows loopback can abort or stall this raw-socket fixture"
)]
async fn capability_probe_rejects_a_server_that_ignores_if_match() {
    let marker = encrypted_object().as_bytes().to_vec();
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = endpoint(&listener, "/vault");
    let server = tokio::spawn(async move {
        let (mut options, _) = listener.accept().await.unwrap();
        read_request(&mut options).await;
        respond(
            &mut options,
            "HTTP/1.1 200 OK\r\nDAV: 1\r\nAllow: OPTIONS, PROPFIND, MKCOL, GET, PUT\r\nContent-Length: 0\r\nConnection: close\r\n\r\n",
            &[],
        )
        .await;
        for _ in 0..3 {
            let (mut collection, _) = listener.accept().await.unwrap();
            read_request(&mut collection).await;
            respond(
                &mut collection,
                "HTTP/1.1 405 Method Not Allowed\r\nContent-Length: 0\r\nConnection: close\r\n\r\n",
                &[],
            )
            .await;
        }
        let (mut existing, _) = listener.accept().await.unwrap();
        read_request(&mut existing).await;
        respond(
            &mut existing,
            &format!(
                "HTTP/1.1 200 OK\r\nETag: \"probe-existing\"\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                marker.len()
            ),
            &marker,
        )
        .await;
        let (mut create, _) = listener.accept().await.unwrap();
        read_request(&mut create).await;
        respond(
            &mut create,
            "HTTP/1.1 412 Precondition Failed\r\nContent-Length: 0\r\nConnection: close\r\n\r\n",
            &[],
        )
        .await;
        let (mut ignored_match, _) = listener.accept().await.unwrap();
        let request = read_request(&mut ignored_match).await;
        assert!(
            String::from_utf8_lossy(&request)
                .to_ascii_lowercase()
                .contains("\r\nif-match: \"yututui-probe-")
        );
        respond(
            &mut ignored_match,
            "HTTP/1.1 204 No Content\r\nContent-Length: 0\r\nConnection: close\r\n\r\n",
            &[],
        )
        .await;
    });

    let error = tokio::task::spawn_blocking(move || {
        let credential = credential();
        BlockingWebDavTransport::new(&url, &credential)
            .unwrap()
            .probe_capabilities()
            .unwrap_err()
    })
    .await
    .unwrap();
    assert_eq!(error, WebDavError::MethodUnsupported);
    server.await.unwrap();
}

#[tokio::test]
#[cfg_attr(
    windows,
    ignore = "GitHub Windows loopback can abort or stall this raw-socket fixture"
)]
async fn ambiguous_put_is_verified_by_bounded_get_and_hash() {
    let object = encrypted_object();
    let expected_bytes = object.as_bytes().to_vec();
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let server = tokio::spawn(async move {
        let (mut put, _) = listener.accept().await.unwrap();
        let request = read_request(&mut put).await;
        assert!(String::from_utf8_lossy(&request).starts_with("PUT /vault/manifest HTTP/1.1"));
        // Drop the connection after consuming the complete PUT. The client cannot know whether
        // the server committed it and must perform a readback.
        put.shutdown().await.unwrap();

        let (mut get, _) = listener.accept().await.unwrap();
        let request = read_request(&mut get).await;
        assert!(String::from_utf8_lossy(&request).starts_with("GET /vault/manifest HTTP/1.1"));
        respond(
            &mut get,
            &format!(
                "HTTP/1.1 200 OK\r\nETag: \"verified\"\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                expected_bytes.len()
            ),
            &expected_bytes,
        )
        .await;
    });
    let key = ObjectKey::new("manifest").unwrap();
    let result = WebDavClient::new(&format!("http://{address}/vault"))
        .unwrap()
        .put(&key, &object, ObjectCondition::CreateOnly, &credential())
        .await
        .unwrap();
    assert!(matches!(result, ObjectWriteResult::AlreadyPresent(_)));
    server.await.unwrap();
}

#[tokio::test]
#[cfg_attr(
    windows,
    ignore = "GitHub Windows loopback can abort or stall this raw-socket fixture"
)]
async fn successful_put_without_etag_requires_matching_readback() {
    let object = encrypted_object();
    let expected_bytes = object.as_bytes().to_vec();
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let server = tokio::spawn(async move {
        let (mut put, _) = listener.accept().await.unwrap();
        read_request(&mut put).await;
        respond(
            &mut put,
            "HTTP/1.1 204 No Content\r\nContent-Length: 0\r\nConnection: close\r\n\r\n",
            &[],
        )
        .await;
        let (mut get, _) = listener.accept().await.unwrap();
        read_request(&mut get).await;
        respond(
            &mut get,
            &format!(
                "HTTP/1.1 200 OK\r\nETag: \"after-write\"\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                expected_bytes.len()
            ),
            &expected_bytes,
        )
        .await;
    });
    let key = ObjectKey::new("devices/device-one/head.age").unwrap();
    let result = WebDavClient::new(&format!("http://{address}/vault"))
        .unwrap()
        .put(
            &key,
            &object,
            ObjectCondition::Match("\"before-write\"".to_owned()),
            &credential(),
        )
        .await
        .unwrap();
    let ObjectWriteResult::Updated(metadata) = result else {
        panic!("expected a verified update");
    };
    assert_eq!(metadata.etag, "\"after-write\"");
    server.await.unwrap();
}

#[tokio::test]
#[cfg_attr(
    windows,
    ignore = "GitHub Windows loopback can abort or stall this raw-socket fixture"
)]
async fn get_rejects_weak_etags_before_accepting_remote_state() {
    let object = encrypted_object();
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let server = tokio::spawn(async move {
        let (mut stream, _) = listener.accept().await.unwrap();
        read_request(&mut stream).await;
        respond(
            &mut stream,
            &format!(
                "HTTP/1.1 200 OK\r\nETag: W/\"weak\"\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                object.as_bytes().len()
            ),
            object.as_bytes(),
        )
        .await;
    });
    let key = ObjectKey::new("manifest").unwrap();
    let error = WebDavClient::new(&format!("http://{address}/vault"))
        .unwrap()
        .get(&key, &credential(), 1024)
        .await
        .unwrap_err();
    assert_eq!(error, WebDavError::MissingStrongEntityTag);
    server.await.unwrap();
}

#[tokio::test]
#[cfg_attr(
    windows,
    ignore = "GitHub Windows loopback can abort or stall this raw-socket fixture"
)]
async fn blocking_adapter_creates_ancestors_and_maps_preconditions() {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = endpoint(&listener, "/vault");
    let server = tokio::spawn(async move {
        for expected in ["/vault/devices", "/vault/devices/device-one"] {
            let (mut stream, _) = listener.accept().await.unwrap();
            let request = read_request(&mut stream).await;
            let request = String::from_utf8_lossy(&request);
            assert!(
                request.starts_with(&format!("MKCOL {expected} HTTP/1.1\r\n")),
                "{request}"
            );
            respond(
                &mut stream,
                "HTTP/1.1 201 Created\r\nContent-Length: 0\r\nConnection: close\r\n\r\n",
                &[],
            )
            .await;
        }
        let (mut stream, _) = listener.accept().await.unwrap();
        let request = read_request(&mut stream).await;
        let request = String::from_utf8_lossy(&request);
        assert!(request.starts_with("PUT /vault/devices/device-one/head.age HTTP/1.1\r\n"));
        respond(
            &mut stream,
            "HTTP/1.1 412 Precondition Failed\r\nContent-Length: 0\r\nConnection: close\r\n\r\n",
            &[],
        )
        .await;
    });
    let error = tokio::task::spawn_blocking(move || {
        let credential = credential();
        let transport = BlockingWebDavTransport::new(&url, &credential).unwrap();
        transport
            .put(
                &ObjectKey::new("devices/device-one/head.age").unwrap(),
                &encrypted_object(),
                ObjectCondition::CreateOnly,
            )
            .unwrap_err()
    })
    .await
    .unwrap();
    assert_eq!(error, VaultError::PreconditionFailed);
    server.await.unwrap();
}

#[tokio::test]
#[cfg_attr(
    windows,
    ignore = "GitHub Windows loopback can abort or stall this raw-socket fixture"
)]
async fn blocking_adapter_reuses_one_deadline_for_ancestor_collections() {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = endpoint(&listener, "/vault");
    let server = tokio::spawn(async move {
        let (mut first, _) = listener.accept().await.unwrap();
        let request = read_request(&mut first).await;
        assert!(String::from_utf8_lossy(&request).starts_with("MKCOL /vault/devices HTTP/1.1\r\n"));
        tokio::time::sleep(Duration::from_millis(300)).await;
        drop(first);
        assert!(
            tokio::time::timeout(Duration::from_millis(30), listener.accept())
                .await
                .is_err(),
            "an expired shared deadline must prevent the next ancestor MKCOL"
        );
    });

    let started = std::time::Instant::now();
    let error = tokio::task::spawn_blocking(move || {
        let credential = credential();
        BlockingWebDavTransport::new(&url, &credential)
            .unwrap()
            .put_with_deadline(
                &ObjectKey::new("devices/device-one/head.age").unwrap(),
                &encrypted_object(),
                ObjectCondition::CreateOnly,
                VaultDeadline::from_now(Duration::from_millis(75)),
            )
            .unwrap_err()
    })
    .await
    .unwrap();
    assert_eq!(error, VaultError::RemoteUnavailable);
    assert!(
        started.elapsed() < Duration::from_millis(200),
        "one delayed MKCOL must not outlive the whole-operation deadline"
    );
    server.await.unwrap();
}

#[tokio::test]
#[cfg_attr(
    windows,
    ignore = "GitHub Windows loopback can abort or stall this raw-socket fixture"
)]
async fn ambiguous_put_readback_reuses_the_original_deadline() {
    let object = encrypted_object();
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let server = tokio::spawn(async move {
        let (mut put, _) = listener.accept().await.unwrap();
        read_request(&mut put).await;
        respond(
            &mut put,
            "HTTP/1.1 500 Internal Server Error\r\nContent-Length: 0\r\nConnection: close\r\n\r\n",
            &[],
        )
        .await;

        let (mut get, _) = listener.accept().await.unwrap();
        let request = read_request(&mut get).await;
        assert!(String::from_utf8_lossy(&request).starts_with("GET /vault/manifest HTTP/1.1\r\n"));
        tokio::time::sleep(Duration::from_millis(300)).await;
        drop(get);
    });

    let client = WebDavClient::new(&format!("http://{address}/vault")).unwrap();
    let key = ObjectKey::new("manifest").unwrap();
    let started = std::time::Instant::now();
    let error = client
        .put_with_deadline(
            &key,
            &object,
            ObjectCondition::CreateOnly,
            &credential(),
            VaultDeadline::from_now(Duration::from_millis(75)),
        )
        .await
        .unwrap_err();
    assert_eq!(error, WebDavError::RequestFailed);
    assert!(
        started.elapsed() < Duration::from_millis(200),
        "ambiguous-write GET must use the PUT's remaining deadline"
    );
    server.await.unwrap();
}

#[tokio::test]
#[cfg_attr(
    windows,
    ignore = "GitHub Windows loopback can abort or stall this raw-socket fixture"
)]
async fn blocking_adapter_lists_with_bounded_depth_one_traversal() {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = endpoint(&listener, "/vault");
    let server = tokio::spawn(async move {
        let responses = [
            (
                "/vault/devices",
                br#"<d:multistatus xmlns:d="DAV:">
<d:response><d:href>/vault/devices/</d:href><d:propstat><d:prop><d:resourcetype><d:collection/></d:resourcetype></d:prop><d:status>HTTP/1.1 200 OK</d:status></d:propstat></d:response>
<d:response><d:href>/vault/devices/device-one/</d:href><d:propstat><d:prop><d:resourcetype><d:collection/></d:resourcetype></d:prop><d:status>HTTP/1.1 200 OK</d:status></d:propstat></d:response>
</d:multistatus>"#
                    .as_slice(),
            ),
            (
                "/vault/devices/device-one",
                br#"<d:multistatus xmlns:d="DAV:">
<d:response><d:href>/vault/devices/device-one/</d:href><d:propstat><d:prop><d:resourcetype><d:collection/></d:resourcetype></d:prop><d:status>HTTP/1.1 200 OK</d:status></d:propstat></d:response>
<d:response><d:href>/vault/devices/device-one/head.age</d:href><d:propstat><d:prop><d:getetag>&quot;head-1&quot;</d:getetag><d:getcontentlength>777</d:getcontentlength><d:resourcetype/></d:prop><d:status>HTTP/1.1 200 OK</d:status></d:propstat></d:response>
</d:multistatus>"#
                    .as_slice(),
            ),
        ];
        for (expected, body) in responses {
            let (mut stream, _) = listener.accept().await.unwrap();
            let request = read_request(&mut stream).await;
            let request = String::from_utf8_lossy(&request).to_ascii_lowercase();
            assert!(
                request.starts_with(&format!("propfind {expected} http/1.1\r\n")),
                "{request}"
            );
            assert!(request.contains("\r\ndepth: 1\r\n"));
            respond(
                &mut stream,
                &format!(
                    "HTTP/1.1 207 Multi-Status\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                    body.len()
                ),
                body,
            )
            .await;
        }
    });
    let outcome = tokio::task::spawn_blocking(move || {
        let credential = credential();
        let transport = BlockingWebDavTransport::new(&url, &credential).unwrap();
        transport
            .list_with_limits(
                &ObjectKey::new("devices").unwrap(),
                ListLimits::for_returned_objects(10),
                VaultDeadline::from_now(Duration::from_secs(5)),
            )
            .unwrap()
    })
    .await
    .unwrap();
    assert_eq!(
        outcome.objects,
        vec![ObjectMetadata {
            key: ObjectKey::new("devices/device-one/head.age").unwrap(),
            etag: "\"head-1\"".to_owned(),
            content_length: 777,
        }]
    );
    assert_eq!(outcome.cost.requests, 2);
    assert_eq!(outcome.cost.scanned_collections, 2);
    assert_eq!(outcome.cost.scanned_resources, 4);
    assert_eq!(outcome.cost.returned_objects, 1);
    assert!(outcome.cost.response_bytes > 0);
    server.await.unwrap();
}

#[tokio::test]
#[cfg_attr(
    windows,
    ignore = "GitHub Windows loopback can abort or stall this raw-socket fixture"
)]
async fn recursive_list_charges_branching_empty_collections() {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = endpoint(&listener, "/vault");
    let server = tokio::spawn(async move {
        let responses = [
            (
                "/vault/devices",
                br#"<d:multistatus xmlns:d="DAV:">
<d:response><d:href>/vault/devices/</d:href><d:propstat><d:prop><d:resourcetype><d:collection/></d:resourcetype></d:prop><d:status>HTTP/1.1 200 OK</d:status></d:propstat></d:response>
<d:response><d:href>/vault/devices/a/</d:href><d:propstat><d:prop><d:resourcetype><d:collection/></d:resourcetype></d:prop><d:status>HTTP/1.1 200 OK</d:status></d:propstat></d:response>
<d:response><d:href>/vault/devices/b/</d:href><d:propstat><d:prop><d:resourcetype><d:collection/></d:resourcetype></d:prop><d:status>HTTP/1.1 200 OK</d:status></d:propstat></d:response>
</d:multistatus>"#
                    .as_slice(),
            ),
            (
                "/vault/devices/a",
                br#"<d:multistatus xmlns:d="DAV:"><d:response><d:href>/vault/devices/a/</d:href><d:propstat><d:prop><d:resourcetype><d:collection/></d:resourcetype></d:prop><d:status>HTTP/1.1 200 OK</d:status></d:propstat></d:response></d:multistatus>"#
                    .as_slice(),
            ),
            (
                "/vault/devices/b",
                br#"<d:multistatus xmlns:d="DAV:"><d:response><d:href>/vault/devices/b/</d:href><d:propstat><d:prop><d:resourcetype><d:collection/></d:resourcetype></d:prop><d:status>HTTP/1.1 200 OK</d:status></d:propstat></d:response></d:multistatus>"#
                    .as_slice(),
            ),
        ];
        for (expected, body) in responses {
            let (mut stream, _) = listener.accept().await.unwrap();
            let request = read_request(&mut stream).await;
            assert!(
                String::from_utf8_lossy(&request)
                    .starts_with(&format!("PROPFIND {expected} HTTP/1.1\r\n"))
            );
            respond(
                &mut stream,
                &format!(
                    "HTTP/1.1 207 Multi-Status\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                    body.len()
                ),
                body,
            )
            .await;
        }
    });
    let outcome = tokio::task::spawn_blocking(move || {
        let credential = credential();
        BlockingWebDavTransport::new(&url, &credential)
            .unwrap()
            .list_with_limits(
                &ObjectKey::new("devices").unwrap(),
                ListLimits::for_returned_objects(10),
                VaultDeadline::from_now(Duration::from_secs(5)),
            )
            .unwrap()
    })
    .await
    .unwrap();

    assert!(outcome.objects.is_empty());
    assert_eq!(outcome.cost.requests, 3);
    assert_eq!(outcome.cost.scanned_collections, 3);
    assert_eq!(outcome.cost.scanned_resources, 5);
    assert_eq!(outcome.cost.returned_objects, 0);
    assert!(outcome.cost.response_bytes > 0);
    server.await.unwrap();
}

#[tokio::test]
#[cfg_attr(
    windows,
    ignore = "GitHub Windows loopback can abort or stall this raw-socket fixture"
)]
async fn expired_recursive_list_deadline_releases_without_a_request() {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = endpoint(&listener, "/vault");
    let error = tokio::task::spawn_blocking(move || {
        let credential = credential();
        BlockingWebDavTransport::new(&url, &credential)
            .unwrap()
            .list_with_limits(
                &ObjectKey::new("devices").unwrap(),
                ListLimits::for_returned_objects(10),
                VaultDeadline::expired(),
            )
            .unwrap_err()
    })
    .await
    .unwrap();
    assert_eq!(error, VaultError::RemoteUnavailable);
    assert!(
        tokio::time::timeout(Duration::from_millis(20), listener.accept())
            .await
            .is_err(),
        "an expired deadline must not start PROPFIND"
    );
}

#[tokio::test]
#[cfg_attr(
    windows,
    ignore = "GitHub Windows loopback can abort or stall this raw-socket fixture"
)]
async fn recursive_list_enforces_one_cumulative_xml_byte_budget() {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = endpoint(&listener, "/vault");
    let response_size = MAX_PROPFIND_BYTES / 2 + 1_024;
    let server = tokio::spawn(async move {
        let prefix = br#"<d:multistatus xmlns:d="DAV:">
<d:response><d:href>/vault/devices/</d:href><d:propstat><d:prop><d:resourcetype><d:collection/></d:resourcetype></d:prop><d:status>HTTP/1.1 200 OK</d:status></d:propstat></d:response>
<d:response><d:href>/vault/devices/device-one/</d:href><d:propstat><d:prop><d:resourcetype><d:collection/></d:resourcetype></d:prop><d:status>HTTP/1.1 200 OK</d:status></d:propstat></d:response>
"#;
        let suffix = b"</d:multistatus>";
        let mut first_body = Vec::with_capacity(response_size);
        first_body.extend_from_slice(prefix);
        first_body.resize(response_size - suffix.len(), b' ');
        first_body.extend_from_slice(suffix);

        let (mut first, _) = listener.accept().await.unwrap();
        read_request(&mut first).await;
        respond(
            &mut first,
            &format!(
                "HTTP/1.1 207 Multi-Status\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                first_body.len()
            ),
            &first_body,
        )
        .await;

        let (mut second, _) = listener.accept().await.unwrap();
        read_request(&mut second).await;
        second
            .write_all(
                format!(
                    "HTTP/1.1 207 Multi-Status\r\nContent-Length: {response_size}\r\nConnection: close\r\n\r\n"
                )
                .as_bytes(),
            )
            .await
            .unwrap();
        second.shutdown().await.unwrap();
    });

    let error = tokio::task::spawn_blocking(move || {
        let credential = credential();
        BlockingWebDavTransport::new(&url, &credential)
            .unwrap()
            .list(&ObjectKey::new("devices").unwrap(), 10)
            .unwrap_err()
    })
    .await
    .unwrap();
    assert_eq!(error, VaultError::ResourceLimitExceeded);
    server.await.unwrap();
}

#[tokio::test]
#[cfg_attr(
    windows,
    ignore = "GitHub Windows loopback can abort or stall this raw-socket fixture"
)]
async fn recursive_list_rejects_collections_beyond_the_depth_budget() {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = endpoint(&listener, "/vault");
    let server = tokio::spawn(async move {
        let mut current = "devices".to_owned();
        for depth in 0..=MAX_LIST_DEPTH {
            let child = format!("{current}/level-{depth}");
            let body = format!(
                r#"<d:multistatus xmlns:d="DAV:">
<d:response><d:href>/vault/{current}/</d:href><d:propstat><d:prop><d:resourcetype><d:collection/></d:resourcetype></d:prop><d:status>HTTP/1.1 200 OK</d:status></d:propstat></d:response>
<d:response><d:href>/vault/{child}/</d:href><d:propstat><d:prop><d:resourcetype><d:collection/></d:resourcetype></d:prop><d:status>HTTP/1.1 200 OK</d:status></d:propstat></d:response>
</d:multistatus>"#
            );
            let (mut stream, _) = listener.accept().await.unwrap();
            let request = read_request(&mut stream).await;
            assert!(
                String::from_utf8_lossy(&request)
                    .starts_with(&format!("PROPFIND /vault/{current} HTTP/1.1\r\n"))
            );
            respond(
                &mut stream,
                &format!(
                    "HTTP/1.1 207 Multi-Status\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                    body.len()
                ),
                body.as_bytes(),
            )
            .await;
            current = child;
        }
    });

    let error = tokio::task::spawn_blocking(move || {
        let credential = credential();
        BlockingWebDavTransport::new(&url, &credential)
            .unwrap()
            .list(&ObjectKey::new("devices").unwrap(), 10)
            .unwrap_err()
    })
    .await
    .unwrap();
    assert_eq!(error, VaultError::ResourceLimitExceeded);
    server.await.unwrap();
}
