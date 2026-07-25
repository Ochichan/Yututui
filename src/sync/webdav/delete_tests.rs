use std::sync::Arc;
use std::time::Duration;

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
    let device = DeviceSecretMaterial::generate_for("webdav-delete-test").unwrap();
    encrypt_json_to_recipients(
        &serde_json::json!({"protected": true}),
        &[device.public_identity().age_recipient],
    )
    .unwrap()
}

async fn read_request(stream: &mut TcpStream) -> Vec<u8> {
    let mut bytes = Vec::new();
    let mut buffer = [0_u8; 4096];
    loop {
        let read = stream.read(&mut buffer).await.unwrap();
        assert!(read > 0, "request ended before its headers");
        bytes.extend_from_slice(&buffer[..read]);
        assert!(bytes.len() <= 64 * 1024, "test request exceeded cap");
        if bytes.windows(4).any(|part| part == b"\r\n\r\n") {
            return bytes;
        }
    }
}

async fn respond(stream: &mut TcpStream, head: &str, body: &[u8]) {
    stream.write_all(head.as_bytes()).await.unwrap();
    stream.write_all(body).await.unwrap();
    stream.shutdown().await.unwrap();
}

fn endpoint(listener: &TcpListener) -> String {
    format!("http://{}/vault", listener.local_addr().unwrap())
}

#[tokio::test]
#[cfg_attr(
    windows,
    ignore = "GitHub Windows loopback can abort or stall this raw-socket fixture"
)]
async fn blocking_delete_sends_strong_if_match_and_maps_success() {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = endpoint(&listener);
    let captured = Arc::new(Mutex::new(Vec::new()));
    let server_capture = Arc::clone(&captured);
    let server = tokio::spawn(async move {
        let (mut stream, _) = listener.accept().await.unwrap();
        *server_capture.lock().await = read_request(&mut stream).await;
        respond(
            &mut stream,
            "HTTP/1.1 204 No Content\r\nContent-Length: 0\r\nConnection: close\r\n\r\n",
            &[],
        )
        .await;
    });

    let result = tokio::task::spawn_blocking(move || {
        let credential = credential();
        BlockingWebDavTransport::new(&url, &credential)
            .unwrap()
            .delete_with_deadline(
                &ObjectKey::new("checkpoints/old.age").unwrap(),
                "\"generation-7\"",
                VaultDeadline::from_now(Duration::from_secs(5)),
            )
            .unwrap()
    })
    .await
    .unwrap();

    assert_eq!(result, ObjectDeleteResult::Deleted);
    let request = String::from_utf8_lossy(&captured.lock().await).to_ascii_lowercase();
    assert!(request.starts_with("delete /vault/checkpoints/old.age http/1.1\r\n"));
    assert!(request.contains("\r\nif-match: \"generation-7\"\r\n"));
    assert!(request.contains("\r\nauthorization: basic "));
    server.await.unwrap();
}

#[tokio::test]
#[cfg_attr(
    windows,
    ignore = "GitHub Windows loopback can abort or stall this raw-socket fixture"
)]
async fn delete_rejects_weak_etag_before_network_io() {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let client = WebDavClient::new(&endpoint(&listener)).unwrap();

    assert_eq!(
        client
            .delete(
                &ObjectKey::new("checkpoints/old.age").unwrap(),
                "W/\"generation-7\"",
                &credential(),
            )
            .await,
        Err(WebDavError::MissingStrongEntityTag)
    );
    assert!(
        tokio::time::timeout(Duration::from_millis(20), listener.accept())
            .await
            .is_err(),
        "invalid precondition must not start DELETE"
    );
}

#[tokio::test]
#[cfg_attr(
    windows,
    ignore = "GitHub Windows loopback can abort or stall this raw-socket fixture"
)]
async fn delete_redirect_must_stay_on_the_exact_origin() {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let server = tokio::spawn(async move {
        let (mut stream, _) = listener.accept().await.unwrap();
        read_request(&mut stream).await;
        respond(
            &mut stream,
            "HTTP/1.1 307 Temporary Redirect\r\nLocation: http://127.0.0.1:9/stolen\r\nContent-Length: 0\r\nConnection: close\r\n\r\n",
            &[],
        )
        .await;
    });

    let error = WebDavClient::new(&format!("http://{address}/vault"))
        .unwrap()
        .delete(
            &ObjectKey::new("checkpoints/old.age").unwrap(),
            "\"generation-7\"",
            &credential(),
        )
        .await
        .unwrap_err();

    assert_eq!(error, WebDavError::CrossOriginRedirect);
    server.await.unwrap();
}

#[tokio::test]
#[cfg_attr(
    windows,
    ignore = "GitHub Windows loopback can abort or stall this raw-socket fixture"
)]
async fn ambiguous_delete_uses_get_readback() {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let server = tokio::spawn(async move {
        let (mut delete, _) = listener.accept().await.unwrap();
        let request = read_request(&mut delete).await;
        let request = String::from_utf8_lossy(&request).to_ascii_lowercase();
        assert!(request.starts_with("delete /vault/checkpoints/old.age http/1.1\r\n"));
        assert!(request.contains("\r\nif-match: \"generation-7\"\r\n"));
        respond(
            &mut delete,
            "HTTP/1.1 500 Internal Server Error\r\nContent-Length: 0\r\nConnection: close\r\n\r\n",
            &[],
        )
        .await;

        let (mut readback, _) = listener.accept().await.unwrap();
        let request = read_request(&mut readback).await;
        assert!(
            String::from_utf8_lossy(&request)
                .starts_with("GET /vault/checkpoints/old.age HTTP/1.1\r\n")
        );
        respond(
            &mut readback,
            "HTTP/1.1 404 Not Found\r\nContent-Length: 0\r\nConnection: close\r\n\r\n",
            &[],
        )
        .await;
    });

    let result = WebDavClient::new(&format!("http://{address}/vault"))
        .unwrap()
        .delete(
            &ObjectKey::new("checkpoints/old.age").unwrap(),
            "\"generation-7\"",
            &credential(),
        )
        .await
        .unwrap();

    assert_eq!(result, ObjectDeleteResult::Deleted);
    server.await.unwrap();
}

#[tokio::test]
#[cfg_attr(
    windows,
    ignore = "GitHub Windows loopback can abort or stall this raw-socket fixture"
)]
async fn ambiguous_delete_never_removes_a_replacement_generation() {
    let replacement = encrypted_object();
    let response_body = replacement.as_bytes().to_vec();
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let server = tokio::spawn(async move {
        let (mut delete, _) = listener.accept().await.unwrap();
        read_request(&mut delete).await;
        respond(
            &mut delete,
            "HTTP/1.1 500 Internal Server Error\r\nContent-Length: 0\r\nConnection: close\r\n\r\n",
            &[],
        )
        .await;

        let (mut readback, _) = listener.accept().await.unwrap();
        read_request(&mut readback).await;
        respond(
            &mut readback,
            &format!(
                "HTTP/1.1 200 OK\r\nETag: \"replacement-8\"\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                response_body.len()
            ),
            &response_body,
        )
        .await;
    });

    let error = WebDavClient::new(&format!("http://{address}/vault"))
        .unwrap()
        .delete(
            &ObjectKey::new("checkpoints/old.age").unwrap(),
            "\"generation-7\"",
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
async fn ambiguous_delete_readback_reuses_original_deadline() {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let server = tokio::spawn(async move {
        let (mut delete, _) = listener.accept().await.unwrap();
        read_request(&mut delete).await;
        respond(
            &mut delete,
            "HTTP/1.1 500 Internal Server Error\r\nContent-Length: 0\r\nConnection: close\r\n\r\n",
            &[],
        )
        .await;

        let (mut readback, _) = listener.accept().await.unwrap();
        read_request(&mut readback).await;
        tokio::time::sleep(Duration::from_millis(300)).await;
        drop(readback);
    });

    let client = WebDavClient::new(&format!("http://{address}/vault")).unwrap();
    let started = std::time::Instant::now();
    let error = client
        .delete_with_deadline(
            &ObjectKey::new("checkpoints/old.age").unwrap(),
            "\"generation-7\"",
            &credential(),
            VaultDeadline::from_now(Duration::from_millis(75)),
        )
        .await
        .unwrap_err();

    assert_eq!(error, WebDavError::RequestFailed);
    assert!(
        started.elapsed() < Duration::from_millis(200),
        "DELETE and ambiguity GET must share one deadline"
    );
    server.await.unwrap();
}

#[tokio::test]
#[cfg_attr(
    windows,
    ignore = "GitHub Windows loopback can abort or stall this raw-socket fixture"
)]
async fn retry_after_delta_seconds_survives_the_blocking_transport_boundary() {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = endpoint(&listener);
    let server = tokio::spawn(async move {
        let (mut stream, _) = listener.accept().await.unwrap();
        read_request(&mut stream).await;
        respond(
            &mut stream,
            "HTTP/1.1 429 Too Many Requests\r\nRetry-After: 75\r\nContent-Length: 0\r\nConnection: close\r\n\r\n",
            &[],
        )
        .await;
    });

    let error = tokio::task::spawn_blocking(move || {
        let credential = credential();
        BlockingWebDavTransport::new(&url, &credential)
            .unwrap()
            .delete_with_deadline(
                &ObjectKey::new("checkpoints/old.age").unwrap(),
                "\"generation-7\"",
                VaultDeadline::from_now(Duration::from_secs(5)),
            )
            .unwrap_err()
    })
    .await
    .unwrap();

    assert_eq!(
        error,
        VaultError::RemoteRateLimited(Some(Duration::from_secs(75)))
    );
    assert_eq!(
        crate::sync::service::SyncServiceError::from(error),
        crate::sync::service::SyncServiceError::RateLimited(Some(Duration::from_secs(75)))
    );
    server.await.unwrap();
}

#[tokio::test]
#[cfg_attr(
    windows,
    ignore = "GitHub Windows loopback can abort or stall this raw-socket fixture"
)]
async fn delete_service_unavailable_preserves_retry_after_without_readback() {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = endpoint(&listener);
    let server = tokio::spawn(async move {
        let (mut stream, _) = listener.accept().await.unwrap();
        read_request(&mut stream).await;
        respond(
            &mut stream,
            "HTTP/1.1 503 Service Unavailable\r\nRetry-After: 90\r\nContent-Length: 0\r\nConnection: close\r\n\r\n",
            &[],
        )
        .await;
    });

    let error = WebDavClient::new(&url)
        .unwrap()
        .delete(
            &ObjectKey::new("checkpoints/old.age").unwrap(),
            "\"generation-7\"",
            &credential(),
        )
        .await
        .unwrap_err();

    assert_eq!(
        error,
        WebDavError::RateLimited(Some(Duration::from_secs(90)))
    );
    server.await.unwrap();
}

#[tokio::test]
#[cfg_attr(
    windows,
    ignore = "GitHub Windows loopback can abort or stall this raw-socket fixture"
)]
async fn service_unavailable_retry_after_is_preserved() {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let server = tokio::spawn(async move {
        let (mut stream, _) = listener.accept().await.unwrap();
        read_request(&mut stream).await;
        respond(
            &mut stream,
            "HTTP/1.1 503 Service Unavailable\r\nRetry-After: 90\r\nContent-Length: 0\r\nConnection: close\r\n\r\n",
            &[],
        )
        .await;
    });

    let error = WebDavClient::new(&format!("http://{address}/vault"))
        .unwrap()
        .get(
            &ObjectKey::new("manifest").unwrap(),
            &credential(),
            MAX_PROTECTED_PAYLOAD_BYTES,
        )
        .await
        .unwrap_err();

    assert_eq!(
        error,
        WebDavError::RateLimited(Some(Duration::from_secs(90)))
    );
    server.await.unwrap();
}
