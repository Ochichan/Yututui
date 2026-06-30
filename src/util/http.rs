//! HTTP response helpers with explicit body-size limits.

use anyhow::{Context, Result, bail};
use serde::de::DeserializeOwned;

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
    #[test]
    fn size_constants_are_plain_bytes() {
        assert_eq!(5 * 1024 * 1024, 5_242_880);
    }
}
