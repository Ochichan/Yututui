use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SpotifyImportMode {
    /// Writes strict matches plus safe best ambiguous candidates to a local Library playlist.
    #[default]
    FastPlaylist,
    /// Writes only confident matches; ambiguous/missing rows remain for Local Deck review.
    StrictPlaylist,
    /// Matches only. Local Deck `A` writes matched/accepted rows later.
    ReviewFirst,
    /// Creates a separate local Library playlist from official music-video matches.
    MusicVideoPlaylist,
}

impl SpotifyImportMode {
    pub const ALL: [Self; 4] = [
        Self::FastPlaylist,
        Self::StrictPlaylist,
        Self::ReviewFirst,
        Self::MusicVideoPlaylist,
    ];

    pub fn label(self) -> &'static str {
        match self {
            Self::FastPlaylist => "Fast playlist",
            Self::StrictPlaylist => "Strict playlist",
            Self::ReviewFirst => "Review first",
            Self::MusicVideoPlaylist => "Music video playlist",
        }
    }

    pub fn description(self) -> &'static str {
        match self {
            Self::FastPlaylist => {
                "writes strict matches plus safe best review candidates to Library playlists"
            }
            Self::StrictPlaylist => {
                "writes only confident matches; review ambiguous rows in Local Deck"
            }
            Self::ReviewFirst => "match-only dry run; write later from Local Deck with A",
            Self::MusicVideoPlaylist => {
                "creates a separate Library playlist from official music-video matches"
            }
        }
    }

    pub fn cycled(self, forward: bool) -> Self {
        let idx = Self::ALL.iter().position(|mode| *mode == self).unwrap_or(0);
        let len = Self::ALL.len();
        let next = if forward {
            (idx + 1) % len
        } else {
            (idx + len - 1) % len
        };
        Self::ALL[next]
    }

    pub fn index(self) -> usize {
        Self::ALL.iter().position(|mode| *mode == self).unwrap_or(0)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn legacy_default_stays_fast_playlist() {
        assert_eq!(
            SpotifyImportMode::default(),
            SpotifyImportMode::FastPlaylist
        );
        let config: crate::config::SpotifyConfig =
            serde_json::from_str(r#"{"client_id":"legacy-app"}"#).unwrap();
        assert_eq!(config.import_mode, SpotifyImportMode::FastPlaylist);
    }

    #[test]
    fn music_video_playlist_has_stable_persisted_name() {
        let value = serde_json::to_string(&SpotifyImportMode::MusicVideoPlaylist).unwrap();
        assert_eq!(value, r#""music_video_playlist""#);
        assert_eq!(
            serde_json::from_str::<SpotifyImportMode>(&value).unwrap(),
            SpotifyImportMode::MusicVideoPlaylist
        );
        assert_eq!(
            SpotifyImportMode::MusicVideoPlaylist.label(),
            "Music video playlist"
        );
        assert!(
            SpotifyImportMode::MusicVideoPlaylist
                .description()
                .contains("official music-video")
        );
    }

    #[test]
    fn cycling_includes_music_video_playlist_and_wraps() {
        assert_eq!(
            SpotifyImportMode::ReviewFirst.cycled(true),
            SpotifyImportMode::MusicVideoPlaylist
        );
        assert_eq!(
            SpotifyImportMode::MusicVideoPlaylist.cycled(true),
            SpotifyImportMode::FastPlaylist
        );
        assert_eq!(
            SpotifyImportMode::FastPlaylist.cycled(false),
            SpotifyImportMode::MusicVideoPlaylist
        );
    }
}
