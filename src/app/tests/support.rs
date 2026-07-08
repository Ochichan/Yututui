use super::*;

pub(super) fn key(code: KeyCode) -> KeyEvent {
    KeyEvent {
        code,
        modifiers: KeyModifiers::NONE,
        kind: KeyEventKind::Press,
        state: KeyEventState::NONE,
    }
}

pub(super) fn ctrl(code: KeyCode) -> KeyEvent {
    KeyEvent {
        code,
        modifiers: KeyModifiers::CONTROL,
        kind: KeyEventKind::Press,
        state: KeyEventState::NONE,
    }
}

pub(super) fn alt_shift(code: KeyCode) -> KeyEvent {
    KeyEvent {
        code,
        modifiers: KeyModifiers::ALT | KeyModifiers::SHIFT,
        kind: KeyEventKind::Press,
        state: KeyEventState::NONE,
    }
}

pub(super) fn shift(code: KeyCode) -> KeyEvent {
    KeyEvent {
        code,
        modifiers: KeyModifiers::SHIFT,
        kind: KeyEventKind::Press,
        state: KeyEventState::NONE,
    }
}

/// The `af` chain set by a `SetAudioFilter` command among `cmds`, if any.
pub(super) fn af(cmds: &[Cmd]) -> Option<&str> {
    cmds.iter().find_map(|c| match c {
        Cmd::Player(PlayerCmd::SetAudioFilter(s)) => Some(s.as_str()),
        _ => None,
    })
}

/// The URL of the `Load` command among `cmds`, if any. (A load now also emits
/// `SaveLibrary`, so tests look for the Load rather than an exact one-element match.)
pub(super) fn load_url(cmds: &[Cmd]) -> Option<&str> {
    cmds.iter().find_map(|c| match c {
        Cmd::Player(PlayerCmd::Load(u)) => Some(u.as_str()),
        _ => None,
    })
}

pub(super) fn load_watch_video_id(cmds: &[Cmd]) -> Option<String> {
    let url = reqwest::Url::parse(load_url(cmds)?).ok()?;
    let is_youtube_watch = matches!(
        url.host_str(),
        Some("music.youtube.com" | "www.youtube.com" | "youtube.com" | "m.youtube.com")
    ) && url.path() == "/watch";
    is_youtube_watch
        .then(|| {
            url.query_pairs()
                .find_map(|(key, value)| (key == "v").then(|| value.into_owned()))
        })
        .flatten()
}

pub(super) fn assert_loads_video(cmds: &[Cmd], expected_video_id: &str) {
    let load = load_url(cmds);
    assert_eq!(
        load_watch_video_id(cmds).as_deref(),
        Some(expected_video_id),
        "expected Load command for video id {expected_video_id}, got {load:?}"
    );
}

pub(super) fn assert_no_load(cmds: &[Cmd]) {
    let load = load_url(cmds);
    assert_eq!(load, None, "expected no Load command, got {load:?}");
}

pub(super) fn has_stop(cmds: &[Cmd]) -> bool {
    cmds.iter()
        .any(|c| matches!(c, Cmd::Player(PlayerCmd::Stop)))
}

pub(super) fn confirm_on_f5_keymap() -> KeyMap {
    let mut keymap = KeyMap::default();
    keymap
        .rebind(
            KeyContext::Common,
            Action::Confirm,
            crate::keymap::parse_chord("f5").unwrap(),
        )
        .unwrap();
    keymap
}

pub(super) fn app_with_three_favorites() -> App {
    let mut app = app_playing(2, 0);
    app.library.favorites = vec![
        Song::remote("f0", "F0", "A", "3:00"),
        Song::remote("f1", "F1", "B", "3:00"),
        Song::remote("f2", "F2", "C", "3:00"),
    ];
    app.mode = Mode::Library;
    app.library_ui.tab = LibraryTab::Favorites;
    app.library_ui.selected = 0;
    app.library_ui.anchor = 0;
    app
}

pub(super) fn songs(n: usize) -> Vec<Song> {
    (0..n)
        .map(|i| Song::remote(format!("id{i}"), format!("t{i}"), "a", "0:10"))
        .collect()
}

pub(super) fn radio_station(id: &str) -> Song {
    Song::from_source(
        SearchSource::RadioBrowser,
        id,
        format!("Station {id}"),
        "KR / MP3",
        "",
        crate::api::PlayableRef::RadioStream {
            url: format!("https://example.com/{id}.mp3"),
        },
    )
}

/// An app with an `n`-track queue, playing track `start`. Builds the queue directly so
/// it stays independent of how individual play paths populate the queue (e.g. search-play
/// only queues the one picked track).
pub(super) fn app_playing(n: usize, start: usize) -> App {
    let mut app = App::new(100);
    app.queue.set(songs(n), start);
    app.mode = Mode::Player;
    let song = app.queue.current().cloned();
    app.load_song(song);
    app
}

pub(super) fn current(app: &App) -> &str {
    app.queue.current().unwrap().video_id.as_str()
}

// ---- Radio recorder state machine --------------------------------------------------------

/// A radio station playing with recording enabled in `mode` and a scratch temp dir. The
/// reducer only builds `Cmd`s (no disk IO), so the temp dir is never actually written.
pub(super) fn recording_app(mode: crate::recorder::RecordingMode) -> App {
    let mut app = App::new(100);
    app.queue.set(vec![radio_station("groove")], 0);
    app.load_song(app.queue.current().cloned());
    app.recorder.supported = true;
    app.recorder.temp_dir = std::path::PathBuf::from("/tmp/ytt-rec-test");
    app.config.recording.mode = mode;
    app
}

pub(super) fn feed_title(app: &mut App, title: &str) -> Vec<Cmd> {
    app.update(PlayerMsg::Metadata(
        serde_json::json!({ "icy-title": title }),
    ))
}

/// Pretend the open segment started `secs` ago so the min/max filters see real duration.
pub(super) fn backdate_current(app: &mut App, secs: u64) {
    if let Some(seg) = app.recorder.current.as_mut() {
        seg.started_at = seg
            .started_at
            .checked_sub(std::time::Duration::from_secs(secs))
            .unwrap_or(seg.started_at);
    }
}

pub(super) fn emits_stream_record_clear(cmds: &[Cmd]) -> bool {
    cmds.iter().any(|c| {
        matches!(c, Cmd::Player(crate::player::PlayerCmd::SetProperty { name, value })
            if name == "stream-record" && value == &serde_json::Value::from(""))
    })
}

pub(super) fn radio_playing(id: &str) -> App {
    let mut app = App::new(100);
    app.queue.set(vec![radio_station(id)], 0);
    app.load_song(app.queue.current().cloned());
    app
}

pub(super) fn radio_with_title(title: &str) -> App {
    let mut app = radio_playing("groove");
    app.update(PlayerMsg::Metadata(
        serde_json::json!({ "icy-title": title }),
    ));
    app
}

/// A radio app with the card already open on a playing song (DJ Gem OFF).
pub(super) fn radio_card(title: &str) -> App {
    let mut app = radio_with_title(title);
    app.update(Msg::Key(key(KeyCode::Char('i'))));
    app
}

pub(super) fn resolve_track_cmd(cmds: &[Cmd]) -> Option<(u64, &str)> {
    cmds.iter().find_map(|c| match c {
        Cmd::ResolveTrack { seq, query, .. } => Some((*seq, query.as_str())),
        _ => None,
    })
}

pub(super) fn ask_ai_prompt(cmds: &[Cmd]) -> Option<&str> {
    cmds.iter().find_map(|c| match c {
        Cmd::AskAi { prompt, .. } => Some(prompt.as_str()),
        _ => None,
    })
}

pub(super) const EXTRACTION_ERR: &str = "mpv could not play this track (unrecognized file format)";

pub(super) fn heal_cmd_id(cmds: &[Cmd]) -> Option<&str> {
    cmds.iter().find_map(|c| match c {
        Cmd::YtdlpSelfHeal { video_id, .. } => Some(video_id.as_str()),
        _ => None,
    })
}

pub(super) fn resolve_cmd_id(cmds: &[Cmd]) -> Option<&str> {
    cmds.iter().find_map(|c| match c {
        Cmd::Resolve { video_id, .. } => Some(video_id.as_str()),
        _ => None,
    })
}

pub(super) fn buffer_row(buf: &ratatui::buffer::Buffer, y: u16) -> String {
    (0..buf.area.width)
        .map(|x| buf.cell((x, y)).map(|c| c.symbol()).unwrap_or(" "))
        .collect::<Vec<_>>()
        .join("")
}

pub(super) fn save_config(cmds: &[Cmd]) -> Option<&Config> {
    cmds.iter().find_map(|c| match c {
        Cmd::Persist(PersistCmd::Config(c)) => Some(c.as_ref()),
        _ => None,
    })
}

pub(super) fn focus_settings_field(app: &mut App, tab: SettingsTab, field: Field) {
    if app.settings.is_none() {
        app.open_settings();
    }
    let st = app.settings.as_mut().expect("settings open");
    st.tab = tab;
    st.row = st
        .fields()
        .iter()
        .position(|f| *f == field)
        .unwrap_or_else(|| panic!("{field:?} is visible on {tab:?}"));
    st.editing_text = false;
    st.capturing = None;
    assert_eq!(st.current_field(), Some(field));
}

pub(super) fn focus_reset_all(app: &mut App) {
    app.update(Msg::Key(key(KeyCode::Char('o')))); // open settings (General tab)
    for _ in 0..SettingsTab::General.fields().len() - 1 {
        app.update(Msg::Key(key(KeyCode::Down)));
    }
    assert_eq!(
        app.settings.as_ref().unwrap().current_field(),
        Some(Field::ResetAll)
    );
}

/// Move the General-tab cursor onto the Reset-keybindings button.
pub(super) fn focus_reset_keybindings(app: &mut App) {
    app.update(Msg::Key(key(KeyCode::Char('o')))); // open settings (General tab)
    let idx = SettingsTab::General
        .fields()
        .iter()
        .position(|f| *f == Field::ResetKeybindings)
        .expect("reset keybindings field");
    for _ in 0..idx {
        app.update(Msg::Key(key(KeyCode::Down)));
    }
    assert_eq!(
        app.settings.as_ref().unwrap().current_field(),
        Some(Field::ResetKeybindings)
    );
}

pub(super) fn ask_ai(cmds: &[Cmd]) -> Option<&str> {
    cmds.iter().find_map(|c| match c {
        Cmd::AskAi { prompt, .. } => Some(prompt.as_str()),
        _ => None,
    })
}

pub(super) fn streaming_fallback(cmds: &[Cmd]) -> Option<(&str, &str, &[String])> {
    cmds.iter().find_map(|c| match c {
        Cmd::StreamingFallback {
            seed,
            seed_video_id,
            exclude_ids,
            ..
        } => Some((
            seed.as_str(),
            seed_video_id.as_str(),
            exclude_ids.as_slice(),
        )),
        _ => None,
    })
}

/// The `(seed_video_id, prompt)` of the `AiRerank` command among `cmds`, if any.
pub(super) fn ai_rerank(cmds: &[Cmd]) -> Option<(&str, &str)> {
    cmds.iter().find_map(|c| match c {
        Cmd::AiRerank {
            seed_video_id,
            prompt,
        } => Some((seed_video_id.as_str(), prompt.as_str())),
        _ => None,
    })
}

pub(super) fn fsong(id: &str, title: &str, artist: &str) -> Song {
    Song::remote(id, title, artist, "0:10")
}

/// Opens the Favorites tab with the given favorites (set directly for a deterministic order).
pub(super) fn app_with_favorites(favs: Vec<Song>) -> App {
    let mut app = App::new(100);
    app.library.favorites = favs;
    app.update(Msg::Key(key(KeyCode::Char('l')))); // open library (All)
    app.update(Msg::Key(key(KeyCode::Tab))); // All -> Favorites
    assert_eq!(app.library_ui.tab, LibraryTab::Favorites);
    app
}

pub(super) fn row_ids(app: &App) -> Vec<String> {
    app.library_rows()
        .iter()
        .map(|s| s.video_id.clone())
        .collect()
}

pub(super) fn app_with_search_results() -> App {
    let mut app = App::new(100);
    app.mode = Mode::Search;
    app.update(Msg::SearchResults {
        request_id: app.search.request_id,
        query: "x".to_owned(),
        source: SearchSource::Youtube,
        timed_out: false,
        songs: vec![
            fsong("a", "Lovely", "Billie Eilish"),
            fsong("b", "Bad Guy", "Billie Eilish"),
            fsong("c", "Anti-Hero", "Taylor Swift"),
        ],
    });
    assert_eq!(app.search.focus, SearchFocus::Results);
    app
}

pub(super) fn filter_row_ids(app: &App) -> Vec<String> {
    app.search_filter_rows()
        .iter()
        .map(|(_, s)| s.video_id.clone())
        .collect()
}

pub(super) fn bare_local(path: &str, title: &str) -> Song {
    Song {
        video_id: format!("local:{path}"),
        title: title.to_owned(),
        artist: "Local file".to_owned(),
        duration: String::new(),
        album: None,
        duration_secs: None,
        source: SearchSource::Youtube,
        playable: None,
        local_path: Some(PathBuf::from(path)),
        yt_video_id: None,
    }
}

pub(super) fn temp_audio_file(tag: &str) -> PathBuf {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let path = std::env::temp_dir().join(format!(
        "yututui-app-test-{}-{tag}-{nanos}.m4a",
        std::process::id()
    ));
    std::fs::write(&path, b"").unwrap();
    path
}

/// Open the library and switch to `tab` by tab-key presses (All is the entry tab).
pub(super) fn open_library_tab(app: &mut App, tab: LibraryTab) {
    app.update(Msg::Key(key(KeyCode::Char('l'))));
    while app.library_ui.tab != tab {
        app.update(Msg::Key(key(KeyCode::Tab)));
    }
}

pub(super) fn lyric_lines() -> Vec<LyricLine> {
    vec![
        LyricLine {
            time: 0.0,
            text: "one".to_owned(),
        },
        LyricLine {
            time: 5.0,
            text: "two".to_owned(),
        },
    ]
}

pub(super) fn resolve_cmd<'a>(cmds: &'a [Cmd], id: &str) -> Option<&'a str> {
    cmds.iter().find_map(|c| match c {
        Cmd::Resolve {
            video_id,
            watch_url,
        } if video_id == id => Some(watch_url.as_str()),
        _ => None,
    })
}

pub(super) fn rendered_help_cluster(app: &App, width: u16, height: u16) -> Rect {
    let backend = TestBackend::new(width, height);
    let mut terminal = Terminal::new(backend).unwrap();
    terminal.draw(|f| crate::ui::render(f, app)).unwrap();

    let buttons = app.hits.regions();
    let key = buttons
        .iter()
        .find(|b| b.target == MouseTarget::Global(Action::ToggleHelp))
        .map(|b| b.rect)
        .expect("rendered key help button");
    let mouse = buttons
        .iter()
        .find(|b| b.target == MouseTarget::MouseHelp)
        .map(|b| b.rect)
        .expect("rendered mouse help button");
    let left = key.left().min(mouse.left());
    let top = key.top().min(mouse.top());
    let right = key.right().max(mouse.right());
    let bottom = key.bottom().max(mouse.bottom());
    Rect {
        x: left,
        y: top,
        width: right.saturating_sub(left),
        height: bottom.saturating_sub(top),
    }
}

pub(super) fn assert_centered_in(rect: Rect, container: Rect) {
    let left = rect.x.saturating_sub(container.x);
    let right = container
        .x
        .saturating_add(container.width)
        .saturating_sub(rect.x.saturating_add(rect.width));
    assert_eq!(
        left, right,
        "help button should be centered in {container:?}"
    );
}

pub(super) fn configure_test_art_picker(
    app: &mut App,
    protocol: ratatui_image::picker::ProtocolType,
) {
    let mut picker = ratatui_image::picker::Picker::halfblocks();
    picker.set_protocol_type(protocol);
    app.config.album_art = Some(true);
    app.art.picker = Some(picker);
    let (tx, _rx) = tokio::sync::mpsc::channel(8);
    app.set_art_resize_tx(tx);
}

pub(super) fn make_test_art_active(app: &mut App, protocol: ratatui_image::picker::ProtocolType) {
    configure_test_art_picker(app, protocol);
    let video_id = app.queue.current().unwrap().video_id.clone();
    app.set_artwork(video_id, Some(image::DynamicImage::new_rgba8(32, 32)));
    app.art.overlay_mask = app.art_overlay_mask();
    app.art.force_clear_next_frame = false;
    app.art.overlay_refresh_clear_frames = 0;
    app.dirty = false;
}

pub(super) fn assert_art_refresh_clear_burst(app: &mut App, context: &str) {
    for frame in 1..=3 {
        assert!(
            app.clear_before_draw_pending(),
            "{context}: pending flag should keep redraw loop awake before frame {frame}"
        );
        assert!(
            app.take_clear_before_draw(),
            "{context}: expected reinforced clear frame {frame}"
        );
    }
    assert!(
        !app.clear_before_draw_pending(),
        "{context}: pending flag should drop after the burst"
    );
    assert!(
        !app.take_clear_before_draw(),
        "{context}: reinforced clear burst should be short"
    );
}

pub(super) fn render_app(app: &App) {
    let backend = TestBackend::new(80, 24);
    let mut terminal = Terminal::new(backend).unwrap();
    terminal.draw(|f| crate::ui::render(f, app)).unwrap();
}

pub(super) fn render_app_buffer(app: &App, width: u16, height: u16) -> ratatui::buffer::Buffer {
    let backend = TestBackend::new(width, height);
    let mut terminal = Terminal::new(backend).unwrap();
    terminal.draw(|f| crate::ui::render(f, app)).unwrap();
    terminal.backend().buffer().clone()
}

pub(super) fn buffer_contains(buf: &ratatui::buffer::Buffer, needle: &str) -> bool {
    (0..buf.area.height).any(|y| buffer_row(buf, y).contains(needle))
}

pub(super) fn assert_opaque_rect(buffer: &ratatui::buffer::Buffer, rect: ratatui::layout::Rect) {
    for y in rect.top()..rect.bottom() {
        for x in rect.left()..rect.right() {
            let cell = buffer.cell((x, y)).expect("cell is inside the buffer");
            assert_ne!(
                cell.bg,
                ratatui::style::Color::Reset,
                "popup cell at ({x},{y}) kept the default background"
            );
        }
    }
}

pub(super) fn assert_rgb_at_least(color: ratatui::style::Color, min: (u8, u8, u8)) {
    let ratatui::style::Color::Rgb(r, g, b) = color else {
        panic!("expected RGB color, got {color:?}");
    };
    assert!(
        r >= min.0 && g >= min.1 && b >= min.2,
        "expected color channels at least {min:?}, got ({r},{g},{b})"
    );
}

pub(super) fn dropdown_popup_rect(
    app: &App,
    mut is_row: impl FnMut(MouseTarget) -> bool,
) -> ratatui::layout::Rect {
    let rects: Vec<_> = app
        .hits
        .regions()
        .iter()
        .filter_map(|b| is_row(b.target).then_some(b.rect))
        .collect();
    assert!(
        !rects.is_empty(),
        "dropdown row rects were not registered; targets: {:?}",
        app.hits
            .regions()
            .iter()
            .map(|b| b.target)
            .collect::<Vec<_>>()
    );

    let left = rects.iter().map(|r| r.left()).min().unwrap();
    let top = rects.iter().map(|r| r.top()).min().unwrap();
    let right = rects.iter().map(|r| r.right()).max().unwrap();
    let bottom = rects.iter().map(|r| r.bottom()).max().unwrap();
    ratatui::layout::Rect::new(
        left.saturating_sub(1),
        top.saturating_sub(1),
        right - left + 2,
        bottom - top + 2,
    )
}

pub(super) fn centered_percent(
    area: ratatui::layout::Rect,
    pct_w: u16,
    pct_h: u16,
) -> ratatui::layout::Rect {
    let w = area.width * pct_w / 100;
    let h = area.height * pct_h / 100;
    ratatui::layout::Rect {
        x: area.x + area.width.saturating_sub(w) / 2,
        y: area.y + area.height.saturating_sub(h) / 2,
        width: w,
        height: h,
    }
}

pub(super) fn centered_fixed(area: ratatui::layout::Rect, w: u16, h: u16) -> ratatui::layout::Rect {
    let w = w.min(area.width);
    let h = h.min(area.height);
    ratatui::layout::Rect {
        x: area.x + area.width.saturating_sub(w) / 2,
        y: area.y + area.height.saturating_sub(h) / 2,
        width: w,
        height: h,
    }
}

pub(super) fn about_icon_rect(area: ratatui::layout::Rect) -> ratatui::layout::Rect {
    let popup = centered_fixed(area, 60, 22);
    let inner = ratatui::layout::Rect {
        x: popup.x.saturating_add(1),
        y: popup.y.saturating_add(1),
        width: popup.width.saturating_sub(2),
        height: popup.height.saturating_sub(2),
    };
    let band = ratatui::layout::Rect {
        height: 9.min(inner.height),
        ..inner
    };
    let h = band.height.clamp(1, 9);
    let w = (h * 2).min(band.width);
    ratatui::layout::Rect {
        x: band.x + band.width.saturating_sub(w) / 2,
        y: band.y + band.height.saturating_sub(h) / 2,
        width: w,
        height: h,
    }
}

/// The center cell of the hit rect registered for `target` in the last render.
pub(super) fn button_center(app: &App, target: MouseTarget) -> (u16, u16) {
    app.hits
        .regions()
        .iter()
        .find(|b| b.target == target)
        .map(|b| (b.rect.x + b.rect.width / 2, b.rect.y + b.rect.height / 2))
        .unwrap_or_else(|| panic!("no hit rect registered for {target:?}"))
}

/// Render `app`, then click the center of `target`'s hit rect.
pub(super) fn click_target(app: &mut App, target: MouseTarget) -> Vec<Cmd> {
    render_app(app);
    let (col, row) = button_center(app, target);
    app.update(Msg::MouseClick { col, row })
}

pub(super) fn double_click_target(app: &mut App, target: MouseTarget) -> Vec<Cmd> {
    render_app(app);
    let (col, row) = button_center(app, target);
    app.update(Msg::MouseDoubleClick { col, row })
}

pub(super) fn app_with_playlists() -> App {
    let mut app = App::new(100);
    app.playlists.create("Alpha");
    app.playlists.add("Alpha", fsong("a1", "Song A1", "X"));
    app.playlists.add("Alpha", fsong("a2", "Song A2", "Y"));
    app.playlists.create("Beta");
    app.playlists.add("Beta", fsong("b1", "Song B1", "Z"));
    open_library_tab(&mut app, LibraryTab::Playlists);
    app
}

pub(super) fn app_with_picker_fixture() -> App {
    let mut app = app_with_favorites(vec![
        fsong("s1", "Song One", "A"),
        fsong("s2", "Song Two", "B"),
    ]);
    app.playlists.create("Mix");
    app
}

pub(super) fn render_at(app: &App, w: u16, h: u16) -> (Vec<MouseTarget>, String) {
    let backend = TestBackend::new(w, h);
    let mut terminal = Terminal::new(backend).unwrap();
    terminal.draw(|f| crate::ui::render(f, app)).unwrap();
    let buf = terminal.backend().buffer().clone();
    let top: String = (0..w)
        .map(|x| buf.cell((x, 0)).map(|c| c.symbol()).unwrap_or(" "))
        .collect::<Vec<_>>()
        .join("");
    let targets = app.hits.regions().iter().map(|b| b.target).collect();
    (targets, top)
}

#[cfg(unix)]
pub(super) fn fake_overlay_proc() -> std::process::Child {
    std::process::Command::new("sleep")
        .arg("30")
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn()
        .expect("spawn test child")
}

pub(super) fn app_with_playlist_row() -> App {
    let mut app = App::new(100);
    app.mode = Mode::Search;
    app.search.focus = SearchFocus::Results;
    app.search.results = vec![Song::remote(
        "ytpl:PLabcdefgh1234",
        "Rainy Mix",
        "Curator",
        "12 tracks",
    )];
    app.search.selected = 0;
    app
}
