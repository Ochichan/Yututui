//! Redacted music-server settings summary and UI session state.

use crate::open_subsonic::PlaylistCreateAttention;

use super::{
    MusicServerBusy, MusicServerCredentialMode, MusicServerFailure, MusicServerWizard, SyncArea,
};

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum MusicServerHealth {
    #[default]
    Off,
    UpToDate,
    NeedsAttention,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum MusicServerHistoryHealth {
    #[default]
    Off,
    Probing,
    Detailed,
    PlayCountsOnly,
    UpdatePassword,
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct MusicServerSummary {
    pub health: MusicServerHealth,
    pub configured: bool,
    pub display_name: Option<String>,
    pub credential_kind: Option<MusicServerCredentialMode>,
    pub lan_http: bool,
    pub custom_ca: bool,
    pub history: MusicServerHistoryHealth,
    /// Ambiguously delivered playback reports awaiting an explicit user decision.
    pub playback_reports_needing_decision: usize,
    /// Replay-unsafe server playlist creates awaiting an explicit keep/abandon decision.
    pub playlist_creates_needing_decision: usize,
    /// Redacted local identities for each replay-unsafe server playlist create.
    pub playlist_create_attention: Vec<PlaylistCreateAttention>,
    /// Linked playlists deleted on the server and awaiting a keep/restore decision.
    pub playlist_links_needing_decision: usize,
    /// Linked playlist writes that need a successful reconnect before they can safely retry.
    pub playlist_projections_needing_decision: usize,
    /// Linked local playlists containing tracks outside the exact connected server account.
    pub playlist_contents_needing_decision: usize,
}

impl MusicServerSummary {
    pub fn display_name(&self) -> &str {
        self.display_name
            .as_deref()
            .unwrap_or_else(|| crate::t!("Music server", "음악 서버", "音楽サーバー"))
    }
}

/// Settings/UI state. It intentionally has no `Debug`/`Clone` because `wizard` may own secrets.
pub struct MusicServerSettingsState {
    pub area: SyncArea,
    pub selected: usize,
    pub generation: u64,
    pub busy: Option<MusicServerBusy>,
    pub summary: MusicServerSummary,
    pub failure: Option<MusicServerFailure>,
    pub wizard: Option<MusicServerWizard>,
}

impl Default for MusicServerSettingsState {
    fn default() -> Self {
        Self {
            area: SyncArea::Status,
            selected: 0,
            generation: 0,
            busy: None,
            summary: MusicServerSummary::default(),
            failure: None,
            wizard: None,
        }
    }
}

impl MusicServerSettingsState {
    pub fn row_count(&self) -> usize {
        if self.summary.configured {
            4 + usize::from(!self.summary.playlist_create_attention.is_empty())
        } else {
            2
        }
    }

    pub fn modal_open(&self) -> bool {
        self.wizard.is_some()
    }

    pub(in crate::app) fn next_generation(&mut self) -> u64 {
        self.generation = self.generation.wrapping_add(1).max(1);
        self.generation
    }
}
