use age::secrecy::SecretString;
use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};

use super::super::super::model::{AccountScopeId, BackendId, ItemId, OpenSubsonicItemRef};
use super::super::super::{ConfiguredPrivateOrigin, OpenSubsonicProfile, ServerCredential};
use super::super::OpenSubsonicClient;

pub(super) struct TestClient {
    pub(super) client: OpenSubsonicClient,
    pub(super) credential: ServerCredential,
    pub(super) backend_id: BackendId,
    pub(super) account_scope_id: AccountScopeId,
}

impl TestClient {
    pub(super) fn item(&self, item_id: &str) -> OpenSubsonicItemRef {
        OpenSubsonicItemRef::new(
            self.backend_id.clone(),
            self.account_scope_id.clone(),
            ItemId::new(item_id).unwrap(),
        )
    }
}

pub(super) async fn test_client(port: u16) -> TestClient {
    test_client_with_credential(
        port,
        ServerCredential::api_key(SecretString::from("sentinel-api-key".to_owned())).unwrap(),
    )
    .await
}

pub(super) async fn test_client_with_password(port: u16) -> TestClient {
    test_client_with_credential(
        port,
        ServerCredential::password("alice", SecretString::from("sentinel-password".to_owned()))
            .unwrap(),
    )
    .await
}

pub(super) async fn test_client_with_credential(
    port: u16,
    credential: ServerCredential,
) -> TestClient {
    let profile = OpenSubsonicProfile::new(
        "Test server",
        ConfiguredPrivateOrigin::new(&format!("http://127.0.0.1:{port}/"), true).unwrap(),
        None,
    )
    .unwrap();
    let backend_id = profile.backend_id().clone();
    let account_scope_id = profile.account_scope_id().clone();
    let client = OpenSubsonicClient::connect(&profile).await.unwrap();
    TestClient {
        client,
        credential,
        backend_id,
        account_scope_id,
    }
}

pub(super) async fn read_request(stream: &mut tokio::net::TcpStream) -> String {
    let mut request = Vec::new();
    let mut byte = [0_u8; 1];
    while request.len() < 32 * 1024 {
        if stream.read(&mut byte).await.unwrap() == 0 {
            break;
        }
        request.push(byte[0]);
        if request.ends_with(b"\r\n\r\n") {
            break;
        }
    }
    String::from_utf8(request).unwrap()
}

pub(super) async fn write_json(stream: &mut tokio::net::TcpStream, body: &str) {
    let response = format!(
        "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
        body.len()
    );
    stream.write_all(response.as_bytes()).await.unwrap();
}

pub(super) fn request_target(request: &str) -> &str {
    request
        .lines()
        .next()
        .and_then(|line| line.split_ascii_whitespace().nth(1))
        .unwrap()
}

pub(super) fn query_values(request: &str, name: &str) -> Vec<String> {
    reqwest::Url::parse(&format!("http://fixture{}", request_target(request)))
        .unwrap()
        .query_pairs()
        .filter(|(key, _)| key == name)
        .map(|(_, value)| value.into_owned())
        .collect()
}
