use serde::{Deserialize, Serialize};

use crate::t;

/// Source/detail level for remote album art shown inside the terminal. `High` preserves the
/// historical max-resolution preference and 768px cap; `Original` keeps the fetched source
/// dimensions intact. Persisted in `config.json`.
#[derive(Debug, Clone, Copy, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum AlbumArtQuality {
    Standard,
    #[default]
    High,
    Original,
}

impl AlbumArtQuality {
    /// Step through `Standard → High → Original`, wrapping in either direction.
    pub fn cycled(self, forward: bool) -> Self {
        match (self, forward) {
            (Self::Standard, true) => Self::High,
            (Self::High, true) => Self::Original,
            (Self::Original, true) => Self::Standard,
            (Self::Standard, false) => Self::Original,
            (Self::High, false) => Self::Standard,
            (Self::Original, false) => Self::High,
        }
    }

    /// Short human label for the Playback settings row.
    pub fn label(self) -> &'static str {
        match self {
            Self::Standard => t!("Standard · up to 640 px", "표준 · 최대 640 px"),
            Self::High => t!("High · up to 768 px", "고화질 · 최대 768 px"),
            Self::Original => t!("Original source", "원본 화질"),
        }
    }
}

/// Window layout for the external mpv video overlay launched from the player (`v`), cycled
/// live with `Shift+V` and chosen as the open default in Settings. `Compact` docks a small
/// ~30% window top-right; `Large` centers a ~50% window; `Fullscreen` fills the screen.
/// Persisted in `config.json`.
#[derive(Debug, Clone, Copy, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum VideoOverlay {
    #[default]
    Compact,
    Large,
    Fullscreen,
}

impl VideoOverlay {
    /// Step to the next/previous layout in the `Compact → Large → Fullscreen` cycle.
    pub fn cycled(self, forward: bool) -> Self {
        match (self, forward) {
            (Self::Compact, true) => Self::Large,
            (Self::Large, true) => Self::Fullscreen,
            (Self::Fullscreen, true) => Self::Compact,
            (Self::Compact, false) => Self::Fullscreen,
            (Self::Large, false) => Self::Compact,
            (Self::Fullscreen, false) => Self::Large,
        }
    }

    /// The next layout (for the forward-cycling `Shift+V` toggle).
    pub fn toggled(self) -> Self {
        self.cycled(true)
    }

    /// Short human label for the status toast.
    pub fn label(self) -> &'static str {
        match self {
            Self::Compact => t!("top-right · 30%", "우상단 · 30%"),
            Self::Large => t!("center · 50%", "가운데 · 50%"),
            Self::Fullscreen => t!("fullscreen", "전체화면"),
        }
    }

    /// mpv flags for the overlay window. `Compact` docks a borderless top-right ~30% window;
    /// `Large` a borderless centered ~50% window; `Fullscreen` fills the screen (borderless/
    /// on-top/autofit are meaningless there, so they're dropped).
    pub fn mpv_window_args(self) -> Vec<String> {
        match self {
            Self::Compact => vec![
                "--ontop".to_owned(),
                "--no-border".to_owned(),
                "--autofit=30%".to_owned(),
                "--geometry=-20+20".to_owned(),
            ],
            Self::Large => vec![
                "--ontop".to_owned(),
                "--no-border".to_owned(),
                "--autofit=50%".to_owned(),
            ],
            Self::Fullscreen => vec!["--fullscreen".to_owned()],
        }
    }
}

/// Where the player control block (title / seekbar / transport / status) sits. `Top` is the
/// legacy layout: the block heads the Player view and other screens carry no player chrome.
/// `Bottom` (the default) docks the block above the footer on every screen, and the Player
/// view centers its filler in the space above. Persisted in `config.json`.
#[derive(Debug, Clone, Copy, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum PlayerBarPosition {
    Top,
    #[default]
    Bottom,
}

impl PlayerBarPosition {
    /// The other position (for the Settings `< >` cycle — two states, so both directions agree).
    pub fn toggled(self) -> Self {
        match self {
            Self::Top => Self::Bottom,
            Self::Bottom => Self::Top,
        }
    }

    /// Short human label for the Settings row.
    pub fn label(self) -> &'static str {
        match self {
            Self::Top => t!("Top (classic)", "상단 (클래식)"),
            Self::Bottom => t!("Bottom (docked)", "하단 (도킹)"),
        }
    }
}
