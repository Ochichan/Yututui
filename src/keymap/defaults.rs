use crossterm::event::{KeyCode, KeyModifiers};

use super::{Action, Chord, KeyContext, chord_to_config};

/// The built-in default bindings, ordered by context (which also drives the cheat-sheet /
/// editor grouping). Mirrors the keys the app shipped with before remapping existed.
pub fn default_bindings() -> Vec<(KeyContext, Action, Chord)> {
    use Action as A;
    use KeyContext as C;
    let key = |code| Chord::new(code, KeyModifiers::empty());
    let ch = |c| Chord::new(KeyCode::Char(c), KeyModifiers::empty());
    let ctrl = |c| Chord::new(KeyCode::Char(c), KeyModifiers::CONTROL);
    let alt_shift = |c| Chord::new(KeyCode::Char(c), KeyModifiers::ALT | KeyModifiers::SHIFT);
    // Shift + a non-`Char` key (arrows / Page / Home / End). `Chord::new` preserves Shift
    // for these, so `Shift+Up` stays distinct from `Up` and can bind range-select.
    let shift = |code| Chord::new(code, KeyModifiers::SHIFT);
    vec![
        // Player (the main screen; self-contained transport + screen switches).
        (C::Player, A::TogglePause, ch(' ')),
        (C::Player, A::ToggleRadioMode, alt_shift('r')),
        (C::Player, A::ToggleRecordings, alt_shift('e')),
        (C::Player, A::SeekBack, key(KeyCode::Left)),
        (C::Player, A::SeekForward, key(KeyCode::Right)),
        (C::Player, A::VolUp, key(KeyCode::Up)),
        (C::Player, A::VolDown, key(KeyCode::Down)),
        (C::Player, A::ToggleMute, ch('m')),
        // mpv-style transport: `,`/`.` skip tracks (mpv's `<`/`>`), since a music player has
        // no use for mpv's frame-step on `,`/`.`.
        (C::Player, A::PrevTrack, ch(',')),
        (C::Player, A::NextTrack, ch('.')),
        (C::Player, A::CycleRating, ch('f')),
        (C::Player, A::OpenLibrary, ch('l')),
        (C::Player, A::OpenQueue, ch('c')),
        (C::Player, A::QueueRemove, key(KeyCode::Delete)),
        (C::Player, A::ToggleLyrics, ch('L')),
        (C::Player, A::LyricsDelayEarlier, ch('z')),
        (C::Player, A::LyricsDelayLater, ch('Z')),
        (C::Player, A::Download, ch('d')),
        (C::Player, A::ToggleShuffle, ch('x')),
        (C::Player, A::CycleRepeat, ch('r')),
        (C::Player, A::IdentifyNowPlaying, ch('i')),
        (C::Player, A::CycleEq, ch('e')),
        (C::Player, A::ToggleNormalize, ch('N')),
        // Playback speed on `[`/`]` to match mpv (frees `<`/`>`).
        (C::Player, A::SpeedUp, ch(']')),
        (C::Player, A::SpeedDown, ch('[')),
        (C::Player, A::OpenSettings, ch('o')),
        (C::Player, A::OpenAi, ch('g')),
        (C::Player, A::OpenSearch, ch('s')),
        (C::Player, A::AddToPlaylist, ch('P')),
        (C::Player, A::CopyLink, ch('y')),
        (C::Player, A::PlayVideo, ch('v')),
        (C::Player, A::ToggleVideoLayout, ch('V')),
        (C::Player, A::Back, ch('q')),
        // The "what's playing" card's own actions (modal; `i`/Esc/Enter close it). `f`/`g`
        // deliberately mirror the player's favorite / DJ Gem keys.
        (C::NowPlaying, A::NowPlayingFavorite, ch('f')),
        (C::NowPlaying, A::NowPlayingAskAi, ch('g')),
        // External mpv video window controls. These are installed into mpv on the next
        // overlay open; compatibility aliases (`<`, `>`, `p`) stay fixed in video.rs.
        (C::MpvOverlay, A::VideoTogglePause, ch(' ')),
        (C::MpvOverlay, A::VideoNext, ch('.')),
        (C::MpvOverlay, A::VideoPrev, ch(',')),
        (C::MpvOverlay, A::VideoClose, ch('q')),
        (C::MpvOverlay, A::VideoToggleFullscreen, ch('f')),
        (C::MpvOverlay, A::VideoToggleMute, ch('m')),
        // Shared navigation (fallback for every list/text screen).
        (C::Common, A::MoveUp, key(KeyCode::Up)),
        (C::Common, A::MoveDown, key(KeyCode::Down)),
        (C::Common, A::PageUp, key(KeyCode::PageUp)),
        (C::Common, A::PageDown, key(KeyCode::PageDown)),
        (C::Common, A::JumpTop, key(KeyCode::Home)),
        (C::Common, A::JumpBottom, key(KeyCode::End)),
        // Shift+nav range-select (extends the anchor..=cursor selection in Library/Queue).
        (C::Common, A::SelectUp, shift(KeyCode::Up)),
        (C::Common, A::SelectDown, shift(KeyCode::Down)),
        (C::Common, A::SelectPageUp, shift(KeyCode::PageUp)),
        (C::Common, A::SelectPageDown, shift(KeyCode::PageDown)),
        (C::Common, A::SelectToTop, shift(KeyCode::Home)),
        (C::Common, A::SelectToBottom, shift(KeyCode::End)),
        (C::Common, A::Confirm, key(KeyCode::Enter)),
        (C::Common, A::FocusPrev, key(KeyCode::BackTab)),
        (C::Common, A::FocusNext, key(KeyCode::Tab)),
        (C::Common, A::DeleteChar, key(KeyCode::Backspace)),
        (
            C::Common,
            A::DeleteWord,
            Chord::new(KeyCode::Backspace, KeyModifiers::CONTROL),
        ),
        (C::Common, A::MoveCursorLeft, key(KeyCode::Left)),
        (C::Common, A::MoveCursorRight, key(KeyCode::Right)),
        (
            C::Common,
            A::MoveCursorWordLeft,
            Chord::new(KeyCode::Left, KeyModifiers::CONTROL),
        ),
        (
            C::Common,
            A::MoveCursorWordRight,
            Chord::new(KeyCode::Right, KeyModifiers::CONTROL),
        ),
        (C::Common, A::Back, ch('q')),
        // Global (active across screens; typeable globals are suppressed in text fields).
        (C::Global, A::Home, ctrl('h')),
        (C::Global, A::ToggleStreaming, ctrl('r')),
        (C::Global, A::ToggleHelp, ch('?')),
        (C::Global, A::OpenContextMenu, shift(KeyCode::F(10))),
        (C::Global, A::ToggleAbout, key(KeyCode::F(1))),
        (C::Global, A::ToggleAnimations, ch('A')),
        (C::Global, A::ToggleControlBox, ch('B')),
        (C::Global, A::WhyAi, ch('w')),
        // Browser-style text zoom (`=` is the unshifted `+` key). Works only on terminals
        // with the text sizing protocol; elsewhere the reducer answers with a hint toast.
        (C::Global, A::TextZoomIn, ctrl('=')),
        (C::Global, A::TextZoomOut, ctrl('-')),
        // Freezes the Ctrl+wheel zoom gesture (an easy thing to fire by accident while
        // scrolling with a modifier held); the Ctrl+-/= keys stay live either way.
        (C::Global, A::ToggleZoomWheelLock, ctrl('l')),
        (C::Global, A::Quit, ctrl('q')),
        // Library list commands.
        (C::Library, A::Confirm, key(KeyCode::Enter)),
        (C::Library, A::ToggleLocalMode, alt_shift('l')),
        (C::Library, A::Enqueue, ch('\\')),
        (C::Library, A::PlayAll, ch('a')),
        (C::Library, A::Favorite, ch('f')),
        (C::Library, A::Download, ch('d')),
        (C::Library, A::DownloadAll, ch('D')),
        (C::Library, A::OpenAi, ch('g')),
        (C::Library, A::AddToPlaylist, ch('p')),
        (C::Library, A::LibraryRemove, key(KeyCode::Delete)),
        (C::Library, A::LibraryFilter, ch('/')),
        (C::Library, A::Back, ch('q')),
        (C::LocalDeck, A::AcceptAllImportReview, ch('A')),
        (C::LocalDeck, A::OpenLocalFind, ctrl('f')),
        // Playlists tab (root list of playlists + opened-playlist drill-down).
        (C::Playlists, A::Confirm, key(KeyCode::Enter)),
        (C::Playlists, A::PlayAll, ch('a')),
        (C::Playlists, A::Enqueue, ch('\\')),
        (C::Playlists, A::PlaylistCreate, ch('n')),
        (C::Playlists, A::Favorite, ch('f')),
        (C::Playlists, A::Download, ch('d')),
        (C::Playlists, A::DownloadAll, ch('D')),
        (C::Playlists, A::OpenAi, ch('g')),
        (C::Playlists, A::AddToPlaylist, ch('p')),
        (C::Playlists, A::LibraryRemove, key(KeyCode::Delete)),
        (C::Playlists, A::LibraryFilter, ch('/')),
        (C::Playlists, A::Back, ch('q')),
        // Queue window (overlay on the player; up/down nav comes from Common).
        (C::Queue, A::Confirm, key(KeyCode::Enter)),
        (C::Queue, A::QueueRemove, key(KeyCode::Delete)),
        (C::Queue, A::Back, ch('q')),
        // Search box (text entry; Enter→search is handled in the input handler).
        (C::SearchInput, A::SelectAll, ctrl('a')),
        (C::SearchInput, A::ToggleSearchSourceMenu, key(KeyCode::Tab)),
        (C::SearchInput, A::ToggleSearchKind, ctrl('p')),
        (C::SearchInput, A::FocusPrev, key(KeyCode::BackTab)),
        // Search results list commands (Enter→play is fixed to the physical key in the
        // handler, so it's not listed here; the cheat-sheet shows it as a fixed row).
        (C::SearchResults, A::FocusPrev, key(KeyCode::BackTab)),
        (
            C::SearchResults,
            A::ToggleSearchSourceMenu,
            key(KeyCode::Tab),
        ),
        (C::SearchResults, A::ToggleSearchKind, ctrl('p')),
        (C::SearchResults, A::Enqueue, ch('\\')),
        (C::SearchResults, A::Favorite, ch('f')),
        (C::SearchResults, A::Download, ch('d')),
        (C::SearchResults, A::AddToPlaylist, ch('p')),
        (C::SearchResults, A::SearchFilter, ch('/')),
        (C::SearchResults, A::Back, ch('q')),
        // DJ Gem box (text entry; Enter→send is handled in the input handler).
        (C::AiInput, A::SelectAll, ctrl('a')),
        // Settings screen commands (nav comes from Common).
        (C::Settings, A::ChangeDecrease, key(KeyCode::Left)),
        (C::Settings, A::ChangeIncrease, key(KeyCode::Right)),
        (C::Settings, A::SettingsCancel, ch('q')),
    ]
}

/// The default chord for `(ctx, action)`, if it has one.
pub(super) fn default_chord(ctx: KeyContext, action: Action) -> Option<Chord> {
    default_bindings()
        .into_iter()
        .find(|(c, a, _)| *c == ctx && *a == action)
        .map(|(.., ch)| ch)
}

/// The editable bindings grouped by context, in display order, for the editor and the
/// `?` cheat-sheet (headers + rows).
pub fn groups() -> Vec<(KeyContext, Vec<Action>)> {
    let mut out: Vec<(KeyContext, Vec<Action>)> = Vec::new();
    for (ctx, action, _) in default_bindings() {
        match out.last_mut() {
            Some((c, v)) if *c == ctx => v.push(action),
            _ => out.push((ctx, vec![action])),
        }
    }
    out
}

/// A flat, header-free list of every editable `(context, action)`, in display order. The
/// Keys-tab cursor indexes directly into this.
/// One editable action row for the wire keymap catalog (docs/gui/05 Hotkeys tab).
pub struct WireAction {
    pub context: &'static str,
    pub id: &'static str,
    pub label: String,
    pub default_chord: String,
}

/// Every editable action with its context, human label, and factory chord — the
/// settings model's `keymap.actions` catalog.
pub fn wire_actions() -> Vec<WireAction> {
    default_bindings()
        .into_iter()
        .map(|(ctx, action, def)| WireAction {
            context: ctx.id(),
            id: action.id(),
            label: action.human_label_for(ctx).to_string(),
            default_chord: chord_to_config(def),
        })
        .collect()
}

pub fn editable_entries() -> Vec<(KeyContext, Action)> {
    default_bindings()
        .into_iter()
        .map(|(c, a, _)| (c, a))
        .collect()
}
