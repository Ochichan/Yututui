//! Shared, hardened primitives for the two album-art actors — the TUI's [`crate::artwork`]
//! and the OS-media-session [`crate::media::artwork`]. Both consume the same untrusted
//! bytes (a YouTube thumbnail or a local file's embedded cover), so the guards against a
//! hostile/corrupt image live here once instead of drifting between two copies.

use std::path::Path;

use image::DynamicImage;

/// Max encoded size of a local embedded cover we're willing to copy out of the tag. Real
/// covers top out at a few MB; this only rejects a pathological/hostile embedded blob that
/// would spike memory the instant the user selects that file with album art enabled.
pub const LOCAL_ART_MAX_BYTES: usize = 32 * 1024 * 1024;

/// Decode-bomb ceilings for [`decode_untrusted`]. Set far above any real album art (covers
/// are at most a few thousand px on a side) so no legitimate image is ever rejected, yet a
/// tiny-file-decodes-to-enormous-canvas bomb can't force a multi-GB allocation.
const MAX_DECODE_DIM: u32 = 16_384;
const MAX_DECODE_ALLOC: u64 = 256 * 1024 * 1024;

/// Read the first embedded cover out of a local audio file (m4a/mp3/flac), rejecting an
/// absurdly large picture *before* allocating a copy of it. `None` on any parse failure, a
/// missing picture, or a picture over [`LOCAL_ART_MAX_BYTES`]. Call from a blocking task —
/// `lofty` parses tags synchronously.
pub fn local_cover_bytes(path: &Path) -> Option<Vec<u8>> {
    use lofty::file::TaggedFileExt;
    let tagged = lofty::read_from_path(path).ok()?;
    let tag = tagged.primary_tag().or_else(|| tagged.first_tag())?;
    let pic = tag.pictures().first()?;
    let data = pic.data();
    if data.len() > LOCAL_ART_MAX_BYTES {
        tracing::warn!(bytes = data.len(), "embedded artwork too large; skipping");
        return None;
    }
    Some(data.to_vec())
}

/// Decode encoded image bytes from an untrusted source (network thumbnail or embedded cover)
/// with decode-bomb limits applied. Behaviour-identical to `image::load_from_memory` for
/// every real image — the limits sit far above any album art — but a malformed/hostile
/// image that would balloon memory is rejected as `None` instead of decoded. Call from a
/// blocking task; decoding is CPU-bound.
pub fn decode_untrusted(bytes: &[u8]) -> Option<DynamicImage> {
    use std::io::Cursor;
    let mut reader = image::ImageReader::new(Cursor::new(bytes))
        .with_guessed_format()
        .ok()?;
    let limits = {
        let mut l = image::Limits::default();
        l.max_image_width = Some(MAX_DECODE_DIM);
        l.max_image_height = Some(MAX_DECODE_DIM);
        l.max_alloc = Some(MAX_DECODE_ALLOC);
        l
    };
    reader.limits(limits);
    reader.decode().ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn decodes_a_real_image() {
        // 4x2 red PNG via the image crate itself.
        let img = DynamicImage::new_rgb8(4, 2);
        let mut buf = std::io::Cursor::new(Vec::new());
        img.write_to(&mut buf, image::ImageFormat::Png).unwrap();
        let decoded = decode_untrusted(buf.get_ref()).unwrap();
        assert_eq!((decoded.width(), decoded.height()), (4, 2));
    }

    #[test]
    fn rejects_non_image_bytes() {
        assert!(decode_untrusted(b"not an image").is_none());
    }
}
