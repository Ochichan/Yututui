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
}

impl SpotifyImportMode {
    pub const ALL: [Self; 3] = [Self::FastPlaylist, Self::StrictPlaylist, Self::ReviewFirst];

    pub fn label(self) -> &'static str {
        match self {
            Self::FastPlaylist => "Fast playlist",
            Self::StrictPlaylist => "Strict playlist",
            Self::ReviewFirst => "Review first",
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
