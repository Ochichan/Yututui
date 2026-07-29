//! Real-server tolerances: the connection probe must judge methods by using them, create a
//! missing vault root, and survive an entity tag that is only briefly weak.

use age::secrecy::SecretString;
use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};
use tokio::net::{TcpListener, TcpStream};

use super::*;
use crate::sync::{DeviceSecretMaterial, encrypt_json_to_recipients};

/// Exactly what Apache 2.4.68 mod_dav answers for an existing collection: no PUT (illegal on a
/// collection) and no MKCOL (illegal on a collection that already exists).
const APACHE_COLLECTION_ALLOW: &str =
    "OPTIONS,GET,HEAD,POST,DELETE,TRACE,PROPFIND,PROPPATCH,COPY,MOVE,LOCK,UNLOCK";

fn credential() -> VaultCredential {
    VaultCredential::password("sync-user", SecretString::from("sync-password")).unwrap()
}

fn encrypted_object() -> EncryptedObject {
    let device = DeviceSecretMaterial::generate_for("capability-test-device").unwrap();
    encrypt_json_to_recipients(
        &serde_json::json!({"protected": true}),
        &[device.public_identity().age_recipient],
    )
    .unwrap()
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

fn request_body(request: &[u8]) -> &[u8] {
    let end = request
        .windows(4)
        .position(|part| part == b"\r\n\r\n")
        .expect("test request has headers");
    &request[end + 4..]
}

fn endpoint(listener: &TcpListener, path: &str) -> String {
    format!("http://{}{path}", listener.local_addr().unwrap())
}

async fn accept_and_respond(listener: &TcpListener, head: &str, body: &[u8]) -> Vec<u8> {
    let (mut stream, _) = listener.accept().await.unwrap();
    let request = read_request(&mut stream).await;
    respond(&mut stream, head, body).await;
    request
}

/// Serve the whole conditional-write proof after OPTIONS: the vault root, the three protocol
/// collections, then create/repeat/mismatch/readback/matched/readback and the bounded PROPFIND.
async fn serve_conditional_write_proof(listener: &TcpListener, root_status: &str) -> Vec<u8> {
    let root = accept_and_respond(listener, root_status, &[]).await;
    assert!(String::from_utf8_lossy(&root).starts_with("MKCOL /vault/ HTTP/1.1\r\n"));
    for expected in [
        "/vault/yututui",
        "/vault/yututui/v2",
        "/vault/yututui/v2/capability",
    ] {
        let request = accept_and_respond(
            listener,
            "HTTP/1.1 201 Created\r\nContent-Length: 0\r\nConnection: close\r\n\r\n",
            &[],
        )
        .await;
        assert!(
            String::from_utf8_lossy(&request)
                .starts_with(&format!("MKCOL {expected} HTTP/1.1\r\n"))
        );
    }

    accept_and_respond(
        listener,
        "HTTP/1.1 404 Not Found\r\nContent-Length: 0\r\nConnection: close\r\n\r\n",
        &[],
    )
    .await;
    let create = accept_and_respond(
        listener,
        "HTTP/1.1 201 Created\r\nETag: \"probe-1\"\r\nContent-Length: 0\r\nConnection: close\r\n\r\n",
        &[],
    )
    .await;
    let marker = request_body(&create).to_vec();
    accept_and_respond(
        listener,
        "HTTP/1.1 412 Precondition Failed\r\nContent-Length: 0\r\nConnection: close\r\n\r\n",
        &[],
    )
    .await;
    accept_and_respond(
        listener,
        "HTTP/1.1 412 Precondition Failed\r\nContent-Length: 0\r\nConnection: close\r\n\r\n",
        &[],
    )
    .await;
    let ok = format!(
        "HTTP/1.1 200 OK\r\nETag: \"probe-1\"\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
        marker.len()
    );
    accept_and_respond(listener, &ok, &marker).await;
    accept_and_respond(
        listener,
        "HTTP/1.1 204 No Content\r\nETag: \"probe-2\"\r\nContent-Length: 0\r\nConnection: close\r\n\r\n",
        &[],
    )
    .await;
    accept_and_respond(listener, &ok, &marker).await;

    let propfind = br#"<d:multistatus xmlns:d="DAV:"><d:response><d:href>/vault/yututui/v2/capability/</d:href><d:propstat><d:prop><d:resourcetype><d:collection/></d:resourcetype></d:prop><d:status>HTTP/1.1 200 OK</d:status></d:propstat></d:response></d:multistatus>"#;
    accept_and_respond(
        listener,
        &format!(
            "HTTP/1.1 207 Multi-Status\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
            propfind.len()
        ),
        propfind,
    )
    .await;
    marker
}

#[tokio::test]
#[cfg_attr(
    windows,
    ignore = "GitHub Windows loopback can abort or stall this raw-socket fixture"
)]
async fn allow_header_without_mkcol_or_put_still_passes_when_the_methods_work() {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = endpoint(&listener, "/vault");
    let server = tokio::spawn(async move {
        let options = accept_and_respond(
            &listener,
            &format!(
                "HTTP/1.1 200 OK\r\nDAV: 1,2\r\nAllow: {APACHE_COLLECTION_ALLOW}\r\nContent-Length: 0\r\nConnection: close\r\n\r\n"
            ),
            &[],
        )
        .await;
        assert!(String::from_utf8_lossy(&options).starts_with("OPTIONS /vault/ HTTP/1.1"));
        serve_conditional_write_proof(
            &listener,
            "HTTP/1.1 405 Method Not Allowed\r\nContent-Length: 0\r\nConnection: close\r\n\r\n",
        )
        .await;
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
    assert!(capabilities.mkcol, "a performed MKCOL is proof of MKCOL");
    assert!(capabilities.put, "a performed PUT is proof of PUT");
    assert!(capabilities.get);
    assert!(capabilities.propfind);
    server.await.unwrap();
}

#[tokio::test]
#[cfg_attr(
    windows,
    ignore = "GitHub Windows loopback can abort or stall this raw-socket fixture"
)]
async fn missing_vault_root_is_created_before_the_protocol_collections() {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = endpoint(&listener, "/vault");
    let server = tokio::spawn(async move {
        accept_and_respond(
            &listener,
            "HTTP/1.1 200 OK\r\nDAV: 1\r\nAllow: OPTIONS, PROPFIND, GET\r\nContent-Length: 0\r\nConnection: close\r\n\r\n",
            &[],
        )
        .await;
        serve_conditional_write_proof(
            &listener,
            "HTTP/1.1 201 Created\r\nContent-Length: 0\r\nConnection: close\r\n\r\n",
        )
        .await;
    });

    let url_for_probe = url.clone();
    tokio::task::spawn_blocking(move || {
        let credential = credential();
        BlockingWebDavTransport::new(&url_for_probe, &credential)
            .unwrap()
            .probe_capabilities()
            .unwrap()
    })
    .await
    .unwrap();
    server.await.unwrap();
}

#[tokio::test]
#[cfg_attr(
    windows,
    ignore = "GitHub Windows loopback can abort or stall this raw-socket fixture"
)]
async fn a_vault_root_whose_parent_is_missing_fails_closed() {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = endpoint(&listener, "/vault");
    let server = tokio::spawn(async move {
        accept_and_respond(
            &listener,
            "HTTP/1.1 200 OK\r\nDAV: 1\r\nAllow: OPTIONS, PROPFIND, MKCOL, GET, PUT\r\nContent-Length: 0\r\nConnection: close\r\n\r\n",
            &[],
        )
        .await;
        let root = accept_and_respond(
            &listener,
            "HTTP/1.1 409 Conflict\r\nContent-Length: 0\r\nConnection: close\r\n\r\n",
            &[],
        )
        .await;
        assert!(String::from_utf8_lossy(&root).starts_with("MKCOL /vault/ HTTP/1.1\r\n"));
    });

    let error = tokio::task::spawn_blocking(move || {
        let credential = credential();
        BlockingWebDavTransport::new(&url, &credential)
            .unwrap()
            .probe_capabilities()
            .expect_err("a vault root that cannot be created must not enable sync")
    })
    .await
    .unwrap();

    assert_eq!(error, WebDavError::Conflict);
    server.await.unwrap();
}

#[tokio::test]
#[cfg_attr(
    windows,
    ignore = "GitHub Windows loopback can abort or stall this raw-socket fixture"
)]
async fn readback_retries_through_a_transient_weak_entity_tag() {
    let object = encrypted_object();
    let body = object.as_bytes().to_vec();
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = endpoint(&listener, "/vault");
    let server = tokio::spawn(async move {
        // Apache mod_dav answers PUT without any ETag at all.
        accept_and_respond(
            &listener,
            "HTTP/1.1 201 Created\r\nContent-Length: 0\r\nConnection: close\r\n\r\n",
            &[],
        )
        .await;
        for tag in ["W/\"2-weak\"", "\"2-strong\""] {
            let request = accept_and_respond(
                &listener,
                &format!(
                    "HTTP/1.1 200 OK\r\nETag: {tag}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                    body.len()
                ),
                &body,
            )
            .await;
            assert!(String::from_utf8_lossy(&request).starts_with("GET /vault/manifest HTTP/1.1"));
        }
    });

    let credential = credential();
    let client = WebDavClient::new(&url).unwrap();
    let key = ObjectKey::new("manifest").unwrap();
    let result = client
        .put(&key, &object, ObjectCondition::CreateOnly, &credential)
        .await
        .expect("a tag that turns strong on the next read must not fail the write");

    match result {
        ObjectWriteResult::Created(metadata) => assert_eq!(metadata.etag, "\"2-strong\""),
        other => panic!("unexpected write result: {other:?}"),
    }
    server.await.unwrap();
}

#[tokio::test]
#[cfg_attr(
    windows,
    ignore = "GitHub Windows loopback can abort or stall this raw-socket fixture"
)]
async fn a_server_that_only_ever_reports_weak_entity_tags_still_fails() {
    let object = encrypted_object();
    let body = object.as_bytes().to_vec();
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = endpoint(&listener, "/vault");
    let server = tokio::spawn(async move {
        accept_and_respond(
            &listener,
            "HTTP/1.1 201 Created\r\nContent-Length: 0\r\nConnection: close\r\n\r\n",
            &[],
        )
        .await;
        for _ in 0..3 {
            accept_and_respond(
                &listener,
                &format!(
                    "HTTP/1.1 200 OK\r\nETag: W/\"2-weak\"\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                    body.len()
                ),
                &body,
            )
            .await;
        }
    });

    let credential = credential();
    let client = WebDavClient::new(&url).unwrap();
    let key = ObjectKey::new("manifest").unwrap();
    let error = client
        .put(&key, &object, ObjectCondition::CreateOnly, &credential)
        .await
        .expect_err("compare-and-swap requires a strong entity tag eventually");

    assert_eq!(error, WebDavError::MissingStrongEntityTag);
    server.await.unwrap();
}
