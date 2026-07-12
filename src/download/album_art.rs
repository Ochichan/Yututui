use std::time::Duration;

use anyhow::{Context as _, Result, anyhow, bail};

const ALBUM_ART_MAX_BYTES: usize = 8 * 1024 * 1024;

pub(super) async fn fetch_album_art(url: &str) -> Result<Vec<u8>> {
    let parsed = reqwest::Url::parse(url).context("invalid album artwork URL")?;
    if parsed.scheme() != "https" {
        bail!("album artwork URL must be https");
    }
    let host = parsed.host_str().unwrap_or_default().to_ascii_lowercase();
    if host != "i.scdn.co" && !host.ends_with(".scdn.co") && !host.ends_with(".spotifycdn.com") {
        bail!("album artwork host is not trusted: {host}");
    }
    let client = reqwest::Client::builder()
        .timeout(Duration::from_secs(12))
        .redirect(reqwest::redirect::Policy::none())
        .user_agent(format!("yututui/{}", env!("CARGO_PKG_VERSION")))
        .build()
        .context("build album artwork client")?;
    let mut resp = client
        .get(parsed)
        .send()
        .await
        .context("download album artwork")?;
    if resp.status().is_redirection() {
        bail!("album artwork redirects are not allowed");
    }
    resp = resp
        .error_for_status()
        .context("album artwork HTTP error")?;
    if resp
        .content_length()
        .is_some_and(|length| length > ALBUM_ART_MAX_BYTES as u64)
    {
        bail!("album artwork Content-Length exceeds the 8 MiB limit");
    }
    let mime = resp
        .headers()
        .get(reqwest::header::CONTENT_TYPE)
        .and_then(|value| value.to_str().ok())
        .unwrap_or_default()
        .to_ascii_lowercase();
    if !mime.starts_with("image/jpeg") && !mime.starts_with("image/png") {
        bail!("album artwork is not jpeg/png");
    }
    let mut bytes = Vec::with_capacity(
        resp.content_length()
            .unwrap_or_default()
            .min(ALBUM_ART_MAX_BYTES as u64) as usize,
    );
    while let Some(chunk) = resp.chunk().await.context("stream album artwork")? {
        let next = bytes
            .len()
            .checked_add(chunk.len())
            .ok_or_else(|| anyhow!("album artwork size overflow"))?;
        if next > ALBUM_ART_MAX_BYTES {
            bail!("album artwork streamed body exceeds the 8 MiB limit");
        }
        bytes.extend_from_slice(&chunk);
    }
    Ok(bytes)
}
