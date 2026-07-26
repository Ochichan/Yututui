//! App-owned music-server domain state and result bucket.

use super::{MusicServerEvent, MusicServerSettingsState, ServerLibraryEvent, ServerLibraryState};

/// All OpenSubsonic UI state owned by the primary reducer.
#[derive(Default)]
pub struct ServerUiState {
    /// Browsing plus explicit deletion-free playlist import/link previews.
    pub library: ServerLibraryState,
    /// Redacted connection status plus the move-only setup wizard.
    pub settings: MusicServerSettingsState,
}

/// Results from bounded music-server workers.
pub enum ServerEvent {
    Settings(MusicServerEvent),
    Library(ServerLibraryEvent),
}
