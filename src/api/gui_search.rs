//! Transport-neutral GUI search correlation and row identity.

use std::fmt::Write as _;

use sha2::{Digest, Sha256};

use super::{Song, sanitize_provider_id};
use crate::search_source::SearchSource;

/// GUI search row identities are accepted back as command arguments, so their canonical
/// projection must stay within the command boundary shared by every transport.
pub const GUI_SEARCH_ROW_ID_MAX_BYTES: usize = 256;

/// Opaque correlation token for one GUI search submitted to the API actor.
///
/// The daemon owns requester/session metadata. The API sees only this token, so it cannot retain
/// a socket handle. `epoch` advances when `sequence` wraps, fencing late prior-cycle answers.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct GuiSearchRequestId {
    epoch: u64,
    sequence: u64,
}

impl GuiSearchRequestId {
    pub(crate) const fn new(epoch: u64, sequence: u64) -> Self {
        Self { epoch, sequence }
    }

    #[cfg(test)]
    pub(crate) const fn parts(self) -> (u64, u64) {
        (self.epoch, self.sequence)
    }
}

/// One catalog's slice of a GUI search answer, still in [`Song`] terms.
pub struct GuiSearchGroup {
    pub source: SearchSource,
    pub songs: Vec<Song>,
    pub error: Option<String>,
}

/// Stable, command-safe identity for one GUI search row.
///
/// YouTube keeps its native id. External catalogs use an opaque digest of their complete
/// playable identity, preventing provider-id sanitization from making distinct rows collide.
pub(crate) fn gui_search_row_id(song: &Song) -> String {
    let sanitized = sanitize_provider_id(&song.video_id);
    if song.source == SearchSource::Youtube
        && sanitized == song.video_id
        && !sanitized.is_empty()
        && sanitized.len() <= GUI_SEARCH_ROW_ID_MAX_BYTES
    {
        return sanitized;
    }

    let identity = serde_json::to_vec(&(
        song.source,
        &song.video_id,
        &song.playable,
        &song.local_path,
    ))
    .expect("GUI search identity serialization is infallible");
    let digest = Sha256::digest(identity);
    let mut row_id = format!("gui:{}:", song.source.id_prefix());
    for byte in digest {
        write!(&mut row_id, "{byte:02x}").expect("writing to String is infallible");
    }
    row_id
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::api::PlayableRef;

    #[test]
    fn youtube_id_is_preserved_but_external_identity_is_opaque_and_stable() {
        let youtube = Song::remote("dQw4w9WgXcQ", "Title", "Artist", "3:00");
        assert_eq!(gui_search_row_id(&youtube), "dQw4w9WgXcQ");

        let external = Song {
            source: SearchSource::SoundCloud,
            playable: Some(PlayableRef::DirectUrl {
                source: SearchSource::SoundCloud,
                url: "https://example.invalid/audio".to_owned(),
            }),
            ..Song::remote("provider-id", "Title", "Artist", "3:00")
        };
        let first = gui_search_row_id(&external);
        assert_eq!(first, gui_search_row_id(&external));
        assert!(first.starts_with("gui:sc:"));
        assert!(first.len() <= GUI_SEARCH_ROW_ID_MAX_BYTES);
    }
}
