//! Album art / video thumbnail fetch + decode actor.
//!
//! Remote (catalog) tracks: the thumbnail is derived from the `video_id` via
//! `i.ytimg.com/vi/<id>/…`; the Playback setting chooses a bandwidth/detail tier from
//! `sddefault.jpg` through the untouched `maxresdefault.jpg` source. Local tracks read embedded
//! cover art with `lofty` and retain the historical 768px cap. Either way we decode + optionally
//! downscale off the main thread and hand the UI a ready [`image::DynamicImage`]; the player view
//! turns it into a terminal graphics protocol via `ratatui-image`.
//!
//! This is opt-in (config `album_art`): the reducer only emits a fetch when the feature is
//! on and a graphics protocol was detected at startup, so nothing here runs otherwise.

use std::path::PathBuf;

use image::DynamicImage;
use tokio::sync::watch;

use crate::config::AlbumArtQuality;
use crate::util::delivery::{DeliveryError, DeliveryReceipt, DeliveryResult};
use crate::util::http;

/// The default/high tier and local-cover cap. This is the historical fixed limit, so existing
/// configs keep identical detail and memory bounds after the quality setting is introduced.
const HIGH_MAX_DIM: u32 = 768;
const STANDARD_MAX_DIM: u32 = 640;
const REMOTE_ART_MAX_BYTES: usize = 5 * 1024 * 1024;

/// Where a track's art comes from.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ArtSource {
    /// A catalog track: fetch `i.ytimg.com/vi/<video_id>/…`.
    Remote {
        video_id: String,
        quality: AlbumArtQuality,
    },
    /// A local file: read its embedded cover art.
    Local(PathBuf),
}

#[derive(Clone)]
pub enum ArtworkCmd {
    Fetch { video_id: String, source: ArtSource },
}

pub enum ArtworkEvent {
    Result {
        video_id: String,
        /// `Some` for remote art, so a same-track result from the previous quality can be dropped.
        quality: Option<AlbumArtQuality>,
        image: Option<DynamicImage>,
    },
}

pub struct ArtworkHandle {
    tx: watch::Sender<Option<ArtworkCmd>>,
}

impl ArtworkHandle {
    pub fn fetch(&self, video_id: String, source: ArtSource) -> DeliveryResult {
        self.tx
            .send(Some(ArtworkCmd::Fetch { video_id, source }))
            .map(|()| DeliveryReceipt::Enqueued)
            .map_err(|_| DeliveryError::Closed)
    }
}

/// Spawn the artwork actor; results return as [`ArtworkEvent`]s.
pub fn spawn<F>(emit: F) -> ArtworkHandle
where
    F: Fn(ArtworkEvent) + Send + Sync + 'static,
{
    let (tx, rx) = watch::channel(None);
    tokio::spawn(run_actor(rx, emit));
    ArtworkHandle { tx }
}

async fn run_actor<F>(mut rx: watch::Receiver<Option<ArtworkCmd>>, emit: F)
where
    F: Fn(ArtworkEvent) + Send + Sync + 'static,
{
    let client = match http::build_no_redirect_client(
        "yututui/1 (https://github.com/Ochichan/Yututui)",
        // Bound connect/response time so a hung thumbnail host can't stall the actor.
        std::time::Duration::from_secs(8),
    ) {
        Ok(client) => Some(client),
        Err(error) => {
            tracing::warn!(%error, "remote artwork disabled: could not build bounded HTTP client");
            None
        }
    };

    // Sequential + latest-only: rapid track-skips replace the pending watch value, so the
    // actor pays network+decode only for the newest unseen request after any in-flight work
    // completes. The UI still drops late results by `video_id` as a backstop.
    while rx.changed().await.is_ok() {
        let Some(cmd) = rx.borrow_and_update().clone() else {
            continue;
        };
        let ArtworkCmd::Fetch { video_id, source } = cmd;
        let (bytes, max_dimension, quality) = match source {
            ArtSource::Remote {
                video_id: id,
                quality,
            } => (
                match client.as_ref() {
                    Some(client) => fetch_remote(client, &id, quality).await,
                    None => None,
                },
                remote_max_dimension(quality),
                Some(quality),
            ),
            ArtSource::Local(path) => (fetch_local(path).await, Some(HIGH_MAX_DIM), None),
        };
        let image = match bytes {
            Some(b) => decode_scaled(b, max_dimension).await,
            None => None,
        };
        tracing::info!(video_id = %video_id, found = image.is_some(), "artwork");
        emit(ArtworkEvent::Result {
            video_id,
            quality,
            image,
        });
    }
}

fn remote_candidates(quality: AlbumArtQuality) -> &'static [&'static str] {
    match quality {
        AlbumArtQuality::Standard => &["sddefault", "hqdefault"],
        AlbumArtQuality::High | AlbumArtQuality::Original => {
            &["maxresdefault", "sddefault", "hqdefault"]
        }
    }
}

fn remote_max_dimension(quality: AlbumArtQuality) -> Option<u32> {
    match quality {
        AlbumArtQuality::Standard => Some(STANDARD_MAX_DIM),
        AlbumArtQuality::High => Some(HIGH_MAX_DIM),
        AlbumArtQuality::Original => None,
    }
}

/// Fetch the YouTube thumbnail for `video_id` at the selected tier, with progressively more
/// compatible fallbacks for videos that do not publish the preferred rendition.
async fn fetch_remote(
    client: &reqwest::Client,
    video_id: &str,
    quality: AlbumArtQuality,
) -> Option<Vec<u8>> {
    for candidate in remote_candidates(quality) {
        let url = format!("https://i.ytimg.com/vi/{video_id}/{candidate}.jpg");
        if let Ok(resp) = client.get(&url).send().await
            && !resp.status().is_redirection()
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
    crate::util::blocking::spawn_io(move || local_cover_bytes(&path))
        .await
        .ok()
        .flatten()
}

fn local_cover_bytes(path: &std::path::Path) -> Option<Vec<u8>> {
    // Shared with the OS-media-session art cache; caps the embedded-cover size before copy.
    crate::util::art::local_cover_bytes(path)
}

/// Decode the raw bytes and optionally cap the longest side (off-thread — decode/resize is CPU).
async fn decode_scaled(bytes: Vec<u8>, max_dimension: Option<u32>) -> Option<DynamicImage> {
    crate::util::blocking::spawn_cpu(move || {
        // Decode-bomb-guarded decode (shared): a hostile/corrupt image can't spike RAM.
        let img = crate::util::art::decode_untrusted(&bytes)?;
        Some(match max_dimension {
            Some(max) if img.width() > max || img.height() > max => {
                // `resize` preserves aspect, fitting within the box; Triangle is a good
                // quality/speed balance (the protocol re-scales again at render).
                img.resize(max, max, image::imageops::FilterType::Triangle)
            }
            _ => img,
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

    #[tokio::test]
    async fn watch_channel_coalesces_a_burst_to_the_newest() {
        let (tx, mut rx) = watch::channel(None);
        for i in 0..5 {
            let quality = if i == 4 {
                AlbumArtQuality::Original
            } else {
                AlbumArtQuality::Standard
            };
            tx.send(Some(ArtworkCmd::Fetch {
                video_id: format!("v{i}"),
                source: ArtSource::Remote {
                    video_id: format!("v{i}"),
                    quality,
                },
            }))
            .unwrap();
        }
        rx.changed().await.unwrap();
        let ArtworkCmd::Fetch { video_id, source } =
            rx.borrow_and_update().clone().expect("latest request");
        assert_eq!(
            video_id, "v4",
            "a burst of skips collapses to the current track"
        );
        assert!(matches!(
            source,
            ArtSource::Remote {
                quality: AlbumArtQuality::Original,
                ..
            }
        ));
        assert!(
            !rx.has_changed().expect("sender is still open"),
            "the backlog is fully coalesced"
        );
    }

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
        assert!(
            decode_scaled(b"not an image".to_vec(), Some(HIGH_MAX_DIM))
                .await
                .is_none()
        );
    }

    #[tokio::test]
    async fn decode_scaled_keeps_small_images_at_original_size() {
        let img = decode_scaled(png_bytes(64, 32), Some(HIGH_MAX_DIM))
            .await
            .unwrap();

        assert_eq!((img.width(), img.height()), (64, 32));
    }

    #[tokio::test]
    async fn decode_scaled_downscales_large_images_to_selected_dimension() {
        let img = decode_scaled(png_bytes(1024, 512), Some(HIGH_MAX_DIM))
            .await
            .unwrap();

        assert_eq!(
            (img.width(), img.height()),
            (HIGH_MAX_DIM, HIGH_MAX_DIM / 2)
        );
    }

    #[tokio::test]
    async fn standard_quality_caps_the_longest_side_at_640px() {
        let img = decode_scaled(png_bytes(960, 480), Some(STANDARD_MAX_DIM))
            .await
            .unwrap();

        assert_eq!((img.width(), img.height()), (640, 320));
    }

    #[tokio::test]
    async fn original_quality_preserves_source_dimensions() {
        let img = decode_scaled(png_bytes(1024, 512), None).await.unwrap();

        assert_eq!((img.width(), img.height()), (1024, 512));
    }

    #[test]
    fn remote_quality_tiers_choose_expected_sources_and_caps() {
        assert_eq!(
            remote_candidates(AlbumArtQuality::Standard),
            ["sddefault", "hqdefault"]
        );
        assert_eq!(
            remote_candidates(AlbumArtQuality::High),
            ["maxresdefault", "sddefault", "hqdefault"]
        );
        assert_eq!(
            remote_candidates(AlbumArtQuality::Original),
            ["maxresdefault", "sddefault", "hqdefault"]
        );
        assert_eq!(remote_max_dimension(AlbumArtQuality::Standard), Some(640));
        assert_eq!(remote_max_dimension(AlbumArtQuality::High), Some(768));
        assert_eq!(remote_max_dimension(AlbumArtQuality::Original), None);
    }

    #[tokio::test]
    async fn actor_emits_none_for_missing_local_artwork() {
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
        let handle = spawn(move |event| {
            tx.send(event).unwrap();
        });

        assert!(
            handle
                .fetch(
                    "missing-local".to_owned(),
                    ArtSource::Local(
                        std::env::temp_dir()
                            .join(format!("yututui-missing-artwork-{}", std::process::id())),
                    ),
                )
                .is_ok()
        );
        drop(handle);

        let event = tokio::time::timeout(Duration::from_secs(2), rx.recv())
            .await
            .expect("actor should emit a result")
            .expect("result channel should stay open until emit");
        match event {
            ArtworkEvent::Result {
                video_id,
                quality,
                image,
            } => {
                assert_eq!(video_id, "missing-local");
                assert_eq!(quality, None);
                assert!(image.is_none());
            }
        }
    }
}
