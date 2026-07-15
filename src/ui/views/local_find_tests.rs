use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use ratatui::Terminal;
use ratatui::backend::TestBackend;

use super::*;
use crate::app::{LocalFindDrill, Mode, Msg};
use crate::config::PlayerBarPosition;
use crate::local::find::{
    LocalFindCommand, LocalFindCorpusRevision, LocalFindGroup, LocalFindQuery, LocalFindSnapshot,
};
use crate::util::text_edit::TextCursor;

fn result_app() -> App {
    let mut app = App::new(100);
    app.local_dedicated_mode = true;
    app.mode = Mode::Search;
    app.config.player_bar_position = Some(PlayerBarPosition::Top);
    app.local_mode.find.query = "blue moon".to_owned();
    app.local_mode.find.focus = LocalFindFocus::Results;
    app.local_mode.find.request_id = 7;
    let track = crate::local::LocalTrack::untagged("/music/blue-moon.flac".into(), 0, 0);
    app.local_mode.find.snapshot = Some(LocalFindSnapshot {
        generation: 7,
        corpus_revision: LocalFindCorpusRevision::default(),
        query: LocalFindQuery::parse("blue moon").unwrap(),
        scope: LocalFindScope::All,
        sort: LocalFindSort::Relevance,
        groups: vec![LocalFindGroup {
            scope: LocalFindScope::Tracks,
            hits: vec![LocalFindHit {
                id: LocalFindHitId::Track(track.id),
                label: "Blue Moon".to_owned(),
                secondary: "Example Artist".to_owned(),
                year: Some(2026),
                locally_playable_count: 1,
                total_track_count: 1,
                match_reason: None,
            }],
        }],
        total_hits: 1,
    });
    app
}

fn empty_snapshot(query: &str) -> LocalFindSnapshot {
    LocalFindSnapshot {
        generation: 7,
        corpus_revision: LocalFindCorpusRevision::default(),
        query: LocalFindQuery::parse(query).unwrap(),
        scope: LocalFindScope::All,
        sort: LocalFindSort::Relevance,
        groups: Vec::new(),
        total_hits: 0,
    }
}

fn draw(app: &App, width: u16, height: u16) -> String {
    let backend = TestBackend::new(width, height);
    let mut terminal = Terminal::new(backend).unwrap();
    terminal
        .draw(|frame| render(frame, app, frame.area()))
        .unwrap();
    terminal
        .backend()
        .buffer()
        .content()
        .iter()
        .map(|cell| cell.symbol())
        .collect()
}

#[test]
fn responsive_width_boundaries_are_stable() {
    assert_eq!(width_tier(110), FindWidthTier::Wide);
    assert_eq!(width_tier(109), FindWidthTier::Medium);
    assert_eq!(width_tier(80), FindWidthTier::Medium);
    assert_eq!(width_tier(79), FindWidthTier::Sidebar);
    assert_eq!(width_tier(72), FindWidthTier::Sidebar);
    assert_eq!(width_tier(71), FindWidthTier::Compact);
    assert_eq!(width_tier(48), FindWidthTier::Compact);
    assert_eq!(width_tier(47), FindWidthTier::Narrow);
    assert_eq!(width_tier(32), FindWidthTier::Narrow);
}

#[test]
fn renders_results_at_each_supported_width() {
    let app = result_app();
    for width in [110, 80, 72, 48, 32] {
        let text = draw(&app, width, 24);
        assert!(
            text.contains("Blue Moon"),
            "missing result at width {width}"
        );
        assert!(text.contains("All▾"), "missing scope chip at width {width}");
        app.clear_mouse_regions();
    }
}

#[test]
fn minimum_height_keeps_results_and_scope_chip_at_each_supported_width() {
    let app = result_app();
    for width in [110, 80, 72, 48, 32] {
        let text = draw(&app, width, 14);
        assert!(
            text.contains("Blue Moon"),
            "missing dense result at width {width}"
        );
        assert!(
            text.contains("All▾"),
            "missing dense scope chip at width {width}"
        );
        app.clear_mouse_regions();
    }
}

#[test]
fn narrow_input_keeps_the_query_tail_and_caret_visible() {
    let mut app = result_app();
    app.local_mode.find.focus = LocalFindFocus::Input;
    app.local_mode.find.query = "a very long structured query with VISIBLETAIL".to_owned();
    app.local_mode.find.input_cursor = TextCursor::at_end(&app.local_mode.find.query);
    let text = draw(&app, 32, 14);
    assert!(text.contains("VISIBLETAIL"), "missing query tail: {text:?}");
    assert!(text.contains('█'), "missing input caret: {text:?}");
}

#[test]
fn blank_collection_renders_recovery_actions_at_minimum_full_size() {
    let mut app = App::new(100);
    app.local_dedicated_mode = true;
    app.mode = Mode::Search;
    let text = draw(&app, 32, 14);
    assert!(
        text.contains("Rescan") || text.contains("다시 스캔"),
        "{text:?}"
    );
}

#[test]
fn queue_capacity_confirmation_names_accepted_and_omitted_counts() {
    let mut app = result_app();
    let first = crate::local::LocalTrack::untagged("/music/one.flac".into(), 0, 0).id;
    let second = crate::local::LocalTrack::untagged("/music/two.flac".into(), 0, 0).id;
    app.local_mode.find.pending_bulk_confirm = Some(LocalFindBulkConfirm {
        action: LocalFindBulkAction::Enqueue,
        track_ids: vec![first, second],
        accepted_count: 1,
        result_generation: 7,
        corpus_revision: LocalFindCorpusRevision::default(),
        queue_revision: app.queue.rev(),
        capacity_recalculated: false,
    });
    let text = draw(&app, 80, 24);
    assert!(
        text.contains("omit 1") || text.contains("1곡은 제외"),
        "{text:?}"
    );
}

#[test]
fn remote_only_playlist_row_explains_that_zero_entries_are_local() {
    let hit = LocalFindHit {
        id: LocalFindHitId::Playlist("remote".to_owned()),
        label: "Remote only".to_owned(),
        secondary: String::new(),
        year: None,
        locally_playable_count: 0,
        total_track_count: 3,
        match_reason: Some(LocalFindMatchReason::PlaylistName),
    };
    let row = format_hit_row(&hit, LocalFindScope::Playlists, 0, 1, FindWidthTier::Wide);
    assert!(row.contains("0/3 local") || row.contains("0/3 로컬"));
}

#[test]
fn result_rows_use_textual_types_and_group_counts() {
    let app = result_app();
    let hit = app
        .local_mode
        .find
        .snapshot
        .as_ref()
        .unwrap()
        .hit_at(0)
        .unwrap();
    let wide = format_hit_row(hit, LocalFindScope::Tracks, 0, 42, FindWidthTier::Wide);
    let compact = format_hit_row(hit, LocalFindScope::Tracks, 1, 42, FindWidthTier::Compact);
    assert!(wide.starts_with("[Tracks · 42] [Track]"), "{wide}");
    assert!(compact.starts_with("[Track 2/42]"), "{compact}");
}

#[test]
fn playlist_safety_prefix_survives_fully_local_and_long_labels() {
    let hit = LocalFindHit {
        id: LocalFindHitId::Playlist("local".to_owned()),
        label: "A playlist label that is deliberately much longer than a narrow terminal"
            .to_owned(),
        secondary: "offline projection".to_owned(),
        year: None,
        locally_playable_count: 3,
        total_track_count: 3,
        match_reason: Some(LocalFindMatchReason::ResolvedLocalTrack),
    };
    let row = format_hit_row(&hit, LocalFindScope::Playlists, 0, 9, FindWidthTier::Narrow);
    assert!(row.starts_with("[Playlist] 3/3 local"), "{row}");
    assert!(row.contains("local track match"), "{row}");
    assert!(
        row.contains(&hit.label),
        "formatting pre-truncated the label: {row}"
    );
}

#[test]
fn loading_scanning_failure_and_unknown_command_have_distinct_messages() {
    let mut loading = result_app();
    loading.local_mode.find.snapshot = None;
    loading.local_mode.index.loading = true;
    let loading_text = draw(&loading, 80, 24);
    assert!(
        loading_text.contains("Loading the Local Deck index"),
        "{loading_text:?}"
    );

    let mut scanning = result_app();
    scanning.local_mode.find.snapshot = None;
    scanning.local_mode.index.scanning = true;
    let scanning_text = draw(&scanning, 80, 24);
    assert!(
        scanning_text.contains("Scanning local audio"),
        "{scanning_text:?}"
    );

    let mut failed = result_app();
    failed.local_mode.find.snapshot = None;
    failed.status.text = "Index load failed safely".to_owned();
    failed.status.kind = StatusKind::Error;
    let failed_text = draw(&failed, 80, 24);
    assert!(
        failed_text.contains("Index load failed safely"),
        "{failed_text:?}"
    );

    let mut unknown = result_app();
    unknown.local_mode.find.query = "> nope".to_owned();
    unknown.local_mode.find.snapshot = Some(empty_snapshot("> nope"));
    let unknown_text = draw(&unknown, 80, 24);
    assert!(
        unknown_text.contains("Unknown Local command"),
        "{unknown_text:?}"
    );
    assert!(unknown_text.contains("> tracks"), "{unknown_text:?}");
}

#[test]
fn global_status_takes_precedence_over_local_activity() {
    let mut app = result_app();
    app.status.text = "Queue is full".to_owned();
    app.status.kind = StatusKind::Error;
    let text = draw(&app, 80, 24);
    assert!(text.contains("Queue is full"), "{text:?}");
    assert!(!text.contains("offline  -  1 result"), "{text:?}");
}

#[test]
fn korean_render_and_row_metadata_are_localized() {
    let _guard = crate::i18n::lock_for_test();
    crate::i18n::set_language(crate::i18n::Language::Korean);
    let mut app = result_app();
    app.local_mode.find.snapshot = None;
    app.local_mode.index.loading = true;
    let text = draw(&app, 80, 24).replace(' ', "");
    let hit = LocalFindHit {
        id: LocalFindHitId::Playlist("ko".to_owned()),
        label: "밤의 목록".to_owned(),
        secondary: String::new(),
        year: None,
        locally_playable_count: 2,
        total_track_count: 3,
        match_reason: Some(LocalFindMatchReason::PlaylistName),
    };
    let row = format_hit_row(&hit, LocalFindScope::Playlists, 0, 1, FindWidthTier::Wide);
    assert!(text.contains("로컬덱인덱스를불러오는중"), "{text:?}");
    assert!(row.starts_with("[플레이리스트] 2/3 로컬"), "{row}");
    assert!(row.contains("이름 일치"), "{row}");
}

#[test]
fn dense_drill_keeps_its_breadcrumb() {
    let mut app = result_app();
    app.local_mode.find.drill = Some(LocalFindDrill {
        title: "Dense Crate".to_owned(),
        source: LocalFindHitId::Command(LocalFindCommand::Tracks),
        track_ids: Vec::new(),
        corpus_revision: LocalFindCorpusRevision::default(),
    });
    let text = draw(&app, 32, 14);
    assert!(text.contains("Dense Crate"), "{text:?}");
}

#[test]
fn narrow_refine_help_records_a_scrollable_wrapped_viewport() {
    let mut app = result_app();
    app.local_mode.find.refine_popup.open = true;
    let first = draw(&app, 32, 14);
    let viewport = app.local_mode.find.refine_popup.help_scroll.viewport();
    assert!(viewport > 0, "refine viewport was not recorded");
    assert!(refine_help_lines(26).len() > viewport);
    assert!(first.contains("Enter apply"), "{first:?}");
    assert!(first.contains("Esc cancel"), "{first:?}");
    app.update(Msg::Key(KeyEvent::new(
        KeyCode::PageDown,
        KeyModifiers::empty(),
    )));
    let after_page = app.local_mode.find.refine_popup.help_scroll.offset();
    assert!(after_page > 0, "PageDown did not scroll Refine help");
    app.update(Msg::Key(KeyEvent::new(
        KeyCode::PageUp,
        KeyModifiers::empty(),
    )));
    assert_eq!(app.local_mode.find.refine_popup.help_scroll.offset(), 0);
    app.update(Msg::MouseScroll {
        up: false,
        col: 2,
        row: 2,
        ctrl: false,
    });
    let second = draw(&app, 32, 14);
    assert!(app.local_mode.find.refine_popup.help_scroll.offset() > 0);
    assert_ne!(
        first, second,
        "scrolling did not change the wrapped help view"
    );
}

#[test]
fn rebuild_confirmation_explains_availability_and_registers_both_actions() {
    let mut app = result_app();
    app.local_mode.find.pending_rebuild_confirm = true;
    let text = draw(&app, 80, 24);
    assert!(text.contains("Full Local index rebuild"), "{text:?}");
    assert!(text.contains("searchable"), "{text:?}");
    assert!(
        app.hits
            .rect_of_target(MouseTarget::ConfirmLocalFindRebuild)
            .is_some()
    );
    assert!(
        app.hits
            .rect_of_target(MouseTarget::CancelLocalFindRebuild)
            .is_some()
    );
}
