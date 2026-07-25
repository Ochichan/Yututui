//! App-owned music-server domain state and result bucket.

use super::{MusicServerEvent, MusicServerSettingsState, ServerLibraryEvent, ServerLibraryState};

/// All OpenSubsonic UI state owned by the primary reducer.
#[derive(Default)]
pub struct ServerUiState {
    /// Read-only source, paging, and drill-down state.
    pub library: ServerLibraryState,
    /// Redacted connection status plus the move-only setup wizard.
    pub settings: MusicServerSettingsState,
}

/// Results from bounded music-server workers.
pub enum ServerEvent {
    Settings(MusicServerEvent),
    Library(ServerLibraryEvent),
}
