//! Album art / video thumbnail fetch + decode actor.
//!
//! Remote (catalog) tracks: the thumbnail is derived from the `video_id` via
//! `i.ytimg.com/vi/<id>/…` — `maxresdefault.jpg` (clean native aspect) first, falling
//! back to `hqdefault.jpg` (always present). Local tracks: the embedded cover art is read
//! straight out of the file with `lofty`. Either way we decode + downscale off the main
//! thread and hand the UI a ready [`image::DynamicImage`]; the player view turns it into a
//! terminal graphics protocol via `ratatui-image`.
//!
//! This is opt-in (config `album_art`): the reducer only emits a fetch when the feature is
//! on and a graphics protocol was detected at startup, so nothing here runs otherwise.

use std::path::PathBuf;

use image::DynamicImage;
use tokio::sync::mpsc::{self, UnboundedReceiver, UnboundedSender};

use crate::util::http;

/// Cap the decoded image to this many pixels on its longest side. The render protocol now
/// upscales (`Resize::Scale`) to fill the art area, so the source needs enough detail to
/// stay sharp when enlarged; this still bounds in-flight RAM and per-track decode/encode
/// cost. Only the current track's image is held at a time, so peak cost is one image.
/// (`maxresdefault` is natively 1280×720; 768px is enough for terminal-cell rendering while
/// keeping protocol resize/encode work smaller.)
const MAX_DIM: u32 = 768;
const REMOTE_ART_MAX_BYTES: usize = 5 * 1024 * 1024;

/// Where a track's art comes from.
pub enum ArtSource {
    /// A catalog track: fetch `i.ytimg.com/vi/<video_id>/…`.
    Remote { video_id: String },
    /// A local file: read its embedded cover art.
    Local(PathBuf),
}

pub enum ArtworkCmd {
    Fetch { video_id: String, source: ArtSource },
}

pub enum ArtworkEvent {
    Result {
        video_id: String,
        image: Option<DynamicImage>,
    },
}

pub struct ArtworkHandle {
    tx: UnboundedSender<ArtworkCmd>,
}

impl ArtworkHandle {
    pub fn fetch(&self, video_id: String, source: ArtSource) {
        let _ = self.tx.send(ArtworkCmd::Fetch { video_id, source });
    }
}

/// Spawn the artwork actor; results return as [`ArtworkEvent`]s.
pub fn spawn<F>(emit: F) -> ArtworkHandle
where
    F: Fn(ArtworkEvent) + Send + Sync + 'static,
{
    let (tx, rx) = mpsc::unbounded_channel();
    tokio::spawn(run_actor(rx, emit));
    ArtworkHandle { tx }
}

async fn run_actor<F>(mut rx: UnboundedReceiver<ArtworkCmd>, emit: F)
where
    F: Fn(ArtworkEvent) + Send + Sync + 'static,
{
    let client = reqwest::Client::builder()
        .user_agent("ytm-tui/1 (https://github.com/Ochichan/ytm-tui)")
        .build()
        .unwrap_or_else(|_| reqwest::Client::new());

    // Sequential, like the lyrics actor: rapid skips queue up and the UI drops stale
    // results by `video_id`, so a per-track image never races a later one onto the screen.
    while let Some(ArtworkCmd::Fetch { video_id, source }) = rx.recv().await {
        let bytes = match source {
            ArtSource::Remote { video_id: id } => fetch_remote(&client, &id).await,
            ArtSource::Local(path) => fetch_local(path).await,
        };
        let image = match bytes {
            Some(b) => decode_scaled(b).await,
            None => None,
        };
        tracing::info!(video_id = %video_id, found = image.is_some(), "artwork");
        emit(ArtworkEvent::Result { video_id, image });
    }
}

/// Fetch the YouTube thumbnail for `video_id`: try the clean native-aspect `maxresdefault`
/// (absent for some tracks), then the always-present 4:3 `hqdefault`.
async fn fetch_remote(client: &reqwest::Client, video_id: &str) -> Option<Vec<u8>> {
    for quality in ["maxresdefault", "hqdefault"] {
        let url = format!("https://i.ytimg.com/vi/{video_id}/{quality}.jpg");
        if let Ok(resp) = client.get(&url).send().await
            && let Ok(resp) = resp.error_for_status()
            && let Ok(bytes) = http::read_response_limited(resp, REMOTE_ART_MAX_BYTES).await
            && !bytes.is_empty()
        {
            return Some(bytes);
        }
    }
    None
}

/// Read embedded cover art from a local audio file (off-thread; lofty parses tags).
async fn fetch_local(path: PathBuf) -> Option<Vec<u8>> {
    tokio::task::spawn_blocking(move || local_cover_bytes(&path))
        .await
        .ok()
        .flatten()
}

fn local_cover_bytes(path: &std::path::Path) -> Option<Vec<u8>> {
    use lofty::file::TaggedFileExt;
    let tagged = lofty::read_from_path(path).ok()?;
    let tag = tagged.primary_tag().or_else(|| tagged.first_tag())?;
    let pic = tag.pictures().first()?;
    Some(pic.data().to_vec())
}

/// Decode the raw bytes and downscale to [`MAX_DIM`] (off-thread — decode/resize is CPU).
async fn decode_scaled(bytes: Vec<u8>) -> Option<DynamicImage> {
    tokio::task::spawn_blocking(move || {
        let img = image::load_from_memory(&bytes).ok()?;
        Some(if img.width() > MAX_DIM || img.height() > MAX_DIM {
            // `resize` preserves aspect, fitting within the box; Triangle is a good
            // quality/speed balance (the protocol re-scales again at render).
            img.resize(MAX_DIM, MAX_DIM, image::imageops::FilterType::Triangle)
        } else {
            img
        })
    })
    .await
    .ok()
    .flatten()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Cursor;
    use std::time::Duration;

    fn png_bytes(width: u32, height: u32) -> Vec<u8> {
        let img = image::RgbImage::from_pixel(width, height, image::Rgb([12, 34, 56]));
        let mut out = Cursor::new(Vec::new());
        DynamicImage::ImageRgb8(img)
            .write_to(&mut out, image::ImageFormat::Png)
            .unwrap();
        out.into_inner()
    }

    #[tokio::test]
    async fn decode_scaled_rejects_invalid_image_bytes() {
        assert!(decode_scaled(b"not an image".to_vec()).await.is_none());
    }

    #[tokio::test]
    async fn decode_scaled_keeps_small_images_at_original_size() {
        let img = decode_scaled(png_bytes(64, 32)).await.unwrap();

        assert_eq!((img.width(), img.height()), (64, 32));
    }

    #[tokio::test]
    async fn decode_scaled_downscales_large_images_to_max_dimension() {
        let img = decode_scaled(png_bytes(1024, 512)).await.unwrap();

        assert_eq!((img.width(), img.height()), (MAX_DIM, MAX_DIM / 2));
    }

    #[tokio::test]
    async fn actor_emits_none_for_missing_local_artwork() {
        let (tx, mut rx) = mpsc::unbounded_channel();
        let handle = spawn(move |event| {
            tx.send(event).unwrap();
        });

        handle.fetch(
            "missing-local".to_owned(),
            ArtSource::Local(
                std::env::temp_dir()
                    .join(format!("ytm-tui-missing-artwork-{}", std::process::id())),
            ),
        );
        drop(handle);

        let event = tokio::time::timeout(Duration::from_secs(2), rx.recv())
            .await
            .expect("actor should emit a result")
            .expect("result channel should stay open until emit");
        match event {
            ArtworkEvent::Result { video_id, image } => {
                assert_eq!(video_id, "missing-local");
                assert!(image.is_none());
            }
        }
    }
}
