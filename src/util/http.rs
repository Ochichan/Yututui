//! HTTP response helpers with explicit body-size limits.

use anyhow::{Context, Result, bail};
use serde::de::DeserializeOwned;

/// Build a bounded client that exposes redirects to the caller instead of following them.
/// Callers must handle builder failure explicitly; falling back to `Client::new()` would silently
/// discard both this redirect policy and the timeout.
pub(crate) fn build_no_redirect_client(
    user_agent: &str,
    timeout: std::time::Duration,
) -> reqwest::Result<reqwest::Client> {
    reqwest::Client::builder()
        .user_agent(user_agent)
        .timeout(timeout)
        .redirect(reqwest::redirect::Policy::none())
        .build()
}

/// Read a response body while enforcing `max_bytes`.
pub async fn read_response_limited(
    mut resp: reqwest::Response,
    max_bytes: usize,
) -> Result<Vec<u8>> {
    if let Some(len) = resp.content_length()
        && len > max_bytes as u64
    {
        bail!("response too large: {len} bytes > {max_bytes} bytes");
    }

    let mut out = Vec::new();
    while let Some(chunk) = resp.chunk().await.context("read response body")? {
        if out.len().saturating_add(chunk.len()) > max_bytes {
            bail!("response too large: more than {max_bytes} bytes");
        }
        out.extend_from_slice(&chunk);
    }
    Ok(out)
}

/// Decode JSON after applying [`read_response_limited`].
pub async fn json_limited<T: DeserializeOwned>(
    resp: reqwest::Response,
    max_bytes: usize,
) -> Result<T> {
    let bytes = read_response_limited(resp, max_bytes).await?;
    serde_json::from_slice(&bytes).context("decode limited JSON response")
}

#[cfg(test)]
mod tests {
    use tokio::io::AsyncWriteExt as _;

    use super::*;

    #[test]
    fn size_constants_are_plain_bytes() {
        assert_eq!(5 * 1024 * 1024, 5_242_880);
    }

    #[test]
    fn invalid_client_configuration_fails_instead_of_falling_back() {
        assert!(
            build_no_redirect_client("invalid\nuser-agent", std::time::Duration::from_secs(1))
                .is_err()
        );
    }

    #[tokio::test]
    async fn bounded_client_exposes_redirects_without_following_them() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let server = tokio::spawn(async move {
            let (mut stream, _) = listener.accept().await.unwrap();
            stream
                .write_all(
                    b"HTTP/1.1 302 Found\r\nLocation: http://127.0.0.1:9/must-not-follow\r\nContent-Length: 0\r\nConnection: close\r\n\r\n",
                )
                .await
                .unwrap();
        });
        let client =
            build_no_redirect_client("yututui-test", std::time::Duration::from_secs(1)).unwrap();

        let response = client
            .get(format!("http://{address}/redirect"))
            .send()
            .await
            .unwrap();

        assert!(response.status().is_redirection());
        server.await.unwrap();
    }

    #[tokio::test]
    async fn streamed_body_cap_rejects_chunked_overflow() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let server = tokio::spawn(async move {
            let (mut stream, _) = listener.accept().await.unwrap();
            stream
                .write_all(
                    b"HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\nConnection: close\r\n\r\n4\r\nabcd\r\n4\r\nefgh\r\n0\r\n\r\n",
                )
                .await
                .unwrap();
        });
        let client =
            build_no_redirect_client("yututui-test", std::time::Duration::from_secs(1)).unwrap();
        let response = client
            .get(format!("http://{address}/chunked"))
            .send()
            .await
            .unwrap();

        let error = read_response_limited(response, 6).await.unwrap_err();

        assert!(error.to_string().contains("more than 6 bytes"));
        server.await.unwrap();
    }
}
