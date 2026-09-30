use super::*;

/// A clickable terminal region's semantic target.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum MouseTarget {
    /// An action row in the open TUI context menu.
    ContextMenuItem(usize),
    Atlas(crate::app::atlas::AtlasTarget),
    ToolSetupCopy,
    ToolSetupGuide,
    ToolSetupRetry,
    ToolSetupLater,
    /// A control on the Beginner Mode coach card. `Noop` seals the card body against
    /// click-through; the remaining actions are rendered as explicit buttons on top.
    Onboarding(OnboardingAction),
    Global(Action),
    Player(Action),
    /// Inert coverage for the open per-track WhyGem card; outside clicks close it while clicks
    /// inside are consumed without reaching the covered queue/player surface.
    WhyGemCard,
    StationCard,
    /// A visible synced-lyric row. The owning track ID and original LRC index make stale frame
    /// targets fail closed instead of seeking a newly loaded track.
    LyricsLine {
        video_id: Arc<str>,
        line_index: usize,
    },
    /// The collapsed `[±]` handle. Carries its rendered track ID for the same stale-frame guard.
    LyricsDelayHandle {
        video_id: Arc<str>,
    },
    /// Expanded lyric-delay buttons. Keeping the rendered track ID prevents an old frame's OSD
    /// from adjusting a newly loaded song before the next frame replaces the hit map.
    LyricsDelayEarlier {
        video_id: Arc<str>,
    },
    LyricsDelayLater {
        video_id: Arc<str>,
    },
    /// Inert coverage for the expanded lyric-delay OSD's value and spacing. Action buttons are
    /// registered after it and win hit-testing; everything else is deliberately consumed.
    LyricsDelayBlock,
    /// Open/close the EQ preset dropdown on the player status line (clicking the `EQ:` label).
    EqMenu,
    /// Pick an EQ preset from the open dropdown.
    EqSelect(EqPreset),
    /// Open/close the streaming-mode dropdown on the player status line (clicking the `streaming:` label).
    StreamingMenu,
    /// Pick a streaming mode from the open dropdown.
    StreamingSelect(StreamingMode),
    /// The player volume cluster (`vol - 50% +`). Clicks are ignored on the label/value,
    /// but wheel events over the cluster nudge volume.
    VolumeArea,
    /// A nav-bar item — switch to that screen from any screen.
    Nav(Mode),
    /// The search bar's submit button.
    SearchSubmit,
    /// The search query input box.
    SearchInput,
    /// The `⌕ Filter` button next to the search bar — opens the results-filter popup.
    SearchFilterOpen,
    /// A row in the results-filter popup, by *display* index into the filtered rows.
    /// Single-click selects; double-click plays; right-click enqueues.
    SearchFilterRow(usize),
    /// A top-songs row on the artist detail screen. Single-click selects; double-click plays.
    ArtistSongRow(usize),
    /// An albums/singles row on the artist detail screen. Single-click selects;
    /// double-click plays the album.
    ArtistAlbumRow(usize),
    /// Open/close the search-source dropdown.
    SearchSourceMenu,
    /// Pick a source from the search-source dropdown.
    SearchSourceSelect(SearchSource),
    /// A Library tab header.
    LibraryTab(LibraryTab),
    /// Switch between the local YuTuTui library and the configured music-server library.
    LibrarySource(LibrarySource),
    /// One of the five read-only music-server library sections.
    ServerLibrarySection(crate::open_subsonic::ServerLibrarySection),
    /// A generation-stamped server row. Old frames fail closed after paging or drill-down.
    ServerLibraryRow {
        generation: u64,
        index: usize,
    },
    ServerLibraryBack {
        generation: u64,
    },
    ServerLibraryPreviousPage {
        generation: u64,
    },
    ServerLibraryNextPage {
        generation: u64,
    },
    /// A Local Deck sidebar section, by index into [`LocalSection::ALL`].
    LocalNav(usize),
    /// A row in the Local Deck list, by display index.
    LocalRow(usize),
    /// A generation-stamped Local Find result/drill row. Old frames cannot redirect actions.
    LocalFindRow {
        index: usize,
        stamp: LocalFindPointerStamp,
    },
    LocalFindInput,
    LocalFindSubmit,
    LocalFindRefineOpen,
    LocalFindRefineRow(usize),
    LocalFindLaunchpad {
        index: usize,
        stamp: LocalFindPointerStamp,
    },
    /// A Local Find scrollbar is separate from generic scrollbars so a delayed press or drag
    /// cannot move a newer query, corpus, or drill view.
    LocalFindScrollbar {
        stamp: LocalFindPointerStamp,
    },
    ConfirmLocalFindBulk,
    CancelLocalFindBulk,
    ConfirmLocalFindRebuild,
    CancelLocalFindRebuild,
    /// The trailing `✗` on a saved Local Deck import-session row. Carries the exact persisted
    /// job id so a re-sort between render and click can never redirect the destructive action.
    LocalImportDel(String),
    /// The footer mouse-help icon. Mouse-only: no keybinding maps to this overlay.
    MouseHelp,
    /// A Settings tab header, by index into [`SettingsTab::ALL`].
    SettingsTab(usize),
    /// An action or detail row in the privacy-safe Sync settings projection. Clicking selects
    /// the row and delegates to the same action path as Enter; informational rows safely no-op.
    SettingsSyncRow(usize),
    /// One of the four plain-language areas inside the top-level Sync settings tab.
    SettingsSyncArea(SyncArea),
    /// A redacted music-server settings action row.
    SettingsMusicServerRow(usize),
    /// A field/action in the move-only music-server setup wizard.
    MusicServerWizardField(usize),
    MusicServerWizardPrimary,
    MusicServerWizardSecondary,
    MusicServerWizardReveal,
    /// A field in the move-only Sync setup/join/recovery wizard.
    SyncWizardField(usize),
    /// The wizard's affirmative action (continue, approve, merge, remove, or finish).
    SyncWizardPrimary,
    /// The wizard's non-affirmative action (back, cancel, or reject).
    SyncWizardSecondary,
    /// Reveal or mask the currently focused secret field. The target carries no secret value.
    SyncWizardReveal,
    /// A clickable value control on a Settings field row — the checkbox of a toggle or the
    /// `<` / `>` arrow of a Select/Slider. Carries the field-row index and the nudge direction,
    /// so a click is the mouse equivalent of ←/→ on that row.
    SettingsChange {
        row: usize,
        delta: i32,
    },
    /// A clickable Settings button or text value, by field-row index — enters edit mode (text)
    /// or fires the action (button); the mouse equivalent of Enter on that row.
    SettingsActivate(usize),
    /// The two-cell color swatch on a Settings theme row — opens the full color picker.
    SettingsColorSwatch(usize),
    /// Modal picker backdrop/chrome. It captures clicks inside the popup that are not choices.
    SettingsColorPickerSurface,
    /// The lossless current-value row in the modal color picker.
    SettingsColorPickerCurrent,
    /// A picker-grid choice: transparent at zero, then the 240 xterm colors.
    SettingsColorPickerChoice(usize),
    /// Open/close the Settings Spotify import-mode dropdown.
    SettingsSpotifyImportModeMenu,
    /// Pick a Settings Spotify import-mode dropdown option.
    SettingsSpotifyImportModeSelect(crate::config::SpotifyImportMode),
    /// A row in the Settings audio-output picker.
    AudioOutputRow(usize),
    /// A list row, by absolute item index (interpreted per the active screen). Single-click
    /// selects; double-click plays.
    ListRow(usize),
    /// A vertical list scrollbar track/thumb. Clicking or dragging maps the pointer row to
    /// the matching viewport offset for the owning scroll state.
    Scrollbar(ScrollSurface),
    /// A rendered visual row in the DJ Gem transcript, after wrapping. Dragging across these
    /// rows copies the selected chat text.
    AiTranscriptRow(usize),
    /// The DJ Gem prompt input box.
    AiInput,
    /// The DJ Gem prompt submit button.
    AiSubmit,
    /// The DJ Gem model label under the prompt — cycles the active model.
    AiModel,
    /// A pickable DJ Gem suggestion row.
    AiSuggestionRow(usize),
    /// The `N/M` queue-position label on the player status line — opens the queue window.
    QueuePos,
    /// A row in the open queue window, by order position. Single-click selects; double-click
    /// jumps playback to it.
    QueueRow(usize),
    /// The per-track WhyGem affordance on a queue-window row.
    QueueWhyGem(usize),
    /// The `✗` delete button on a queue-window row, by order position.
    QueueDel(usize),
    /// The `✗` delete button on a Library list row, by row index in the current tab.
    LibraryDel(usize),
    /// The breadcrumb of an opened playlist (Playlists tab drill-down) — returns to the
    /// playlist list.
    PlaylistBack,
    /// Confirm button on the "delete playlist" modal.
    ConfirmPlaylistDelete,
    /// Cancel button on the "delete playlist" modal.
    CancelPlaylistDelete,
    /// Create button on the "new playlist" popup.
    ConfirmPlaylistCreate,
    /// Cancel button on the "new playlist" popup.
    CancelPlaylistCreate,
    /// Set / Cancel buttons on the sleep-timer popup.
    ConfirmSleepTimer,
    CancelSleepTimer,
    /// A row in the "add to playlist" picker: `0..len` choose a playlist, `len` is the
    /// trailing "New playlist…" row.
    PlaylistPickRow(usize),
    /// Create button on the picker's inline new-playlist name entry.
    ConfirmPickerCreate,
    /// Back button on the picker's inline new-playlist name entry (returns to the list).
    CancelPickerCreate,
    /// A row in the "Import from Spotify" picker, by item index. Single-click selects;
    /// clicking the already-selected row (or double-click) starts the import.
    SpotifyPickRow(usize),
    /// Confirm button on the "delete downloaded files" modal.
    ConfirmDelete,
    /// Cancel button on the "delete downloaded files" modal.
    CancelDelete,
    /// Confirm button on the bulk "download N songs" modal.
    ConfirmDownload,
    /// Cancel button on the bulk "download N songs" modal.
    CancelDownload,
    /// Confirm button on a Settings confirmation modal.
    ConfirmSettings,
    /// Cancel button on a Settings confirmation modal.
    CancelSettings,
    /// Apply button on a prepared server-playlist import/link preview.
    ConfirmServerPlaylistPreview,
    /// Back button on a server-playlist import/link preview.
    CancelServerPlaylistPreview,
    /// Create-and-link button on the local-to-server playlist confirmation.
    ConfirmServerPlaylistCreate,
    /// Back button on the local-to-server playlist confirmation.
    CancelServerPlaylistCreate,
    /// Confirm a destructive linked-playlist recovery action.
    ConfirmServerPlaylistRecovery,
    /// Back out of a linked-playlist recovery confirmation.
    CancelServerPlaylistRecovery,
    /// Confirm button on the radio-mode confirmation modal.
    ConfirmRadioMode,
    /// Cancel button on the radio-mode confirmation modal.
    CancelRadioMode,
    /// Confirm button on the local-player confirmation modal.
    ConfirmLocalMode,
    /// Cancel button on the local-player confirmation modal.
    CancelLocalMode,
    /// Confirm button on the local import organize modal.
    ConfirmLocalOrganize,
    /// Cancel button on the local import organize modal.
    CancelLocalOrganize,
    /// Confirm button on the local import accept-all modal.
    ConfirmLocalAcceptAll,
    /// Cancel button on the local import accept-all modal.
    CancelLocalAcceptAll,
    /// Buttons on the local import-history delete confirmation.
    ConfirmLocalImportDelete,
    CancelLocalImportDelete,
    /// "Save to favorites" on the "what's playing" overlay (resolves a real YT track first).
    NowPlayingFavorite,
    /// "Tell me more" on the "what's playing" overlay — hands off to the DJ Gem view.
    NowPlayingAskAi,
    /// Close button on the "what's playing" overlay.
    CloseNowPlaying,
    /// The `yututui` brand label at the top-left of the nav bar — opens the About card.
    AboutTitle,
    /// The GitHub link inside the About card — opens the repo in the system browser.
    AboutLink,
    /// The "Releases" link inside the About card's update notice — opens the latest release
    /// page in the system browser (only present when an update is available).
    AboutUpdateLink,
    /// A row in the radio-recording settings popup, by row index (`0..7`). Clicking focuses the
    /// row; for the folder (edit) and browse (open list) rows it also activates them — the mouse
    /// equivalent of moving there and pressing Enter. Value rows (mode / sliders / notify) only
    /// focus here; their arrows and track publish [`MouseTarget::RecordingChange`] /
    /// [`MouseTarget::RecordingSlider`] rects on top so a bare row click never changes a value.
    RecordingRow(usize),
    /// A `‹`/`›` arrow (or the mode `< >`, or the notify `[x]`) on a radio-recording popup row —
    /// carries the row index and the nudge direction, so a click is the mouse equivalent of
    /// ←/→ on that row.
    RecordingChange {
        row: usize,
        delta: i32,
    },
    /// The draggable bar track of a numeric radio-recording row (min / max / keep-recent). A
    /// press maps pointer-x to a value and arms a drag that keeps mapping as the pointer moves,
    /// exactly like the player seekbar.
    RecordingSlider(usize),
    /// A row in the radio-recordings browser, by item index. Single-click selects the row.
    RecordingBrowseRow(usize),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MouseButtonRegion {
    pub rect: Rect,
    pub target: MouseTarget,
}
