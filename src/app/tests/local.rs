use super::*;
use crate::artwork::ArtSource;
use ratatui_image::picker::Picker;

pub(super) fn local_song(stem: &str) -> Song {
    Song::local_file(PathBuf::from(format!("/tmp/{stem}.m4a")))
}

pub(super) fn local_deck_track(
    path: &str,
    title: &str,
    artist: &[&str],
    album: Option<&str>,
    album_artist: Option<&str>,
    genre: &[&str],
    modified_at: i64,
) -> crate::local::LocalTrack {
    let mut track = crate::local::LocalTrack::untagged(
        PathBuf::from(path),
        path.len() as u64 + 100,
        modified_at,
    );
    track.title = title.to_owned();
    track.artist = artist.iter().map(|value| (*value).to_owned()).collect();
    track.album = album.map(str::to_owned);
    track.album_artist = album_artist.map(str::to_owned);
    track.genre = genre.iter().map(|value| (*value).to_owned()).collect();
    track.duration_ms = Some(60_000);
    track
}

#[derive(Debug, PartialEq)]
struct ModeSwitchProjection {
    local: bool,
    radio: bool,
    mode: Mode,
    queue: Vec<u8>,
    cursor: usize,
    loaded: Option<String>,
    paused: bool,
    position_epoch: u64,
    streaming_pending: bool,
    rerank_pending: bool,
    theme: Vec<u8>,
}

fn mode_switch_projection(app: &App) -> ModeSwitchProjection {
    ModeSwitchProjection {
        local: app.local_dedicated_mode,
        radio: app.radio_dedicated_mode,
        mode: app.mode,
        queue: serde_json::to_vec(&app.queue.snapshot()).expect("queue snapshot serializes"),
        cursor: app.queue.cursor_pos(),
        loaded: app.prefetch.loaded_video_id.clone(),
        paused: app.playback.paused,
        position_epoch: app.playback.position_epoch,
        streaming_pending: app.streaming.pending,
        rerank_pending: app.streaming.pending_rerank.is_some(),
        theme: serde_json::to_vec(&app.theme).expect("theme serializes"),
    }
}

pub(super) fn admit_local_mode_confirm(app: &mut App, confirm: LocalModeConfirm) -> Vec<Cmd> {
    let before = mode_switch_projection(app);
    let mut cmds = app.apply_local_mode_confirm(confirm);
    assert_eq!(
        mode_switch_projection(app),
        before,
        "local mode switch state must wait for player admission"
    );
    admit_player_transition(app, &mut cmds);
    cmds
}

fn admit_radio_mode_confirm(app: &mut App, confirm: RadioModeConfirm) -> Vec<Cmd> {
    let before = mode_switch_projection(app);
    let mut cmds = app.apply_radio_mode_confirm(confirm);
    assert_eq!(
        mode_switch_projection(app),
        before,
        "radio mode switch state must wait for player admission"
    );
    admit_player_transition(app, &mut cmds);
    cmds
}

fn assert_stop_before_load(cmds: &[Cmd]) {
    let commands: Vec<_> = cmds.iter().flat_map(Cmd::player_commands).collect();
    let stop = commands
        .iter()
        .position(|command| matches!(command, PlayerCmd::Stop))
        .expect("mode switch Stop command");
    let load = commands
        .iter()
        .position(|command| matches!(command, PlayerCmd::Load(_)))
        .expect("mode switch Load command");
    assert!(stop < load, "mode switch must Stop before restoring a Load");
}

pub(super) fn app_with_local_deck_index(tracks: Vec<crate::local::LocalTrack>) -> App {
    let mut app = App::new(100);
    app.mode = Mode::Library;
    admit_local_mode_confirm(&mut app, LocalModeConfirm::Enter);
    let mut index = crate::local::LocalIndex::default();
    index.set_tracks(tracks);
    app.update(Msg::Local(LocalMsg::ScanFinished {
        index_path: None,
        result: crate::local::LocalScanResult {
            summary: crate::local::LocalScanSummary {
                indexed: index.tracks().len(),
                added: index.tracks().len(),
                ..crate::local::LocalScanSummary::default()
            },
            index,
            errors: Vec::new(),
        },
    }));
    app
}

fn render_local_snapshot(
    app: &App,
    snapshot: &LocalRowsSnapshot,
    width: u16,
    height: u16,
) -> (ratatui::buffer::Buffer, Vec<MouseButtonRegion>) {
    app.clear_mouse_regions();
    let backend = TestBackend::new(width, height);
    let mut terminal = Terminal::new(backend).unwrap();
    terminal
        .draw(|frame| {
            let area = frame.area();
            crate::ui::views::local::render_test_snapshot(frame, app, area, snapshot);
        })
        .unwrap();
    let buffer = terminal.backend().buffer().clone();
    let hits = app.hits.regions().clone();
    (buffer, hits)
}

pub(super) fn assert_local_cached_render_matches_legacy(app: &App, width: u16, height: u16) {
    let legacy = app.local_uncached_rows_snapshot();
    let cached = app.local_rows_snapshot();
    assert_eq!(legacy.rows(), cached.rows());
    assert_eq!(legacy.total_len(), cached.total_len());
    let legacy_render = render_local_snapshot(app, &legacy, width, height);
    let cached_render = render_local_snapshot(app, &cached, width, height);
    assert_eq!(legacy_render, cached_render);
}

#[test]
fn double_click_library_nav_confirms_local_deck_shell() {
    let mut app = App::new(100);
    app.mode = Mode::Library;

    let cmds = double_click_target(&mut app, MouseTarget::Nav(Mode::Library));
    assert!(cmds.is_empty());
    assert_eq!(
        app.local_mode.pending_confirm,
        Some(LocalModeConfirm::Enter)
    );
    assert!(!app.local_dedicated_mode);

    let before = mode_switch_projection(&app);
    let mut cmds = app.update(Msg::Key(key(KeyCode::Enter)));
    assert_eq!(mode_switch_projection(&app), before);
    admit_player_transition(&mut app, &mut cmds);
    assert!(app.local_dedicated_mode);
    assert_eq!(app.mode, Mode::Library);
    assert!(app.local_mode.pending_confirm.is_none());

    let cmds = double_click_target(&mut app, MouseTarget::Nav(Mode::Library));
    assert!(cmds.is_empty());
    assert_eq!(app.local_mode.pending_confirm, Some(LocalModeConfirm::Exit));

    let before = mode_switch_projection(&app);
    let mut cmds = app.update(Msg::Key(key(KeyCode::Enter)));
    assert_eq!(mode_switch_projection(&app), before);
    admit_player_transition(&mut app, &mut cmds);
    assert!(!app.local_dedicated_mode);
    assert!(app.local_mode.pending_confirm.is_none());
}

#[test]
fn local_deck_and_radio_mode_are_mutually_exclusive() {
    let mut app = App::new(100);
    app.mode = Mode::Library;
    admit_local_mode_confirm(&mut app, LocalModeConfirm::Enter);
    assert!(app.local_dedicated_mode);

    app.mode = Mode::Player;
    let cmds = app.request_radio_mode_switch();
    assert!(cmds.is_empty());
    assert!(app.radio_mode.pending_radio_mode_confirm.is_none());
    assert!(!app.radio_dedicated_mode);
    assert!(!app.status.text.is_empty());

    admit_local_mode_confirm(&mut app, LocalModeConfirm::Exit);
    assert!(!app.local_dedicated_mode);
    admit_radio_mode_confirm(&mut app, RadioModeConfirm::Enter);
    assert!(app.radio_dedicated_mode);

    app.mode = Mode::Library;
    let cmds = app.request_local_mode_switch();
    assert!(cmds.is_empty());
    assert!(app.local_mode.pending_confirm.is_none());
    assert!(app.radio_dedicated_mode);
}

#[test]
fn alt_shift_l_confirms_local_deck_enter_and_exit_from_keyboard() {
    let mut app = App::new(100);
    app.mode = Mode::Library;

    let cmds = app.update(Msg::Key(alt_shift(KeyCode::Char('l'))));

    assert!(cmds.is_empty());
    assert_eq!(
        app.local_mode.pending_confirm,
        Some(LocalModeConfirm::Enter)
    );
    let before = mode_switch_projection(&app);
    let mut cmds = app.update(Msg::Key(key(KeyCode::Enter)));
    assert_eq!(mode_switch_projection(&app), before);
    admit_player_transition(&mut app, &mut cmds);
    assert!(app.local_dedicated_mode);

    let cmds = app.update(Msg::Key(alt_shift(KeyCode::Char('l'))));

    assert!(cmds.is_empty());
    assert_eq!(app.local_mode.pending_confirm, Some(LocalModeConfirm::Exit));
}

#[test]
fn local_deck_keyboard_toggle_uses_user_rebound_key() {
    let mut app = App::new(100);
    app.mode = Mode::Library;
    app.keymap
        .rebind(
            KeyContext::Library,
            Action::ToggleLocalMode,
            crate::keymap::parse_chord("f8").unwrap(),
        )
        .unwrap();

    app.update(Msg::Key(key(KeyCode::F(8))));

    assert_eq!(
        app.local_mode.pending_confirm,
        Some(LocalModeConfirm::Enter)
    );
}

#[test]
fn local_deck_renders_download_seed_rows_and_activates_them() {
    let mut app = App::new(100);
    app.mode = Mode::Library;
    app.library_ui.downloaded = vec![Song::local_file(PathBuf::from("/tmp/Alpha.m4a"))];
    admit_local_mode_confirm(&mut app, LocalModeConfirm::Enter);

    let buf = render_app_buffer(&app, 80, 24);
    assert!(buffer_contains(&buf, "LOCAL DECK"));
    assert!(
        app.hits
            .regions()
            .iter()
            .any(|r| r.target == MouseTarget::LocalRow(0))
    );

    let mut cmds = double_click_target(&mut app, MouseTarget::LocalRow(0));
    assert!(!cmds.is_empty());
    admit_player_transition(&mut app, &mut cmds);
    assert_eq!(app.queue.current().map(|s| s.title.as_str()), Some("Alpha"));
}

#[test]
fn local_deck_enter_loads_index_then_scans_download_root_when_empty() {
    let mut app = App::new(100);
    let root = PathBuf::from("/tmp/yututui-local-deck-test-root");
    app.config.download_dir = Some(root.clone());

    let before = mode_switch_projection(&app);
    let mut enter = app.apply_local_mode_confirm(LocalModeConfirm::Enter);

    assert_eq!(mode_switch_projection(&app), before);
    assert!(!app.local_dedicated_mode);
    assert!(!app.local_mode.index.loading);
    admit_player_transition(&mut app, &mut enter);
    assert!(app.local_dedicated_mode);
    assert!(app.local_mode.index.loading);
    assert!(
        enter
            .iter()
            .any(|cmd| matches!(cmd, Cmd::Local(LocalCmd::LoadIndex { .. })))
    );

    let scan = app.update(Msg::Local(LocalMsg::IndexLoaded {
        index_path: None,
        index: crate::local::LocalIndex::default(),
        warnings: Vec::new(),
    }));

    assert!(app.local_mode.index.scanning);
    let Some(Cmd::Local(LocalCmd::ScanRoots {
        roots, previous, ..
    })) = scan
        .iter()
        .find(|cmd| matches!(cmd, Cmd::Local(LocalCmd::ScanRoots { .. })))
    else {
        panic!("expected Local Deck scan command after empty index load");
    };
    assert_eq!(roots, &vec![crate::local::LocalScanRoot::download(root)]);
    assert!(previous.is_empty());
}

#[test]
fn local_deck_scan_roots_follow_local_config() {
    let mut app = App::new(100);
    let downloads = PathBuf::from("/tmp/yututui-local-downloads");
    let music = PathBuf::from("/tmp/yututui-music-root");
    app.config.download_dir = Some(downloads.clone());
    app.config.local.roots = vec![crate::config::LocalRootConfig {
        path: music.clone(),
        enabled: Some(true),
        recursive: Some(true),
    }];

    assert_eq!(
        app.local_scan_roots(),
        vec![
            crate::local::LocalScanRoot::download(downloads.clone()),
            crate::local::LocalScanRoot::recursive(music.clone()),
        ]
    );

    app.config.local.include_download_dir = Some(false);
    assert_eq!(
        app.local_scan_roots(),
        vec![crate::local::LocalScanRoot::recursive(music)]
    );
}

#[test]
fn local_deck_scan_roots_merge_duplicate_download_root_recursively() {
    let mut app = App::new(100);
    let root = PathBuf::from("/tmp/yututui-local-merged-root");
    app.config.download_dir = Some(root.clone());
    app.config.local.roots = vec![crate::config::LocalRootConfig {
        path: root.clone(),
        enabled: Some(true),
        recursive: Some(true),
    }];

    assert_eq!(
        app.local_scan_roots(),
        vec![crate::local::LocalScanRoot::recursive(root)]
    );
}

#[test]
fn local_deck_scan_result_replaces_seed_rows_and_activates_index_track() {
    let mut app = App::new(100);
    app.mode = Mode::Library;
    app.library_ui.downloaded = vec![Song::local_file(PathBuf::from("/tmp/Seed.m4a"))];
    admit_local_mode_confirm(&mut app, LocalModeConfirm::Enter);

    let mut track = crate::local::LocalTrack::untagged(PathBuf::from("/tmp/Indexed.flac"), 7, 8);
    track.title = "Indexed Title".to_owned();
    track.artist = vec!["Indexed Artist".to_owned()];
    track.duration_ms = Some(61_000);
    let mut index = crate::local::LocalIndex::default();
    index.set_tracks(vec![track]);
    app.update(Msg::Local(LocalMsg::ScanFinished {
        index_path: None,
        result: crate::local::LocalScanResult {
            index,
            summary: crate::local::LocalScanSummary {
                indexed: 1,
                added: 1,
                ..crate::local::LocalScanSummary::default()
            },
            errors: Vec::new(),
        },
    }));

    assert_eq!(app.local_rows_len(), 1);
    assert_eq!(app.local_mode.ui.section, LocalSection::Tracks);

    let mut cmds = double_click_target(&mut app, MouseTarget::LocalRow(0));

    assert!(!cmds.is_empty());
    admit_player_transition(&mut app, &mut cmds);
    assert_eq!(
        app.queue.current().map(|s| s.title.as_str()),
        Some("Indexed Title")
    );
    assert_eq!(
        app.queue.current().map(|s| s.artist.as_str()),
        Some("Indexed Artist")
    );
}

#[test]
fn local_deck_r_key_requests_incremental_rescan() {
    let mut app = App::new(100);
    admit_local_mode_confirm(&mut app, LocalModeConfirm::Enter);
    app.local_mode.index.loading = false;
    app.local_mode.index.loaded = true;
    let track = crate::local::LocalTrack::untagged(PathBuf::from("/tmp/Indexed.flac"), 7, 8);
    let mut index = crate::local::LocalIndex::default();
    index.set_tracks(vec![track]);
    app.local_mode.index.index = index;

    let cmds = app.update(Msg::Key(key(KeyCode::Char('r'))));

    assert!(app.local_mode.index.scanning);
    let Some(Cmd::Local(LocalCmd::ScanRoots { previous, .. })) = cmds
        .iter()
        .find(|cmd| matches!(cmd, Cmd::Local(LocalCmd::ScanRoots { .. })))
    else {
        panic!("expected Local Deck rescan command");
    };
    assert_eq!(previous.tracks().len(), 1);
}

#[test]
fn local_deck_scan_progress_updates_status_line_until_finished() {
    let mut app = app_with_local_deck_index(Vec::new());
    app.config.download_dir = Some(PathBuf::from("/tmp/music"));

    app.request_local_scan(false);
    app.update(Msg::Local(LocalMsg::ScanProgress(
        crate::local::LocalScanProgress {
            seen: 3,
            indexed: 2,
            skipped: 1,
            errors: 1,
            current: Some(PathBuf::from("/tmp/music/song.flac")),
        },
    )));

    assert!(app.local_mode.index.scanning);
    assert_eq!(
        app.local_mode
            .index
            .progress
            .as_ref()
            .map(|progress| progress.seen),
        Some(3)
    );
    assert!(app.status.text.contains("3 seen"));
    assert!(app.status.text.contains("2 indexed"));
    assert!(app.status.text.contains("song.flac"));
    let buf = render_app_buffer(&app, 100, 24);
    assert!(buffer_contains(&buf, "3 seen"));
    assert!(buffer_contains(&buf, "2 indexed"));

    app.update(Msg::Local(LocalMsg::ScanFinished {
        index_path: None,
        result: crate::local::LocalScanResult {
            index: crate::local::LocalIndex::default(),
            summary: crate::local::LocalScanSummary::default(),
            errors: Vec::new(),
        },
    }));

    assert!(app.local_mode.index.progress.is_none());
    assert!(!app.local_mode.index.scanning);
}

#[test]
fn local_deck_slash_filters_index_tracks_and_activation_uses_visible_row() {
    let mut app = App::new(100);
    admit_local_mode_confirm(&mut app, LocalModeConfirm::Enter);
    let mut alpha = crate::local::LocalTrack::untagged(PathBuf::from("/tmp/Alpha.flac"), 7, 8);
    alpha.title = "Alpha".to_owned();
    let mut beta = crate::local::LocalTrack::untagged(PathBuf::from("/tmp/Beta.flac"), 9, 10);
    beta.title = "Beta".to_owned();
    beta.artist = vec!["Filtered Artist".to_owned()];
    let mut index = crate::local::LocalIndex::default();
    index.set_tracks(vec![alpha, beta]);
    app.update(Msg::Local(LocalMsg::ScanFinished {
        index_path: None,
        result: crate::local::LocalScanResult {
            index,
            summary: crate::local::LocalScanSummary {
                indexed: 2,
                added: 2,
                ..crate::local::LocalScanSummary::default()
            },
            errors: Vec::new(),
        },
    }));

    app.update(Msg::Key(key(KeyCode::Char('/'))));
    assert!(app.local_mode.ui.filter_editing);
    for ch in "filtered".chars() {
        app.update(Msg::Key(key(KeyCode::Char(ch))));
    }

    assert_eq!(app.local_rows_len(), 1);
    let mut cmds = double_click_target(&mut app, MouseTarget::LocalRow(0));

    assert!(!cmds.is_empty());
    admit_player_transition(&mut app, &mut cmds);
    assert_eq!(app.queue.current().map(|s| s.title.as_str()), Some("Beta"));
}

#[test]
fn local_deck_escape_clears_committed_filter_before_exit() {
    let mut app = App::new(100);
    admit_local_mode_confirm(&mut app, LocalModeConfirm::Enter);
    let mut track = crate::local::LocalTrack::untagged(PathBuf::from("/tmp/Alpha.flac"), 7, 8);
    track.title = "Alpha".to_owned();
    let mut index = crate::local::LocalIndex::default();
    index.set_tracks(vec![track]);
    app.local_mode.index.index = index;
    app.local_mode.index.loaded = true;

    app.update(Msg::Key(key(KeyCode::Char('/'))));
    app.update(Msg::Key(key(KeyCode::Char('a'))));
    app.update(Msg::Key(key(KeyCode::Enter)));
    assert!(!app.local_mode.ui.filter_editing);
    assert_eq!(app.local_mode.ui.filter_query, "a");

    app.update(Msg::Key(key(KeyCode::Esc)));

    assert!(app.local_dedicated_mode);
    assert!(app.local_mode.pending_confirm.is_none());
    assert!(app.local_mode.ui.filter_query.is_empty());
}

#[test]
fn ctrl_backspace_edits_the_local_deck_filter() {
    let mut app = app_with_local_deck_index(Vec::new());
    app.update(Msg::Key(key(KeyCode::Char('/'))));
    for c in "album artist".chars() {
        app.update(Msg::Key(key(KeyCode::Char(c))));
    }
    app.update(Msg::Key(ctrl(KeyCode::Backspace)));
    assert_eq!(app.local_mode.ui.filter_query, "album ");
}

#[test]
fn local_deck_sidebar_switches_sections_with_mouse_and_number_keys() {
    let mut app = app_with_local_deck_index(vec![local_deck_track(
        "/tmp/music/Daft Punk/Discovery/One More Time.flac",
        "One More Time",
        &["Daft Punk"],
        Some("Discovery"),
        Some("Daft Punk"),
        &["House"],
        10,
    )]);

    let buf = render_app_buffer(&app, 100, 24);
    assert!(buffer_contains(&buf, "3 Albums"));
    assert!(
        app.hits
            .regions()
            .iter()
            .any(|r| r.target == MouseTarget::LocalNav(2))
    );

    let cmds = click_target(&mut app, MouseTarget::LocalNav(2));
    assert!(cmds.is_empty());
    assert_eq!(app.local_mode.ui.section, LocalSection::Albums);
    assert_eq!(app.local_rows_len(), 1);

    app.update(Msg::Key(key(KeyCode::Char('4'))));
    assert_eq!(app.local_mode.ui.section, LocalSection::Artists);
    assert_eq!(app.local_rows_len(), 1);
}

#[test]
fn local_deck_album_rows_drill_down_to_tracks_and_play() {
    let mut first = local_deck_track(
        "/tmp/music/Daft Punk/Discovery/01 One More Time.flac",
        "One More Time",
        &["Daft Punk"],
        Some("Discovery"),
        Some("Daft Punk"),
        &["House"],
        10,
    );
    first.track_no = Some(1);
    let mut second = local_deck_track(
        "/tmp/music/Daft Punk/Discovery/02 Aerodynamic.flac",
        "Aerodynamic",
        &["Daft Punk"],
        Some("Discovery"),
        Some("Daft Punk"),
        &["House"],
        11,
    );
    second.track_no = Some(2);
    let mut app = app_with_local_deck_index(vec![second, first]);

    app.update(Msg::Key(key(KeyCode::Char('3'))));
    assert_eq!(app.local_mode.ui.section, LocalSection::Albums);
    assert_eq!(app.local_rows_len(), 1);
    assert!(
        app.local_row_text(&app.local_visible_rows()[0])
            .contains("Discovery")
    );

    let open = double_click_target(&mut app, MouseTarget::LocalRow(0));
    assert!(open.is_empty());
    assert_eq!(app.local_mode.ui.drill.len(), 1);
    assert_eq!(app.local_rows_len(), 2);

    let mut play = double_click_target(&mut app, MouseTarget::LocalRow(0));
    assert!(!play.is_empty());
    admit_player_transition(&mut app, &mut play);
    assert_eq!(
        app.queue.current().map(|song| song.title.as_str()),
        Some("One More Time")
    );
}

#[test]
fn local_deck_artist_rows_open_album_drill_down() {
    let mut app = app_with_local_deck_index(vec![local_deck_track(
        "/tmp/music/IU/Palette/Palette.flac",
        "Palette",
        &["IU"],
        Some("Palette"),
        Some("IU"),
        &["K-Pop"],
        10,
    )]);

    app.update(Msg::Key(key(KeyCode::Char('4'))));
    assert_eq!(app.local_mode.ui.section, LocalSection::Artists);
    assert_eq!(app.local_rows_len(), 1);

    let open_artist = double_click_target(&mut app, MouseTarget::LocalRow(0));
    assert!(open_artist.is_empty());
    assert_eq!(app.local_rows_len(), 1);
    assert!(
        app.local_row_text(&app.local_visible_rows()[0])
            .contains("Palette")
    );

    let open_album = double_click_target(&mut app, MouseTarget::LocalRow(0));
    assert!(open_album.is_empty());
    assert_eq!(app.local_rows_len(), 1);

    let mut play = double_click_target(&mut app, MouseTarget::LocalRow(0));
    assert!(!play.is_empty());
    admit_player_transition(&mut app, &mut play);
    assert_eq!(
        app.queue.current().map(|song| song.title.as_str()),
        Some("Palette")
    );
}

#[test]
fn local_deck_folder_smart_and_scan_error_sections_render_rows() {
    let untagged = local_deck_track(
        "/tmp/music/Misc/untagged.flac",
        "untagged",
        &[],
        None,
        None,
        &[],
        9,
    );
    let tagged = local_deck_track(
        "/tmp/music/Tagged/song.flac",
        "Song",
        &["Artist"],
        Some("Album"),
        Some("Artist"),
        &["Indie"],
        10,
    );
    let mut app = app_with_local_deck_index(vec![untagged, tagged]);
    app.local_mode.index.load_errors = vec![crate::local::ScanError {
        path: PathBuf::from("/tmp/local-index.json"),
        message: "local index JSON was corrupt and was rebuilt".to_owned(),
    }];
    app.local_mode.index.errors = vec![crate::local::ScanError {
        path: PathBuf::from("/tmp/music/bad.mp3"),
        message: "bad tags".to_owned(),
    }];

    app.update(Msg::Key(key(KeyCode::Char('6'))));
    assert_eq!(app.local_mode.ui.section, LocalSection::Folders);
    assert_eq!(app.local_rows_len(), 2);
    let open_folder = double_click_target(&mut app, MouseTarget::LocalRow(0));
    assert!(open_folder.is_empty());
    assert_eq!(app.local_rows_len(), 1);

    app.update(Msg::Key(key(KeyCode::Esc)));
    app.update(Msg::Key(key(KeyCode::Char('7'))));
    assert_eq!(app.local_mode.ui.section, LocalSection::SmartLists);
    let open_missing_artist = double_click_target(&mut app, MouseTarget::LocalRow(3));
    assert!(open_missing_artist.is_empty());
    assert_eq!(app.local_rows_len(), 1);
    assert!(
        app.local_row_text(&app.local_visible_rows()[0])
            .contains("untagged")
    );

    app.update(Msg::Key(key(KeyCode::Esc)));
    app.update(Msg::Key(key(KeyCode::Char('8'))));
    assert_eq!(app.local_mode.ui.section, LocalSection::ScanErrors);
    assert_eq!(app.local_rows_len(), 2);
    let buf = render_app_buffer(&app, 100, 24);
    assert!(buffer_contains(&buf, "index JSON"));
    assert!(buffer_contains(&buf, "bad tags"));
}

#[test]
fn local_deck_smart_lists_report_counts_for_every_shipped_list() {
    let mut downloaded = local_deck_track(
        "/tmp/music/Ytm/downloaded.m4a",
        "Downloaded",
        &["Artist"],
        Some("Album"),
        Some("Artist"),
        &["Pop"],
        12,
    );
    downloaded.linked_video_id = Some("abcdefghijk".to_owned());
    downloaded.embedded_art_key = Some("cover".to_owned());

    let mut missing = local_deck_track(
        "/tmp/music/Misc/missing.mp3",
        "Missing",
        &[],
        None,
        None,
        &[],
        11,
    );
    missing.file_size = 60 * 1024 * 1024;

    let mut lossless = local_deck_track(
        "/tmp/music/Tagged/lossless.flac",
        "Lossless",
        &["Band"],
        Some("Record"),
        Some("Band"),
        &["Rock"],
        10,
    );
    lossless.embedded_art_key = Some("cover".to_owned());

    let mut app = app_with_local_deck_index(vec![downloaded, missing, lossless]);
    app.update(Msg::Key(key(KeyCode::Char('7'))));

    let labels: Vec<_> = app
        .local_visible_rows()
        .iter()
        .map(|row| app.local_row_text(row))
        .collect();

    for expected in [
        "Recently Added  (3 tracks)",
        "Downloaded from YouTube Music  (1 tracks)",
        "Local-only  (2 tracks)",
        "Missing Artist  (1 tracks)",
        "Missing Album  (1 tracks)",
        "No Embedded Cover  (1 tracks)",
        "Large Files  (1 tracks)",
        "Lossless  (1 tracks)",
    ] {
        assert!(
            labels.iter().any(|label| label == expected),
            "missing smart list label {expected:?} in {labels:?}"
        );
    }
}

#[test]
fn local_deck_details_include_selected_track_metadata_and_up_next() {
    let mut first = local_deck_track(
        "/tmp/music/Daft Punk/Discovery/01 One More Time.flac",
        "One More Time",
        &["Daft Punk"],
        Some("Discovery"),
        Some("Daft Punk"),
        &["House"],
        10,
    );
    first.year = Some(2001);
    first.disc_no = Some(1);
    first.track_no = Some(1);
    first.duration_ms = Some(61_000);
    first.format = Some(crate::local::AudioFormat::Flac);
    first.sample_rate = Some(44_100);
    first.bitrate = Some(320_000);
    first.embedded_art_key = Some("embedded-cover".to_owned());
    let mut second = local_deck_track(
        "/tmp/music/Daft Punk/Discovery/02 Aerodynamic.flac",
        "Aerodynamic",
        &["Daft Punk"],
        Some("Discovery"),
        Some("Daft Punk"),
        &["House"],
        11,
    );
    second.track_no = Some(2);
    let mut app = app_with_local_deck_index(vec![first.clone(), second.clone()]);
    app.config.download_dir = Some(PathBuf::from("/tmp/music"));
    app.queue.set(vec![first.to_song(), second.to_song()], 0);

    let lines = app.local_details_lines();

    for expected in [
        "Title: One More Time",
        "Artist: Daft Punk",
        "Album: Discovery · 2001",
        "Track: disc 1 · track 1",
        "Duration: 1:01",
        "Format: FLAC",
        "Sample rate: 44.1 kHz",
        "Bitrate: 320 kbps",
        "Cover: embedded cover",
        "File: 01 One More Time.flac",
        "Path: Daft Punk/Discovery/01 One More Time.flac",
        "1. Aerodynamic - Daft Punk  (1:00)",
    ] {
        assert!(
            lines.iter().any(|line| line == expected),
            "missing {expected:?} in {lines:?}"
        );
    }
}

#[test]
fn local_deck_render_expands_details_then_collapses_to_summary() {
    let mut track = local_deck_track(
        "/tmp/music/IU/Palette/Palette.flac",
        "Palette",
        &["IU"],
        Some("Palette"),
        Some("IU"),
        &["K-Pop"],
        10,
    );
    track.year = Some(2017);
    track.embedded_art_key = Some("embedded-cover".to_owned());
    let app = app_with_local_deck_index(vec![track]);

    let wide = render_app_buffer(&app, 120, 30);
    assert!(buffer_contains(&wide, "Selected"));
    assert!(buffer_contains(&wide, "Title: Palette"));
    assert!(buffer_contains(&wide, "Cover: embedded cover"));

    let medium = render_app_buffer(&app, 90, 24);
    assert!(buffer_contains(&medium, "Selected: Palette - IU"));
}

#[test]
fn local_deck_a_enqueues_selected_track_without_interrupting_current() {
    let mut app = app_with_local_deck_index(vec![local_deck_track(
        "/tmp/music/local-alpha.flac",
        "Local Alpha",
        &["Local Artist"],
        None,
        None,
        &[],
        10,
    )]);
    app.queue.set(songs(1), 0);
    let mut load = app.load_song(app.queue.current().cloned());
    admit_player_transition(&mut app, &mut load);
    app.mode = Mode::Library;

    let cmds = app.update(Msg::Key(key(KeyCode::Char('a'))));

    assert!(load_url(&cmds).is_none());
    assert_eq!(current(&app), "id0");
    assert_eq!(app.queue.len(), 2);
    let ordered: Vec<_> = app
        .queue
        .ordered()
        .iter()
        .map(|s| s.title.as_str())
        .collect();
    assert_eq!(ordered, vec!["t0", "Local Alpha"]);
}

#[test]
fn local_deck_shift_a_enqueues_visible_filtered_rows() {
    let mut app = app_with_local_deck_index(vec![
        local_deck_track(
            "/tmp/music/a-alpha.flac",
            "Alpha",
            &["A"],
            None,
            None,
            &[],
            10,
        ),
        local_deck_track(
            "/tmp/music/b-beta.flac",
            "Beta",
            &["Filtered Artist"],
            None,
            None,
            &[],
            11,
        ),
    ]);
    app.local_mode.ui.filter_query = "filtered".to_owned();

    let mut cmds = app.update(Msg::Key(key(KeyCode::Char('A'))));

    admit_player_transition(&mut app, &mut cmds);
    assert_eq!(app.queue.len(), 1);
    assert_eq!(app.queue.current().map(|s| s.title.as_str()), Some("Beta"));
    assert!(
        load_url(&cmds)
            .expect("filtered local load")
            .contains("b-beta")
    );
}

#[test]
fn local_deck_p_plays_selected_collection_now() {
    let mut first = local_deck_track(
        "/tmp/music/Daft Punk/Discovery/01 One More Time.flac",
        "One More Time",
        &["Daft Punk"],
        Some("Discovery"),
        Some("Daft Punk"),
        &["House"],
        10,
    );
    first.track_no = Some(1);
    let mut second = local_deck_track(
        "/tmp/music/Daft Punk/Discovery/02 Aerodynamic.flac",
        "Aerodynamic",
        &["Daft Punk"],
        Some("Discovery"),
        Some("Daft Punk"),
        &["House"],
        11,
    );
    second.track_no = Some(2);
    let mut app = app_with_local_deck_index(vec![second, first]);
    app.update(Msg::Key(key(KeyCode::Char('3'))));

    let mut cmds = app.update(Msg::Key(key(KeyCode::Char('P'))));

    admit_player_transition(&mut app, &mut cmds);
    assert_eq!(app.mode, Mode::Player);
    assert_eq!(app.queue.len(), 2);
    assert_eq!(
        app.queue.current().map(|song| song.title.as_str()),
        Some("One More Time")
    );
    assert!(load_url(&cmds).expect("album load").contains("01 One"));
}

#[test]
fn local_deck_s_shuffles_current_view_from_selected_row() {
    let mut app = app_with_local_deck_index(vec![
        local_deck_track(
            "/tmp/music/a-alpha.flac",
            "Alpha",
            &["A"],
            None,
            None,
            &[],
            10,
        ),
        local_deck_track(
            "/tmp/music/b-beta.flac",
            "Beta",
            &["B"],
            None,
            None,
            &[],
            11,
        ),
        local_deck_track(
            "/tmp/music/c-gamma.flac",
            "Gamma",
            &["C"],
            None,
            None,
            &[],
            12,
        ),
    ]);
    app.update(Msg::Key(key(KeyCode::Down)));

    let before = serde_json::to_value(app.queue.snapshot()).unwrap();
    let before_rev = app.queue.rev();
    let rejected = app.update(Msg::Key(key(KeyCode::Char('s'))));
    assert_eq!(serde_json::to_value(app.queue.snapshot()).unwrap(), before);
    assert_eq!(app.queue.rev(), before_rev);
    assert_eq!(app.mode, Mode::Library, "mode changes only after admission");
    assert!(!app.queue.shuffle, "shuffle changes only after admission");
    assert!(
        reject_player_transition(
            &mut app,
            rejected,
            crate::util::delivery::DeliveryError::Busy,
        )
        .is_empty()
    );
    assert_eq!(serde_json::to_value(app.queue.snapshot()).unwrap(), before);
    assert_eq!(app.queue.rev(), before_rev);
    assert_eq!(app.mode, Mode::Library);
    assert!(!app.queue.shuffle);

    let mut cmds = app.update(Msg::Key(key(KeyCode::Char('s'))));
    admit_player_transition(&mut app, &mut cmds);

    assert_eq!(app.mode, Mode::Player);
    assert!(app.queue.shuffle);
    assert_ne!(app.queue.rev(), before_rev);
    assert_eq!(app.queue.len(), 3);
    assert_eq!(app.queue.current().map(|s| s.title.as_str()), Some("Beta"));
    assert!(
        load_url(&cmds)
            .expect("shuffled local load")
            .contains("b-beta")
    );
    assert!(
        cmds.iter()
            .any(|cmd| matches!(cmd, Cmd::Persist(PersistCmd::Config(_))))
    );
}

#[test]
fn local_deck_c_opens_queue_popup_and_space_toggles_pause() {
    let mut app = app_with_local_deck_index(vec![local_deck_track(
        "/tmp/music/local-alpha.flac",
        "Local Alpha",
        &["Local Artist"],
        None,
        None,
        &[],
        10,
    )]);
    app.queue.set(songs(1), 0);
    let mut load = app.load_song(app.queue.current().cloned());
    admit_player_transition(&mut app, &mut load);
    app.mode = Mode::Library;

    app.update(Msg::Key(key(KeyCode::Char('c'))));
    assert!(app.queue_popup.open);

    app.queue_popup.open = false;
    app.playback.paused = false;
    let cmds = app.update(Msg::Key(key(KeyCode::Char(' '))));

    assert!(!app.playback.paused);
    assert!(matches!(
        cmds.as_slice(),
        [cmd] if matches!(
            cmd.player_command(),
            Some(PlayerCmd::SetProperty { name, value })
                if name == "pause" && value == &serde_json::Value::Bool(true)
        )
    ));
    app.admit_player_intents_for_test(&cmds);
    assert!(app.playback.paused);
}

#[test]
fn local_deck_collection_context_menu_can_enqueue_it() {
    let mut first = local_deck_track(
        "/tmp/music/Daft Punk/Discovery/01 One More Time.flac",
        "One More Time",
        &["Daft Punk"],
        Some("Discovery"),
        Some("Daft Punk"),
        &["House"],
        10,
    );
    first.track_no = Some(1);
    let mut second = local_deck_track(
        "/tmp/music/Daft Punk/Discovery/02 Aerodynamic.flac",
        "Aerodynamic",
        &["Daft Punk"],
        Some("Discovery"),
        Some("Daft Punk"),
        &["House"],
        11,
    );
    second.track_no = Some(2);
    let mut app = app_with_local_deck_index(vec![second, first]);
    app.update(Msg::Key(key(KeyCode::Char('3'))));
    render_app(&app);
    let (col, row) = button_center(&app, MouseTarget::LocalRow(0));

    let open_cmds = app.update(Msg::MouseRightClick { col, row });

    assert!(open_cmds.is_empty());
    assert!(app.overlays.context_menu.is_some());
    assert_eq!(
        app.queue.len(),
        0,
        "opening the menu must not enqueue tracks"
    );

    // Local collection menus contain "Activate" followed by "Add to queue".
    let mut cmds = choose_context_menu_item(&mut app, 1);
    admit_player_transition(&mut app, &mut cmds);
    assert_eq!(app.queue.len(), 2);
    assert_eq!(
        app.queue.current().map(|song| song.title.as_str()),
        Some("One More Time")
    );
    assert!(
        load_url(&cmds)
            .expect("context-menu album load")
            .contains("01 One")
    );
}

#[test]
fn local_download_seed_context_menu_rejects_an_index_shift() {
    let mut app = App::new(100);
    app.mode = Mode::Library;
    admit_local_mode_confirm(&mut app, LocalModeConfirm::Enter);
    app.local_mode.ui.section = LocalSection::Tracks;
    app.library_ui.downloaded = vec![local_song("old-track")];
    render_app(&app);
    let (col, row) = button_center(&app, MouseTarget::LocalRow(0));

    app.update(Msg::MouseRightClick { col, row });
    assert!(app.overlays.context_menu.is_some());

    // DownloadSeed rows carry an index; inserting at the front must not let the old menu act
    // on the replacement now occupying that index.
    app.library_ui.downloaded.insert(0, local_song("new-track"));
    let cmds = choose_context_menu_item(&mut app, 1); // Add to queue

    assert!(cmds.is_empty());
    assert!(app.queue.is_empty());
    assert_eq!(app.status.kind, StatusKind::Info);
    assert!(
        app.status.text.contains("list changed") || app.status.text.contains("목록이 바뀌었어요")
    );
}

#[test]
fn local_deck_switch_stops_playback_and_restores_cached_queues() {
    let mut app = app_playing(3, 1);
    app.playback.paused = false;
    app.streaming.pending = true;
    app.streaming.pending_rerank = Some(PendingRerank {
        seed_video_id: "id1".to_owned(),
        shortlist: Vec::new(),
        local_pick: Vec::new(),
        cid_map: Vec::new(),
        mode: crate::streaming::config::StreamingMode::Balanced,
        cache_key: 42,
    });

    let before_enter = mode_switch_projection(&app);
    let mut enter = app.apply_local_mode_confirm(LocalModeConfirm::Enter);

    assert_eq!(mode_switch_projection(&app), before_enter);
    assert!(!app.local_dedicated_mode);
    assert!(has_stop(&enter), "entering Local Deck should stop mpv");
    assert_eq!(app.queue.len(), 3);
    assert!(app.streaming.pending);
    assert!(app.streaming.pending_rerank.is_some());
    admit_player_transition(&mut app, &mut enter);

    assert!(app.local_dedicated_mode);
    assert_eq!(app.mode, Mode::Library);
    assert!(app.queue.is_empty());
    assert!(load_url(&enter).is_none());
    assert!(!app.streaming.pending);
    assert!(app.streaming.pending_rerank.is_none());

    app.queue
        .set(vec![local_song("local_alpha"), local_song("local_beta")], 1);
    let mut local_load = app.load_song(app.queue.current().cloned());
    admit_player_transition(&mut app, &mut local_load);
    app.playback.paused = false;
    let before_exit = mode_switch_projection(&app);
    let mut exit = app.apply_local_mode_confirm(LocalModeConfirm::Exit);

    assert_eq!(mode_switch_projection(&app), before_exit);
    assert!(app.local_dedicated_mode);
    assert!(has_stop(&exit), "leaving Local Deck should stop mpv");
    assert_stop_before_load(&exit);
    admit_player_transition(&mut app, &mut exit);

    assert!(!app.local_dedicated_mode);
    assert_eq!(app.queue.len(), 3);
    assert_eq!(current(&app), "id1");
    assert!(
        load_url(&exit)
            .expect("restored normal load")
            .contains("id1")
    );
    assert!(!app.playback.paused);

    app.queue.set(songs(1), 0);
    let mut normal_load = app.load_song(app.queue.current().cloned());
    admit_player_transition(&mut app, &mut normal_load);
    let before_reenter = mode_switch_projection(&app);
    let mut reenter = app.apply_local_mode_confirm(LocalModeConfirm::Enter);

    assert_eq!(mode_switch_projection(&app), before_reenter);
    assert!(!app.local_dedicated_mode);
    assert!(has_stop(&reenter));
    assert_stop_before_load(&reenter);
    admit_player_transition(&mut app, &mut reenter);

    assert!(app.local_dedicated_mode);
    assert_eq!(app.queue.len(), 2);
    assert_eq!(
        app.queue.current().map(|s| s.title.as_str()),
        Some("local_beta")
    );
    assert!(
        load_url(&reenter)
            .expect("restored local load")
            .contains("/tmp/local_beta.m4a")
    );
}

#[test]
fn local_deck_switch_busy_and_closed_preserve_state_until_retry() {
    use crate::util::delivery::DeliveryError;

    for error in [DeliveryError::Busy, DeliveryError::Closed] {
        let mut app = app_playing(3, 1);
        app.mode = Mode::Player;
        app.local_mode.pending_confirm = Some(LocalModeConfirm::Enter);
        app.streaming.pending = true;
        app.queue_popup.open = true;
        app.art.force_clear_next_frame = false;
        let before = mode_switch_projection(&app);
        let before_rev = app.queue.rev();

        let cmds = app.apply_local_mode_confirm(LocalModeConfirm::Enter);

        assert!(has_stop(&cmds));
        assert_eq!(mode_switch_projection(&app), before);
        assert_eq!(
            app.local_mode.pending_confirm,
            Some(LocalModeConfirm::Enter)
        );
        assert!(app.queue_popup.open);
        assert!(!app.art.force_clear_next_frame);

        assert!(reject_player_transition(&mut app, cmds, error).is_empty());
        assert_eq!(mode_switch_projection(&app), before);
        assert_eq!(app.queue.rev(), before_rev);
        assert_eq!(
            app.local_mode.pending_confirm,
            Some(LocalModeConfirm::Enter)
        );
        assert!(app.queue_popup.open);
        assert!(!app.art.force_clear_next_frame);
        assert_eq!(app.status.kind, StatusKind::Error);
        assert!(!app.status.text.is_empty());

        let mut retry = app.apply_local_mode_confirm(LocalModeConfirm::Enter);
        admit_player_transition(&mut app, &mut retry);
        assert!(app.local_dedicated_mode);
        assert!(app.local_mode.pending_confirm.is_none());
        assert_eq!(app.mode, Mode::Library);
        assert!(app.queue.is_empty());
        assert_ne!(app.queue.rev(), before_rev);
        assert!(!app.streaming.pending);
        assert!(!app.queue_popup.open);
        assert!(app.art.force_clear_next_frame);
        assert_eq!(app.status.kind, StatusKind::Info);
    }
}

#[test]
fn local_mode_switch_retires_video_pause_ownership_only_after_batch_admission() {
    use crate::util::delivery::DeliveryError;

    for error in [
        DeliveryError::Busy,
        DeliveryError::Saturated,
        DeliveryError::Closed,
    ] {
        let mut app = app_playing(1, 0);
        let mut open = app.toggle_video_overlay_with_fake_spawn(true);
        admit_player_transition(&mut app, &mut open);
        assert!(app.video_open());
        assert!(app.video_pause_owned_for_test());
        app.local_mode.pending_confirm = Some(LocalModeConfirm::Enter);

        let cmds = app.apply_local_mode_confirm(LocalModeConfirm::Enter);
        assert!(cmds.iter().flat_map(Cmd::player_commands).any(|command| {
            matches!(
                command,
                PlayerCmd::SetProperty { name, value }
                    if name == "pause" && value == &serde_json::Value::Bool(false)
            )
        }));

        assert!(reject_player_transition(&mut app, cmds, error).is_empty());
        assert!(
            app.video_open(),
            "rejected mode switch must keep the overlay"
        );
        assert!(app.video_pause_owned_for_test());
        assert!(!app.local_dedicated_mode);

        let mut retry = app.apply_local_mode_confirm(LocalModeConfirm::Enter);
        admit_player_transition(&mut app, &mut retry);
        assert!(!app.video_open(), "accepted mode switch closes stale video");
        assert!(!app.video_pause_owned_for_test());
        assert!(app.local_dedicated_mode);
        assert!(app.queue.is_empty());
        assert!(app.playback.paused);
    }
}

#[test]
fn nonempty_mode_restore_resumes_the_replacement_after_retiring_video() {
    let mut app = app_playing(1, 0);
    app.local_mode.local_mode_queue = Some(app.queue.snapshot());
    let mut open = app.toggle_video_overlay_with_fake_spawn(true);
    admit_player_transition(&mut app, &mut open);
    assert!(app.playback.paused);
    assert!(app.video_pause_owned_for_test());

    app.local_mode.pending_confirm = Some(LocalModeConfirm::Enter);
    let mut switch = app.apply_local_mode_confirm(LocalModeConfirm::Enter);
    assert!(load_url(&switch).is_some());
    admit_player_transition(&mut app, &mut switch);

    assert!(app.local_dedicated_mode);
    assert!(!app.video_open());
    assert!(!app.video_pause_owned_for_test());
    assert!(!app.playback.paused);
    assert_eq!(app.prefetch.loaded_video_id.as_deref(), Some("id0"));
}

#[test]
fn local_deck_session_snapshot_and_restore_use_local_queue() {
    let mut app = app_playing(2, 1);
    admit_local_mode_confirm(&mut app, LocalModeConfirm::Enter);
    app.queue
        .set(vec![local_song("local_alpha"), local_song("local_beta")], 1);

    let cache = app.session_cache_snapshot();

    assert_eq!(cache.last_mode, crate::session::LastMode::Local);
    assert_eq!(cache.normal_queue.as_ref().map(|s| s.songs.len()), Some(2));
    assert_eq!(cache.local_queue.as_ref().map(|s| s.cursor), Some(1));

    let mut restored = App::new(100);
    restored.restore_last_session_from_cache(&cache);

    assert!(restored.local_dedicated_mode);
    assert_eq!(restored.mode, Mode::Library);
    assert_eq!(restored.queue.len(), 2);
    assert_eq!(
        restored.queue.current().map(|s| s.title.as_str()),
        Some("local_beta")
    );
    assert!(restored.playback.paused);
}

#[test]
fn restoring_empty_local_session_does_not_fall_back_to_normal_history() {
    let mut app = App::new(100);
    app.library.record_play(&songs(1)[0]);
    let cache = crate::session::SessionCache::from_last_mode(crate::session::LastMode::Local);

    app.restore_last_session_from_cache(&cache);

    assert!(app.local_dedicated_mode);
    assert!(app.queue.is_empty());
}

#[test]
fn settings_local_music_root_persists_and_rescans_active_local_deck() {
    let mut app = App::new(100);
    let downloads = PathBuf::from("/tmp/ytt-local-downloads");
    let music = PathBuf::from("/tmp/ytt-local-library");
    app.config.download_dir = Some(downloads.clone());
    app.local_dedicated_mode = true;
    app.open_settings();

    focus_settings_field(&mut app, SettingsTab::General, Field::LocalMusicRoot);
    {
        let draft = &mut app.settings.as_mut().unwrap().draft;
        draft.local_music_root = music.display().to_string();
        draft.local_music_root_recursive = false;
    }

    let cmds = app.settings_persist_text_field(Field::LocalMusicRoot);

    assert_eq!(app.config.local.roots.len(), 1);
    assert_eq!(app.config.local.roots[0].path, music);
    assert!(!app.config.local.roots[0].recursive());
    let Some(Cmd::Local(LocalCmd::ScanRoots { roots, .. })) = cmds
        .iter()
        .find(|cmd| matches!(cmd, Cmd::Local(LocalCmd::ScanRoots { .. })))
    else {
        panic!("expected Local Deck rescan after changing the music root");
    };
    assert_eq!(
        roots,
        &vec![
            crate::local::LocalScanRoot::download(downloads),
            crate::local::LocalScanRoot {
                path: PathBuf::from("/tmp/ytt-local-library"),
                recursive: false,
                max_depth: None,
            },
        ]
    );
    assert!(save_config(&cmds).is_some());
}

#[test]
fn closing_settings_with_local_root_toggles_rescans_active_local_deck() {
    let mut app = App::new(100);
    let downloads = PathBuf::from("/tmp/ytt-close-downloads");
    let music = PathBuf::from("/tmp/ytt-close-library");
    app.config.download_dir = Some(downloads);
    app.config.local.roots = vec![crate::config::LocalRootConfig {
        path: music.clone(),
        enabled: Some(true),
        recursive: Some(true),
    }];
    app.local_dedicated_mode = true;
    app.open_settings();
    {
        let draft = &mut app.settings.as_mut().unwrap().draft;
        draft.local_include_download_dir = false;
        draft.local_music_root_recursive = false;
    }

    let mut cmds = app.close_settings();
    admit_player_transition(&mut app, &mut cmds);

    assert!(!app.config.local.include_download_dir());
    assert!(!app.config.local.roots[0].recursive());
    let Some(Cmd::Local(LocalCmd::ScanRoots { roots, .. })) = cmds
        .iter()
        .find(|cmd| matches!(cmd, Cmd::Local(LocalCmd::ScanRoots { .. })))
    else {
        panic!("expected Local Deck rescan after changing local root toggles");
    };
    assert_eq!(
        roots,
        &vec![crate::local::LocalScanRoot {
            path: music,
            recursive: false,
            max_depth: None,
        }]
    );
}

#[test]
fn local_deck_linked_track_artwork_still_uses_local_file_source() {
    let mut app = App::new(100);
    app.config.album_art = Some(true);
    app.art.picker = Some(Picker::halfblocks());
    let mut track = crate::local::LocalTrack::untagged(
        std::path::PathBuf::from("/music/linked-song.m4a"),
        10,
        20,
    );
    track.linked_video_id = Some("abcdefghijk".to_owned());
    let song = track.to_song();

    assert!(song.youtube_id().is_some());
    assert!(matches!(
        app.artwork_source(&song),
        Some(ArtSource::Local(path)) if path.ends_with("linked-song.m4a")
    ));
}
