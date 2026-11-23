//! TUI polish regressions: the idle Player, the stopped status, and the shared track rows.

use super::*;

fn row_text(buffer: &ratatui::buffer::Buffer, needle: &str) -> Option<String> {
    (0..buffer.area.height)
        .map(|y| buffer_row(buffer, y))
        .find(|row| row.contains(needle))
}

fn click_target(app: &mut App, target: MouseTarget) -> Vec<Cmd> {
    let rect = app
        .hits
        .regions()
        .iter()
        .find(|region| region.target == target)
        .map(|region| region.rect)
        .unwrap_or_else(|| panic!("{target:?} not rendered"));
    app.update(Msg::MouseClick {
        col: rect.x,
        row: rect.y,
        multi: false,
    })
}

#[test]
fn idle_player_names_the_ways_in_and_they_are_clickable() {
    let _guard = crate::i18n::lock_for_test();
    for language in [
        crate::i18n::Language::English,
        crate::i18n::Language::Korean,
        crate::i18n::Language::Japanese,
    ] {
        crate::i18n::set_language(language);
        let mut app = App::new(100);
        let buffer = render_app_buffer(&app, 60, 24);
        for label in [
            crate::t!("Search for music", "음악 검색", "音楽を検索"),
            crate::t!("Open your library", "라이브러리 열기", "ライブラリを開く"),
            crate::t!("Ask DJ Gem", "DJ Gem에게 요청", "DJ Gem に頼む"),
        ] {
            let compact: String = label.chars().filter(|c| !c.is_whitespace()).collect();
            assert!(
                (0..24).any(|y| buffer_row(&buffer, y)
                    .chars()
                    .filter(|c| !c.is_whitespace())
                    .collect::<String>()
                    .contains(&compact)),
                "{language:?}: {label} missing"
            );
        }
        // The status line agrees with the title: nothing is loaded, so nothing is playing.
        let stopped: String = crate::t!("■ stopped", "■ 정지", "■ 停止中")
            .chars()
            .filter(|c| !c.is_whitespace())
            .collect();
        assert!(
            (0..24).any(|y| buffer_row(&buffer, y)
                .chars()
                .filter(|c| !c.is_whitespace())
                .collect::<String>()
                .contains(&stopped)),
            "{language:?}: {stopped} missing"
        );
        let _ = click_target(
            &mut app,
            MouseTarget::Player(crate::keymap::Action::OpenSearch),
        );
        assert_eq!(app.mode, Mode::Search, "{language:?}");
    }
    crate::i18n::set_language(crate::i18n::Language::English);
}

#[test]
fn idle_card_gives_way_to_a_loaded_track_and_the_status_says_playing() {
    let _guard = crate::i18n::lock_for_test();
    crate::i18n::set_language(crate::i18n::Language::English);
    let app = app_playing(3, 0);
    let buffer = render_app_buffer(&app, 60, 24);
    assert!(!buffer_contains(&buffer, "Start listening"));
    assert!(buffer_contains(&buffer, "▸ playing"));
    assert!(!buffer_contains(&buffer, "stopped"));
}

#[test]
fn narrow_search_rows_keep_the_duration_and_mark_clipped_titles() {
    let _guard = crate::i18n::lock_for_test();
    crate::i18n::set_language(crate::i18n::Language::English);
    let mut app = app_with_search_results();
    app.search.results[1].duration = "12:34".to_owned();
    let buffer = render_app_buffer(&app, 40, 16);
    let row = row_text(&buffer, "Bad Guy").expect("second result row");
    assert!(row.contains("12:34"), "{row}");
    assert!(row.contains('…'), "{row}");
    // Keyboard selection still moves across the reformatted rows.
    app.update(Msg::Key(key(KeyCode::Down)));
    assert_eq!(app.search.selected, 1);
}

#[test]
fn queue_rows_show_each_track_duration() {
    let _guard = crate::i18n::lock_for_test();
    crate::i18n::set_language(crate::i18n::Language::English);
    let mut app = app_playing(4, 1);
    app.update(Msg::Key(key(KeyCode::Char('c'))));
    let buffer = render_app_buffer(&app, 60, 24);
    let row = row_text(&buffer, "t3 — a").expect("queue row");
    assert!(row.contains("0:10"), "{row}");
}

#[test]
fn retro_search_bar_keeps_readable_dropdown_and_filter_glyphs() {
    let _guard = crate::i18n::lock_for_test();
    crate::i18n::set_language(crate::i18n::Language::English);
    let mut app = app_with_search_results();
    app.config.retro_mode = true;
    let buffer = render_app_buffer(&app, 60, 24);
    assert!(buffer_contains(&buffer, "YTv"), "dropdown caret");
    assert!(buffer_contains(&buffer, "/ Filter"), "filter icon");
    assert!(!buffer_contains(&buffer, "? Filter"));
}

#[test]
fn a_clipped_cursor_row_marks_the_cut_then_crawls_to_its_end_keeping_the_duration() {
    let _guard = crate::i18n::lock_for_test();
    crate::i18n::set_language(crate::i18n::Language::English);
    let mut app = app_with_search_results();
    app.search.results[0].artist = "Billie Eilish and a very long featured artist list".to_owned();
    let first = render_app_buffer(&app, 40, 16);
    let row = row_text(&first, "Lovely").expect("cursor row");
    assert!(row.contains('…'), "held frame marks the cut: {row}");
    assert!(row.contains("0:10"), "{row}");
    // Step the animation clock through a whole crawl: every frame keeps the duration column,
    // and the end of the artist list comes into view at some point.
    let mut saw_end = false;
    for _ in 0..600 {
        app.anim.anim_frame += 1;
        let buffer = render_app_buffer(&app, 40, 16);
        let row = (0..16)
            .map(|y| buffer_row(&buffer, y))
            .find(|row| row.starts_with("│▶"))
            .expect("cursor row stays drawn");
        assert!(row.contains("0:10"), "{row}");
        saw_end |= row.contains("artist list");
    }
    assert!(saw_end, "the crawl never reached the end of the row");
}

fn compact(buffer: &ratatui::buffer::Buffer) -> String {
    (0..buffer.area.height)
        .flat_map(|y| buffer_row(buffer, y).chars().collect::<Vec<_>>())
        .filter(|c| !c.is_whitespace() && *c != '│')
        .collect()
}

fn squash(text: &str) -> String {
    text.chars().filter(|c| !c.is_whitespace()).collect()
}

#[test]
fn sleep_timer_note_wraps_whole_in_a_narrow_japanese_popup() {
    let _guard = crate::i18n::lock_for_test();
    crate::i18n::set_language(crate::i18n::Language::Japanese);
    let mut app = app_playing(2, 0);
    app.update(Msg::Key(key(KeyCode::Char('S'))));
    let text = compact(&render_app_buffer(&app, 40, 16));
    crate::i18n::set_language(crate::i18n::Language::English);
    assert!(
        text.contains(&squash(
            "一時停止の前に音量がフェードアウトします。「off」でキャンセル"
        )),
        "{text}"
    );
}

#[test]
fn local_deck_home_states_the_empty_index_once() {
    let _guard = crate::i18n::lock_for_test();
    crate::i18n::set_language(crate::i18n::Language::English);
    let mut app = App::new(100);
    app.local_dedicated_mode = true;
    app.mode = Mode::Library;
    let buffer = render_app_buffer(&app, 100, 30);
    assert!(buffer_contains(&buffer, "Not indexed yet"));
    assert!(buffer_contains(
        &buffer,
        "Press r to scan the download folder."
    ));
    assert!(!buffer_contains(&buffer, "No local downloads indexed yet"));
}

#[test]
fn atlas_panel_key_hint_is_never_cut() {
    let _guard = crate::i18n::lock_for_test();
    crate::i18n::set_language(crate::i18n::Language::English);
    let mut app = App::new(100);
    app.config.album_art = Some(true);
    let mut cmds = app.apply_radio_mode_confirm(RadioModeConfirm::Enter);
    admit_player_transition(&mut app, &mut cmds);
    app.update(Msg::Key(key(KeyCode::Char('a'))));
    assert!(app.radio_mode.atlas.open);
    let buffer = render_app_buffer(&app, 100, 30);
    assert!(buffer_contains(&buffer, "c country"), "hint split or cut");
    assert!(buffer_contains(&buffer, "Enter plays"));
}

#[test]
fn why_this_pick_meters_confidence() {
    let _guard = crate::i18n::lock_for_test();
    crate::i18n::set_language(crate::i18n::Language::English);
    let mut app = app_playing(1, 0);
    app.why_gem.upsert(
        "id0".to_owned(),
        crate::remote::proto::WhyGemModel {
            slot: "Balanced".to_owned(),
            reasons: Vec::new(),
            confidence: serde_json::Number::from_f64(0.6),
        },
    );
    app.open_why_gem_at(0);
    let buffer = render_app_buffer(&app, 80, 24);
    assert!(buffer_contains(&buffer, "■■■■■■□□□□ 60%"));
}

#[test]
fn empty_search_says_what_to_type_until_results_arrive() {
    let _guard = crate::i18n::lock_for_test();
    crate::i18n::set_language(crate::i18n::Language::English);
    let mut app = App::new(100);
    app.mode = Mode::Search;
    let hint = "Type a song, artist, or album, then press Enter.";
    assert!(buffer_contains(&render_app_buffer(&app, 80, 24), hint));
    let app = app_with_search_results();
    assert!(!buffer_contains(&render_app_buffer(&app, 80, 24), hint));
}

#[test]
fn a_failed_search_says_how_to_retry_until_the_next_search() {
    let _guard = crate::i18n::lock_for_test();
    crate::i18n::set_language(crate::i18n::Language::English);
    let mut app = App::new(100);
    app.mode = Mode::Search;
    let request_id = app.search.request_id;
    app.update(Msg::Search(SearchMsg::Error {
        request_id,
        source: SearchSource::Youtube,
        error: "network unavailable".to_owned(),
    }));
    let retry = "The search failed. Press Enter to try again";
    assert!(buffer_contains(&render_app_buffer(&app, 80, 24), retry));
    // Results from a later search clear it.
    app.update(Msg::Search(SearchMsg::Results {
        request_id,
        query: "x".to_owned(),
        source: SearchSource::Youtube,
        timed_out: false,
        songs: vec![fsong("a", "Lovely", "Billie Eilish")],
    }));
    assert!(!app.search.failed);
    assert!(!buffer_contains(&render_app_buffer(&app, 80, 24), retry));
}

#[test]
fn station_card_hint_wraps_whole_at_forty_columns() {
    let _guard = crate::i18n::lock_for_test();
    crate::i18n::set_language(crate::i18n::Language::Korean);
    let mut app = app_playing(2, 0);
    app.autoplay_streaming = true;
    app.ai.available = false;
    app.config.streaming.ai.enabled = false;
    app.update(Msg::Key(key(KeyCode::Char('e'))));
    assert!(app.overlays.station_card.is_some());
    let text = compact(&render_app_buffer(&app, 40, 16));
    crate::i18n::set_language(crate::i18n::Language::English);
    assert!(
        text.contains(&squash(
            "Enter: 재생 중인 아티스트와 비슷하게 · 앞에 - 는 제외"
        )),
        "{text}"
    );
}

#[test]
fn clipped_local_deck_rows_end_in_an_ellipsis() {
    let _guard = crate::i18n::lock_for_test();
    crate::i18n::set_language(crate::i18n::Language::English);
    let tracks: Vec<_> = (0..3)
        .map(|i| {
            let mut track = crate::local::LocalTrack::untagged(
                PathBuf::from(format!("/tmp/ytt-fixture/{i}.flac")),
                7,
                8,
            );
            track.title = format!("Fixture track {i} with a fairly long local title");
            track.artist = vec!["Fixture Artist".to_owned()];
            track
        })
        .collect();
    let mut app = super::local::app_with_local_deck_index(tracks);
    app.local_mode.ui.section = crate::app::LocalSection::Tracks;
    let buffer = render_app_buffer(&app, 60, 24);
    let row = row_text(&buffer, "Fixture track 2").expect("non-cursor row");
    assert!(row.contains('…'), "{row}");
}

#[test]
fn a_long_atlas_country_name_is_marked_and_keeps_the_play_hint() {
    let _guard = crate::i18n::lock_for_test();
    crate::i18n::set_language(crate::i18n::Language::English);
    let mut app = App::new(100);
    app.config.album_art = Some(true);
    let mut cmds = app.apply_radio_mode_confirm(RadioModeConfirm::Enter);
    admit_player_transition(&mut app, &mut cmds);
    app.update(Msg::Key(key(KeyCode::Char('a'))));
    app.radio_mode.atlas.active_country = Some(*b"GB");
    app.radio_mode.atlas.active_country_name =
        "United Kingdom of Great Britain and Northern Ireland".to_owned();
    let buffer = render_app_buffer(&app, 100, 30);
    assert!(buffer_contains(&buffer, "United Kingdom of Great Britain"));
    assert!(buffer_contains(&buffer, "…"));
    assert!(buffer_contains(&buffer, "Enter plays"));
}

/// The foreground of the first cell on `row` holding `symbol` at or after `from_x`.
fn cell_fg(
    buffer: &ratatui::buffer::Buffer,
    row: u16,
    symbol: &str,
    from_x: u16,
) -> Option<ratatui::style::Color> {
    (from_x..buffer.area.width)
        .filter_map(|x| buffer.cell((x, row)))
        .find(|cell| cell.symbol() == symbol)
        .map(|cell| cell.fg)
}

fn row_index(buffer: &ratatui::buffer::Buffer, needle: &str) -> u16 {
    (0..buffer.area.height)
        .find(|&y| buffer_row(buffer, y).contains(needle))
        .unwrap_or_else(|| panic!("{needle} not rendered"))
}

#[test]
fn highlighted_row_actions_use_the_selection_text_color() {
    let _guard = crate::i18n::lock_for_test();
    crate::i18n::set_language(crate::i18n::Language::English);
    for preset in [
        crate::theme::ThemePreset::Default,
        crate::theme::ThemePreset::Light,
    ] {
        let mut app = app_playing(4, 1);
        app.theme.set_preset(preset);
        app.update(Msg::Key(key(KeyCode::Char('c'))));
        let buffer = render_app_buffer(&app, 100, 30);
        let selected = app.theme.color(crate::theme::ThemeRole::SelectionFg);
        let y = row_index(&buffer, "t1 — a");
        assert_eq!(
            cell_fg(&buffer, y, "✗", 0),
            Some(selected),
            "{preset:?} queue"
        );

        let mut app = app_with_three_favorites();
        app.theme.set_preset(preset);
        let buffer = render_app_buffer(&app, 100, 30);
        let y = row_index(&buffer, "F0 — A");
        assert_eq!(
            cell_fg(&buffer, y, "✗", 0),
            Some(selected),
            "{preset:?} library"
        );
    }
}

#[test]
fn search_box_title_is_drawn_as_text_not_border() {
    let _guard = crate::i18n::lock_for_test();
    crate::i18n::set_language(crate::i18n::Language::English);
    let app = app_with_search_results();
    let buffer = render_app_buffer(&app, 100, 30);
    let y = row_index(&buffer, "Search · anonymous");
    let x = buffer_row(&buffer, y).find("Search · anonymous").unwrap();
    let x = buffer_row(&buffer, y)[..x].chars().count() as u16;
    assert_eq!(
        cell_fg(&buffer, y, "S", x),
        Some(app.theme.color(crate::theme::ThemeRole::TextMuted))
    );
}

/// sRGB triple for a rendered cell color; `Reset` stands for the theme's reference
/// background (Default is transparent over the terminal, measured against Mocha's base).
fn cell_rgb(color: ratatui::style::Color, reset: (u8, u8, u8)) -> (u8, u8, u8) {
    match color {
        ratatui::style::Color::Rgb(r, g, b) => (r, g, b),
        _ => reset,
    }
}

fn contrast_ratio(a: (u8, u8, u8), b: (u8, u8, u8)) -> f64 {
    let lum = |(r, g, b): (u8, u8, u8)| {
        let ch = |v: u8| {
            let c = f64::from(v) / 255.0;
            if c <= 0.039_28 {
                c / 12.92
            } else {
                ((c + 0.055) / 1.055).powf(2.4)
            }
        };
        0.2126 * ch(r) + 0.7152 * ch(g) + 0.0722 * ch(b)
    };
    let (la, lb) = (lum(a), lum(b));
    (la.max(lb) + 0.05) / (la.min(lb) + 0.05)
}

#[test]
fn default_and_light_text_meets_four_and_a_half_to_one_on_its_rendered_background() {
    let _guard = crate::i18n::lock_for_test();
    crate::i18n::set_language(crate::i18n::Language::English);
    for (preset, base) in [
        (crate::theme::ThemePreset::Default, (0x1E, 0x1E, 0x2E)),
        (crate::theme::ThemePreset::Light, (0xF7, 0xF7, 0xF2)),
    ] {
        let styled = |mut app: App| {
            app.theme.set_preset(preset);
            if let Some(settings) = app.settings.as_mut() {
                settings.draft.theme.set_preset(preset);
            }
            app
        };
        let mut queue = app_playing(8, 2);
        queue.update(Msg::Key(key(KeyCode::Char('c'))));
        let mut help = app_playing(3, 0);
        help.update(Msg::Key(key(KeyCode::Char('?'))));
        let mut sleep = app_playing(2, 0);
        sleep.update(Msg::Key(key(KeyCode::Char('S'))));
        let mut settings = App::new(100);
        focus_settings_field(&mut settings, SettingsTab::General, Field::DownloadDir);
        for (name, app) in [
            ("player", styled(app_playing(5, 1))),
            ("player idle", styled(App::new(100))),
            ("search", styled(app_with_search_results())),
            ("library", styled(app_with_three_favorites())),
            ("queue popup", styled(queue)),
            ("help", styled(help)),
            ("settings", styled(settings)),
            ("sleep popup", styled(sleep)),
        ] {
            let buffer = render_app_buffer(&app, 100, 30);
            let fg_reset = cell_rgb(app.theme.color(crate::theme::ThemeRole::TextPrimary), base);
            for y in 0..buffer.area.height {
                for x in 0..buffer.area.width {
                    let cell = buffer.cell((x, y)).unwrap();
                    let symbol = cell.symbol();
                    let Some(first) = symbol.chars().next() else {
                        continue;
                    };
                    // Text only: skip blanks, frame and gauge glyphs, and emoji (own colors).
                    if first.is_whitespace()
                        || ('\u{2500}'..='\u{259F}').contains(&first)
                        || u32::from(first) >= 0x1F000
                    {
                        continue;
                    }
                    let (mut fg, mut bg) = (cell_rgb(cell.fg, fg_reset), cell_rgb(cell.bg, base));
                    if cell.modifier.contains(ratatui::style::Modifier::REVERSED) {
                        std::mem::swap(&mut fg, &mut bg);
                    }
                    let ratio = contrast_ratio(fg, bg);
                    assert!(
                        ratio >= 4.5,
                        "{preset:?} {name}: {symbol:?} at ({x},{y}) is {ratio:.2}:1 in row {:?}",
                        buffer_row(&buffer, y).trim()
                    );
                }
            }
        }
    }
}

#[test]
fn listening_view_renders_selection_highlight_scrollbar_and_handles_mouse_wheel() {
    let _guard = crate::i18n::lock_for_test();
    crate::i18n::set_language(crate::i18n::Language::English);
    let mut app = App::new(100);
    let rows: Vec<crate::app::ListeningRow> = (0..18)
        .map(|i| {
            crate::app::ListeningRow::Bookmark(crate::listening::BookmarkRecord {
                bookmark_id: crate::listening::BookmarkId::new(format!("bm-{i}")).unwrap(),
                track: crate::personal_state::PortableTrack {
                    key: crate::personal_state::PortableTrackKey::Catalog {
                        provider: "youtube".to_owned(),
                        exact_catalog_id: format!("vid-{i}"),
                    },
                    title: format!("Track {i}"),
                    artist: "Artist".to_owned(),
                    album: None,
                    duration_secs: Some(180),
                    isrc: None,
                },
                position_ms: (i as u64) * 15_000,
                label: format!("Moment {i}"),
            })
        })
        .collect();
    app.overlays.listening = Some(crate::app::ListeningDialog {
        tab: crate::app::ListeningTab::Bookmarks,
        revision: 1,
        selected: 0,
        rows,
        ..Default::default()
    });

    let buffer = render_app_buffer(&app, 80, 20);
    let sel_bg = app.theme.color(crate::theme::ThemeRole::SelectionBg);
    let has_selected_row = (0..buffer.area.height).any(|y| {
        buffer_row(&buffer, y).contains("Moment 0")
            && (0..buffer.area.width).any(|x| buffer.cell((x, y)).unwrap().bg == sel_bg)
    });
    assert!(
        has_selected_row,
        "selected Listening row should use SelectionBg"
    );
    let has_scrollbar = (0..buffer.area.height).any(|y| buffer_row(&buffer, y).contains('█'));
    assert!(
        has_scrollbar,
        "overflowing Listening list should render a scrollbar thumb"
    );

    let before = app.overlays.listening.as_ref().unwrap().scroll.offset();
    app.update(Msg::MouseScroll {
        up: false,
        col: 40,
        row: 10,
        ctrl: false,
    });
    let after = app.overlays.listening.as_ref().unwrap().scroll.offset();
    assert!(
        after > before,
        "mouse wheel down should scroll the Listening list ({before} -> {after})"
    );
}

#[test]
fn seekbar_scrub_preview_shows_delta_chapter_and_sponsorblock_spans() {
    let _guard = crate::i18n::lock_for_test();
    crate::i18n::set_language(crate::i18n::Language::English);
    let mut app = app_playing(2, 0);
    app.playback.duration = Some(300.0);
    app.playback.time_pos = Some(40.0);
    app.playback.time_pos_at = Some(Instant::now());
    app.playback.chapters = vec![
        crate::player::Chapter {
            title: "Intro".into(),
            start_secs: 0.0,
        },
        crate::player::Chapter {
            title: "Chorus".into(),
            start_secs: 90.0,
        },
    ];
    // Include a segment across the center of the seekbar to verify the centered label's
    // internal spaces are excluded from SponsorBlock overlays.
    app.install_sponsorblock_parity_state(&[(20.0, 90.0), (110.0, 190.0), (210.0, 280.0)]);

    let buffer = render_app_buffer(&app, 100, 26);
    let sb_rect = app.hits.seekbar_rect().expect("seekbar rect registered");
    let warning = app.theme.color(crate::theme::ThemeRole::Warning);
    let has_sb_marker = (sb_rect.left()..sb_rect.right()).any(|x| {
        let cell = buffer.cell((x, sb_rect.y)).unwrap();
        cell.symbol() == "▒" && cell.fg == warning
    });
    assert!(
        has_sb_marker,
        "SponsorBlock segments should render warning-tinted markers on the seekbar"
    );
    let idle_row = buffer_row(&buffer, sb_rect.y);
    assert!(
        idle_row.contains("0:40 / 5:00"),
        "SponsorBlock overlay must not paint over spaces inside the seekbar label, got: {idle_row:?}"
    );

    // Click around 55% of the seekbar (~165s, inside "Chorus", delta ~+2:05).
    let click_col = sb_rect.x + sb_rect.width * 55 / 100;
    app.update(Msg::MouseClick {
        col: click_col,
        row: sb_rect.y,
        multi: false,
    });
    assert!(app.seekbar_preview_target().is_some());
    let scrub_buf = render_app_buffer(&app, 100, 26);
    let row = buffer_row(&scrub_buf, sb_rect.y);
    assert!(
        row.contains(" / 5:00 (+") && row.contains(" · Chorus"),
        "scrub preview should keep intact spaces with signed delta and destination chapter, got: {row:?}"
    );
}

#[test]
fn cjk_chapter_tag_truncates_by_display_width_not_char_count() {
    let _guard = crate::i18n::lock_for_test();
    crate::i18n::set_language(crate::i18n::Language::Korean);
    let mut app = app_playing(2, 0);
    app.playback.duration = Some(300.0);
    app.playback.time_pos = Some(10.0);
    app.playback.time_pos_at = Some(Instant::now());
    // 20 Hangul syllables = 20 chars (< 24) but 40 terminal cells (> 24).
    let long_ko = "가나다라마바사아자차카타파하가나다라마바";
    app.playback.chapters = vec![crate::player::Chapter {
        title: long_ko.into(),
        start_secs: 0.0,
    }];
    let buffer = render_app_buffer(&app, 140, 26);
    let compact_rows: Vec<String> = (0..buffer.area.height)
        .map(|y| {
            buffer_row(&buffer, y)
                .chars()
                .filter(|c| !c.is_whitespace())
                .collect()
        })
        .collect();
    let status_row = compact_rows
        .iter()
        .find(|row| row.contains("1/2") && row.contains("EQ:"))
        .unwrap_or_else(|| panic!("expected status row in buffer: {compact_rows:?}"));
    assert!(
        !status_row.contains(long_ko),
        "20-syllable Korean chapter (40 cells) must be truncated by display width, got: {status_row:?}"
    );
    assert!(
        status_row.contains("·가나다라마바사아자차카…"),
        "expected width-capped Korean chapter tag with ellipsis, got: {status_row:?}"
    );
    crate::i18n::set_language(crate::i18n::Language::English);
}

#[test]
fn modal_scrim_dims_backdrop_outside_modal_and_skips_retro_mode() {
    let _guard = crate::i18n::lock_for_test();
    crate::i18n::set_language(crate::i18n::Language::English);
    let mut app = app_playing(3, 0);
    app.update(Msg::Key(key(KeyCode::Char('?'))));

    let buf = render_app_buffer(&app, 80, 24);
    assert!(
        buf.cell((0, 0))
            .unwrap()
            .modifier
            .contains(ratatui::style::Modifier::DIM),
        "backdrop outside centered modal should be dimmed"
    );
    assert!(
        !buf.cell((40, 12))
            .unwrap()
            .modifier
            .contains(ratatui::style::Modifier::DIM),
        "cells inside the modal must stay undimmed"
    );

    app.config.retro_mode = true;
    let retro_buf = render_app_buffer(&app, 80, 24);
    assert!(
        !retro_buf
            .cell((0, 0))
            .unwrap()
            .modifier
            .contains(ratatui::style::Modifier::DIM),
        "retro mode should skip modal backdrop dimming"
    );
}
