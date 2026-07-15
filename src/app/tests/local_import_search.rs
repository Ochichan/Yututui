use super::*;

fn app_on_manual_search_import_row(session_id: &str) -> App {
    super::local_import::save_ambiguous_import_job(session_id);

    let mut app = super::local_import::app_with_local_deck_index(Vec::new());
    app.update(Msg::Key(key(KeyCode::Char('9'))));
    app.local_mode.ui.filter_query = session_id.to_owned();
    let open = double_click_target(&mut app, MouseTarget::LocalRow(0));
    assert!(open.is_empty());
    app.local_mode.ui.filter_query.clear();
    app
}

fn replace_manual_search_metadata(session_id: &str, title: String, artists: Vec<String>) {
    let mut session = crate::transfer::session::ImportSession::load(session_id)
        .expect("load manual-search import session");
    let row = session.rows.first_mut().expect("manual-search row");
    row.title = title;
    row.artists = artists;
    session.save().expect("save manual-search import session");
}

fn search_cmd_count(cmds: &[Cmd]) -> usize {
    cmds.iter()
        .filter(|cmd| matches!(cmd, Cmd::Search { .. }))
        .count()
}

#[test]
fn local_deck_import_row_s_searches_once_only_after_accepted_exit() {
    let mut app = app_on_manual_search_import_row("sp2yt-local-manual-search");
    app.search.input = "stale online query".to_owned();
    app.search.input_cursor = TextCursor::from_byte_index(2);
    let hint = app.local_import_action_hint().expect("review action hint");
    for expected in [
        "A mark all ready",
        "a accept",
        "r reject",
        "c candidate",
        "x skip",
        "o open candidate",
        "s search",
    ] {
        assert!(hint.contains(expected), "missing {expected:?} in {hint:?}");
    }

    let cmds = app.update(Msg::Key(key(KeyCode::Char('s'))));
    assert!(cmds.is_empty());
    assert!(app.local_dedicated_mode);
    assert_eq!(app.mode, Mode::Library);
    assert_eq!(app.local_mode.pending_confirm, Some(LocalModeConfirm::Exit));
    assert!(app.local_import_search_pending());

    let mut exit = app.update(Msg::Key(key(KeyCode::Enter)));
    assert!(app.local_dedicated_mode);
    assert_eq!(app.mode, Mode::Library);
    assert_eq!(search_cmd_count(&exit), 0);
    admit_player_transition(&mut app, &mut exit);

    assert!(!app.local_dedicated_mode);
    assert_eq!(app.mode, Mode::Search);
    assert_eq!(app.search.focus, SearchFocus::Input);
    assert_eq!(app.search.kind, SearchKind::Songs);
    assert_eq!(app.search.input, "Maybe Artist");
    assert_eq!(
        app.search.input_cursor.byte_index(&app.search.input),
        app.search.input.len(),
        "the confirmed handoff must place the online search cursor after its replacement query"
    );
    assert_eq!(search_cmd_count(&exit), 1);
    let Some(Cmd::Search {
        query,
        source,
        config,
        ..
    }) = exit.iter().find(|cmd| matches!(cmd, Cmd::Search { .. }))
    else {
        panic!("expected manual search command");
    };
    assert_eq!(query, "Maybe Artist");
    assert_eq!(*source, crate::search_source::SearchSource::Youtube);
    assert_eq!(config.source, crate::search_source::SearchSource::Youtube);
    assert!(!app.local_import_search_pending());
    assert!(
        app.complete_local_import_search_continuation(None)
            .is_empty()
    );
}

#[test]
fn local_import_manual_search_double_enter_keeps_first_intent_single_use() {
    let mut app = app_on_manual_search_import_row("sp2yt-local-manual-search-double-enter");
    assert!(app.update(Msg::Key(key(KeyCode::Char('s')))).is_empty());

    let mut first = app.update(Msg::Key(key(KeyCode::Enter)));
    let duplicate = app.update(Msg::Key(key(KeyCode::Enter)));
    assert!(
        duplicate.is_empty(),
        "the claimed confirmation must not emit a second player intent"
    );
    assert!(app.local_import_search_pending());

    admit_player_transition(&mut app, &mut first);
    assert!(!app.local_dedicated_mode);
    assert_eq!(app.mode, Mode::Search);
    assert_eq!(search_cmd_count(&first), 1);
    assert!(!app.local_import_search_pending());
}

#[test]
fn stale_cancelled_exit_cannot_consume_or_clear_a_new_search_confirmation() {
    let mut app = app_on_manual_search_import_row("sp2yt-local-manual-search-aba");
    assert!(app.update(Msg::Key(key(KeyCode::Char('s')))).is_empty());
    let stale_exit = app.update(Msg::Key(key(KeyCode::Enter)));

    assert!(app.update(Msg::Key(key(KeyCode::Esc))).is_empty());
    assert!(app.update(Msg::Key(key(KeyCode::Char('s')))).is_empty());
    assert!(app.local_import_search_pending());
    assert_eq!(app.local_mode.pending_confirm, Some(LocalModeConfirm::Exit));

    assert_rejected_before_send(&mut app, stale_exit);
    assert!(app.local_dedicated_mode);
    assert!(app.local_import_search_pending());
    assert_eq!(app.local_mode.pending_confirm, Some(LocalModeConfirm::Exit));

    let mut current_exit = app.update(Msg::Key(key(KeyCode::Enter)));
    admit_player_transition(&mut app, &mut current_exit);
    assert!(!app.local_dedicated_mode);
    assert_eq!(search_cmd_count(&current_exit), 1);
}

#[test]
fn local_import_manual_search_rejects_invalid_query_before_exit_confirmation() {
    for (session_id, title) in [
        (
            "sp2yt-local-manual-search-control",
            "unsafe\nquery".to_owned(),
        ),
        (
            "sp2yt-local-manual-search-long",
            "x".repeat(crate::util::query::MAX_SEARCH_QUERY_BYTES + 1),
        ),
    ] {
        let mut app = app_on_manual_search_import_row(session_id);
        replace_manual_search_metadata(session_id, title, Vec::new());

        assert!(app.update(Msg::Key(key(KeyCode::Char('s')))).is_empty());
        assert!(app.local_dedicated_mode);
        assert_eq!(app.mode, Mode::Library);
        assert!(app.local_mode.pending_confirm.is_none());
        assert!(!app.local_import_search_pending());
        assert_eq!(app.status.kind, StatusKind::Error);
    }
}

#[test]
fn local_import_manual_search_requires_youtube_before_exit_confirmation() {
    let mut app = app_on_manual_search_import_row("sp2yt-local-manual-search-youtube-disabled");
    app.config.search.youtube = false;
    app.config.search.soundcloud = true;

    assert!(app.update(Msg::Key(key(KeyCode::Char('s')))).is_empty());
    assert!(app.local_dedicated_mode);
    assert_eq!(app.mode, Mode::Library);
    assert!(app.local_mode.pending_confirm.is_none());
    assert!(!app.local_import_search_pending());
    assert_eq!(app.status.kind, StatusKind::Error);
    assert!(app.status.text.contains("YouTube"));
}

#[test]
fn local_import_manual_search_cancel_and_stale_origin_fail_closed() {
    let mut cancelled = app_on_manual_search_import_row("sp2yt-local-manual-search-cancel");
    assert!(
        cancelled
            .update(Msg::Key(key(KeyCode::Char('s'))))
            .is_empty()
    );
    assert!(cancelled.local_import_search_pending());

    assert!(cancelled.update(Msg::Key(key(KeyCode::Esc))).is_empty());
    assert!(cancelled.local_dedicated_mode);
    assert_eq!(cancelled.mode, Mode::Library);
    assert!(cancelled.local_mode.pending_confirm.is_none());
    assert!(!cancelled.local_import_search_pending());

    let mut stale = app_on_manual_search_import_row("sp2yt-local-manual-search-stale");
    assert!(stale.update(Msg::Key(key(KeyCode::Char('s')))).is_empty());
    stale
        .local_mode
        .rows_revision
        .set(stale.local_mode.rows_revision.get().wrapping_add(1));
    let mut exit = stale.update(Msg::Key(key(KeyCode::Enter)));
    admit_player_transition(&mut stale, &mut exit);

    assert!(!stale.local_dedicated_mode);
    assert_eq!(search_cmd_count(&exit), 0);
    assert!(!stale.local_import_search_pending());
    assert_ne!(stale.mode, Mode::Search);
}

#[test]
fn local_import_manual_search_admission_rejection_clears_continuation() {
    use crate::util::delivery::DeliveryError;

    for (session_id, error) in [
        ("sp2yt-local-manual-search-busy", DeliveryError::Busy),
        ("sp2yt-local-manual-search-closed", DeliveryError::Closed),
    ] {
        let mut app = app_on_manual_search_import_row(session_id);
        assert!(app.update(Msg::Key(key(KeyCode::Char('s')))).is_empty());
        let exit = app.update(Msg::Key(key(KeyCode::Enter)));
        assert_eq!(search_cmd_count(&exit), 0);

        let rejected = reject_player_transition(&mut app, exit, error);
        assert_eq!(search_cmd_count(&rejected), 0);
        assert!(app.local_dedicated_mode);
        assert_eq!(app.mode, Mode::Library);
        assert_eq!(app.local_mode.pending_confirm, Some(LocalModeConfirm::Exit));
        assert!(!app.local_import_search_pending());

        let mut retry = app.update(Msg::Key(key(KeyCode::Enter)));
        admit_player_transition(&mut app, &mut retry);
        assert!(!app.local_dedicated_mode);
        assert_eq!(search_cmd_count(&retry), 0);
        assert_ne!(app.mode, Mode::Search);
    }
}
