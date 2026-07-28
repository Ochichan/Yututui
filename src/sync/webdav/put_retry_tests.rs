use std::time::Duration;

use age::secrecy::SecretString;
use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};
use tokio::net::{TcpListener, TcpStream};

use super::*;
use crate::sync::{DeviceSecretMaterial, encrypt_json_to_recipients};

fn credential() -> VaultCredential {
    VaultCredential::password("sync-user", SecretString::from("sync-password")).unwrap()
}

fn encrypted_object() -> EncryptedObject {
    let device = DeviceSecretMaterial::generate_for("put-retry-test-device").unwrap();
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

#[tokio::test]
#[cfg_attr(
    windows,
    ignore = "GitHub Windows loopback can abort or stall this raw-socket fixture"
)]
async fn put_service_unavailable_preserves_retry_after_when_readback_fails() {
    let object = encrypted_object();
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let server = tokio::spawn(async move {
        let (mut put, _) = listener.accept().await.unwrap();
        read_request(&mut put).await;
        respond(
            &mut put,
            "HTTP/1.1 503 Service Unavailable\r\nRetry-After: 600\r\nContent-Length: 0\r\nConnection: close\r\n\r\n",
            &[],
        )
        .await;

        let (mut get, _) = listener.accept().await.unwrap();
        let request = read_request(&mut get).await;
        assert!(String::from_utf8_lossy(&request).starts_with("GET /vault/manifest HTTP/1.1"));
        respond(
            &mut get,
            "HTTP/1.1 404 Not Found\r\nContent-Length: 0\r\nConnection: close\r\n\r\n",
            &[],
        )
        .await;
    });

    let key = ObjectKey::new("manifest").unwrap();
    let error = WebDavClient::new(&format!("http://{address}/vault"))
        .unwrap()
        .put(&key, &object, ObjectCondition::CreateOnly, &credential())
        .await
        .unwrap_err();
    assert_eq!(
        error,
        WebDavError::RateLimited(Some(Duration::from_secs(600)))
    );
    server.await.unwrap();
}

#[tokio::test]
#[cfg_attr(
    windows,
    ignore = "GitHub Windows loopback can abort or stall this raw-socket fixture"
)]
async fn put_service_unavailable_accepts_a_matching_readback() {
    let object = encrypted_object();
    let expected_bytes = object.as_bytes().to_vec();
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let server = tokio::spawn(async move {
        let (mut put, _) = listener.accept().await.unwrap();
        read_request(&mut put).await;
        respond(
            &mut put,
            "HTTP/1.1 503 Service Unavailable\r\nRetry-After: 600\r\nContent-Length: 0\r\nConnection: close\r\n\r\n",
            &[],
        )
        .await;

        let (mut get, _) = listener.accept().await.unwrap();
        read_request(&mut get).await;
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
