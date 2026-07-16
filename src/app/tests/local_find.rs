use std::sync::Arc;

use super::*;
use crate::local::find::{
    LocalFindCorpus, LocalFindCorpusOptions, LocalFindCorpusRevision, LocalFindHitId,
    LocalFindQuery, LocalFindScope, LocalFindSnapshot, LocalFindSort,
};

fn fixture_tracks() -> Vec<crate::local::LocalTrack> {
    vec![
        super::local::local_deck_track(
            "/music/Palette Jam.flac",
            "Palette Jam",
            &["IU"],
            Some("Aurora"),
            Some("IU"),
            &["Pop"],
            30,
        ),
        super::local::local_deck_track(
            "/music/Second Light.flac",
            "Second Light",
            &["IU"],
            Some("Aurora"),
            Some("IU"),
            &["Pop"],
            20,
        ),
        super::local::local_deck_track(
            "/music/Blue Night.ogg",
            "Blue Night",
            &["AKMU"],
            Some("Sailing"),
            Some("AKMU"),
            &["Folk"],
            10,
        ),
    ]
}

fn assert_no_remote_search(commands: &[Cmd]) {
    assert!(
        commands
            .iter()
            .all(|command| !matches!(command, Cmd::Search { .. } | Cmd::SearchPlaylists { .. })),
        "Local Find must never emit an online search command"
    );
}

fn corpus_for(app: &App) -> Arc<LocalFindCorpus> {
    Arc::new(LocalFindCorpus::build(
        app.local_mode.index.index.tracks(),
        &[],
        LocalFindCorpusRevision {
            index: app.local_mode.index.revision,
            playlists: app.playlists.revision(),
            downloads: 0,
            options: crate::local::model::stable_hash_segments(&[app
                .config
                .effective_download_dir()
                .to_string_lossy()
                .as_bytes()]),
        },
        &LocalFindCorpusOptions::default(),
    ))
}

fn local_app_before_find() -> App {
    super::local::app_with_local_deck_index(fixture_tracks())
}

fn open_and_install_corpus(app: &mut App) -> Arc<LocalFindCorpus> {
    let commands = app.update(Msg::Key(ctrl(KeyCode::Char('f'))));
    assert_no_remote_search(&commands);
    assert_eq!(app.mode, Mode::Search);
    assert_eq!(app.active_search_surface(), ActiveSearchSurface::Local);
    assert!(matches!(
        commands.as_slice(),
        [Cmd::Local(LocalCmd::BuildFindCorpus { .. })]
    ));

    let generation = app.local_mode.find.corpus_generation;
    let corpus = corpus_for(app);
    let follow_up = app.update(Msg::Local(LocalMsg::FindCorpusReady {
        generation,
        corpus: Arc::clone(&corpus),
    }));
    assert_no_remote_search(&follow_up);
    assert!(follow_up.is_empty(), "blank Find opens on its launchpad");
    corpus
}

fn evaluate_command(
    commands: &[Cmd],
) -> (
    u64,
    u64,
    Arc<LocalFindCorpus>,
    LocalFindQuery,
    LocalFindScope,
    LocalFindSort,
) {
    assert_no_remote_search(commands);
    commands
        .iter()
        .find_map(|command| match command {
            Cmd::Local(LocalCmd::EvaluateFind {
                request_id,
                generation,
                corpus,
                query,
                scope,
                sort,
            }) => Some((
                *request_id,
                *generation,
                Arc::clone(corpus),
                query.clone(),
                *scope,
                *sort,
            )),
            _ => None,
        })
        .expect("Local Find evaluation command")
}

fn submit_and_apply(app: &mut App, query: &str) -> LocalFindSnapshot {
    app.local_mode.find.query = query.to_owned();
    let commands = app.submit_local_find_query();
    let (request_id, generation, corpus, query, scope, sort) = evaluate_command(&commands);
    let snapshot = corpus.search(&query, scope, sort, request_id);
    let follow_up = app.update(Msg::Local(LocalMsg::FindResultsReady {
        request_id,
        generation,
        snapshot: snapshot.clone(),
    }));
    assert_no_remote_search(&follow_up);
    assert!(follow_up.is_empty());
    snapshot
}

fn apply_evaluation(app: &mut App, commands: &[Cmd]) -> LocalFindSnapshot {
    let (request_id, generation, corpus, query, scope, sort) = evaluate_command(commands);
    let snapshot = corpus.search(&query, scope, sort, request_id);
    app.update(Msg::Local(LocalMsg::FindResultsReady {
        request_id,
        generation,
        snapshot: snapshot.clone(),
    }));
    snapshot
}

#[test]
fn normal_and_local_search_navigation_keep_independent_state() {
    let mut app = local_app_before_find();
    app.search.input = "online sentinel".to_owned();
    app.search.results = vec![Song::remote("remote", "Remote", "Artist", "3:00")];
    app.search.selected = 0;

    open_and_install_corpus(&mut app);
    submit_and_apply(&mut app, "Palette");
    assert_eq!(app.local_mode.find.query, "Palette");
    assert_eq!(app.search.input, "online sentinel");
    assert_eq!(app.search.results[0].video_id, "remote");

    super::local::admit_local_mode_confirm(&mut app, LocalModeConfirm::Exit);
    assert_eq!(app.mode, Mode::Search);
    assert_eq!(app.active_search_surface(), ActiveSearchSurface::Normal);
    assert_eq!(app.search.input, "online sentinel");
    assert_eq!(app.search.results[0].video_id, "remote");

    super::local::admit_local_mode_confirm(&mut app, LocalModeConfirm::Enter);
    assert_eq!(app.mode, Mode::Library);
    let commands = app.update(Msg::Key(ctrl(KeyCode::Char('f'))));
    assert_no_remote_search(&commands);
    assert!(
        commands.is_empty(),
        "unchanged corpus must be reused on re-entry"
    );
    assert!(app.local_mode.find.query.is_empty());
    assert!(app.local_mode.find.snapshot.is_none());
    assert!(app.local_mode.find.corpus.is_some());
}

#[test]
fn local_find_open_action_is_remappable_and_ctrl_f_is_only_the_default() {
    let mut app = local_app_before_find();
    let replacement = Chord::new(KeyCode::F(12), KeyModifiers::empty());
    app.keymap
        .rebind(KeyContext::LocalDeck, Action::OpenLocalFind, replacement)
        .expect("free Local Deck test chord");

    let old = app.update(Msg::Key(ctrl(KeyCode::Char('f'))));
    assert_no_remote_search(&old);
    assert!(old.is_empty());
    assert_eq!(app.mode, Mode::Library);

    let replacement_commands = app.update(Msg::Key(key(KeyCode::F(12))));
    assert_no_remote_search(&replacement_commands);
    assert_eq!(app.mode, Mode::Search);
    assert!(matches!(
        replacement_commands.as_slice(),
        [Cmd::Local(LocalCmd::BuildFindCorpus { .. })]
    ));
}

#[test]
fn local_find_supports_grapheme_safe_middle_cursor_edits() {
    let mut app = local_app_before_find();
    open_and_install_corpus(&mut app);

    for ch in "ab🙂c".chars() {
        assert_no_remote_search(&app.update(Msg::Key(key(KeyCode::Char(ch)))));
    }
    assert_eq!(app.local_mode.find.query, "ab🙂c");

    assert_no_remote_search(&app.update(Msg::Key(key(KeyCode::Left))));
    assert_no_remote_search(&app.update(Msg::Key(key(KeyCode::Left))));
    assert_eq!(
        app.local_mode
            .find
            .input_cursor
            .byte_index(&app.local_mode.find.query),
        2
    );

    assert_no_remote_search(&app.update(Msg::Key(key(KeyCode::Char('X')))));
    assert_eq!(app.local_mode.find.query, "abX🙂c");
    assert_no_remote_search(&app.update(Msg::Key(key(KeyCode::Backspace))));
    assert_eq!(app.local_mode.find.query, "ab🙂c");

    app.keymap
        .rebind(
            KeyContext::Common,
            Action::DeleteChar,
            Chord::new(KeyCode::Char(';'), KeyModifiers::empty()),
        )
        .unwrap();
    assert_no_remote_search(&app.update(Msg::Key(ctrl(KeyCode::Char('a')))));
    assert_no_remote_search(&app.update(Msg::Key(key(KeyCode::Char(';')))));
    assert!(app.local_mode.find.query.is_empty());
    assert_eq!(app.local_mode.find.input_cursor, TextCursor::default());
}

#[test]
fn printable_open_find_remap_yields_to_local_find_text_entry() {
    let mut app = local_app_before_find();
    open_and_install_corpus(&mut app);
    app.keymap
        .rebind(
            KeyContext::LocalDeck,
            Action::OpenLocalFind,
            Chord::new(KeyCode::Char('x'), KeyModifiers::empty()),
        )
        .expect("free printable Local Deck test chord");

    let commands = app.update(Msg::Key(key(KeyCode::Char('x'))));

    assert_eq!(app.mode, Mode::Search);
    assert_eq!(app.local_mode.find.query, "x");
    assert_eq!(app.local_mode.find.focus, LocalFindFocus::Input);
    assert!(matches!(
        commands.as_slice(),
        [Cmd::Local(LocalCmd::EvaluateFind { .. })]
    ));
}

#[test]
fn live_typing_keeps_input_focus_but_explicit_submit_selects_first_result() {
    let mut app = local_app_before_find();
    open_and_install_corpus(&mut app);

    let live = app.update(Msg::Key(key(KeyCode::Char('I'))));
    assert_eq!(app.local_mode.find.focus, LocalFindFocus::Input);
    apply_evaluation(&mut app, &live);
    app.local_mode.find.selected = 1;
    app.local_mode.find.focus = LocalFindFocus::Input;

    let committed = app.update(Msg::Key(key(KeyCode::Enter)));
    assert_eq!(app.local_mode.find.focus, LocalFindFocus::Results);
    assert_eq!(app.local_mode.find.selected, 0);
    apply_evaluation(&mut app, &committed);
    assert_eq!(app.local_mode.find.selected, 0);

    app.local_mode.find.focus = LocalFindFocus::Input;
    app.local_mode.find.selected = 1;
    let clicked = app.on_mouse_target(MouseTarget::LocalFindSubmit);
    assert_eq!(app.local_mode.find.focus, LocalFindFocus::Results);
    assert_eq!(app.local_mode.find.selected, 0);
    apply_evaluation(&mut app, &clicked);
    assert_eq!(app.local_mode.find.selected, 0);
}

#[test]
fn input_tab_cycles_scope_and_backtab_moves_to_results_without_stealing_text() {
    let mut app = local_app_before_find();
    open_and_install_corpus(&mut app);
    app.local_mode.find.query = "IU".to_owned();
    app.local_mode.find.focus = LocalFindFocus::Input;

    let tab = app.update(Msg::Key(key(KeyCode::Tab)));
    let (_, _, _, _, scope, _) = evaluate_command(&tab);
    assert_eq!(scope, LocalFindScope::Tracks);
    assert_eq!(app.local_mode.find.scope, LocalFindScope::Tracks);
    assert_eq!(app.local_mode.find.focus, LocalFindFocus::Input);
    apply_evaluation(&mut app, &tab);

    app.update(Msg::Key(key(KeyCode::BackTab)));
    assert_eq!(app.local_mode.find.focus, LocalFindFocus::Results);
    assert_eq!(app.local_mode.find.query, "IU");
}

#[test]
fn stale_corpus_and_query_results_are_rejected_by_generation_and_revision() {
    let mut app = local_app_before_find();
    let open = app.update(Msg::Key(ctrl(KeyCode::Char('f'))));
    assert_no_remote_search(&open);
    let stale_generation = app.local_mode.find.corpus_generation;
    let stale_corpus = corpus_for(&app);

    app.local_mode.index.revision = app.local_mode.index.revision.wrapping_add(1);
    let rebuild = app.update(Msg::Local(LocalMsg::FindCorpusReady {
        generation: stale_generation,
        corpus: Arc::clone(&stale_corpus),
    }));
    assert_no_remote_search(&rebuild);
    assert!(matches!(
        rebuild.as_slice(),
        [Cmd::Local(LocalCmd::BuildFindCorpus { .. })]
    ));
    let live_generation = app.local_mode.find.corpus_generation;
    assert_ne!(live_generation, stale_generation);
    assert!(app.local_mode.find.corpus.is_none());

    let ignored = app.update(Msg::Local(LocalMsg::FindCorpusReady {
        generation: stale_generation,
        corpus: stale_corpus,
    }));
    assert!(ignored.is_empty());
    assert!(app.local_mode.find.corpus.is_none());

    let live_corpus = corpus_for(&app);
    app.update(Msg::Local(LocalMsg::FindCorpusReady {
        generation: live_generation,
        corpus: Arc::clone(&live_corpus),
    }));
    let first = submit_and_apply(&mut app, "Palette");

    app.local_mode.find.query = "Blue".to_owned();
    let second_commands = app.submit_local_find_query();
    let (second_id, second_generation, corpus, query, scope, sort) =
        evaluate_command(&second_commands);
    let second = corpus.search(&query, scope, sort, second_id);

    let stale_request = app.update(Msg::Local(LocalMsg::FindResultsReady {
        request_id: first.generation,
        generation: second_generation,
        snapshot: first.clone(),
    }));
    assert!(stale_request.is_empty());
    assert_eq!(app.local_mode.find.snapshot.as_ref(), Some(&first));

    app.local_mode.index.revision = app.local_mode.index.revision.wrapping_add(1);
    let rebuild_generation = app.local_mode.find.corpus_generation.wrapping_add(1).max(1);
    let stale_revision = app.update(Msg::Local(LocalMsg::FindResultsReady {
        request_id: second_id,
        generation: second_generation,
        snapshot: second,
    }));
    assert!(matches!(
        stale_revision.as_slice(),
        [Cmd::Local(LocalCmd::BuildFindCorpus { generation, .. })]
            if *generation == rebuild_generation
    ));
    assert_eq!(app.local_mode.find.snapshot.as_ref(), Some(&first));
}

#[test]
fn invalid_query_keeps_the_last_valid_snapshot_visible() {
    let mut app = local_app_before_find();
    open_and_install_corpus(&mut app);
    let valid = submit_and_apply(&mut app, "Blue");

    app.local_mode.find.query = "Palette".to_owned();
    let in_flight = app.submit_local_find_query();
    let (old_request, generation, corpus, query, scope, sort) = evaluate_command(&in_flight);
    let late_snapshot = corpus.search(&query, scope, sort, old_request);

    app.local_mode.find.query = "year:not-a-year".to_owned();
    let commands = app.submit_local_find_query();
    assert_no_remote_search(&commands);
    assert!(matches!(
        commands.as_slice(),
        [Cmd::Local(LocalCmd::CancelFindEvaluations)]
    ));
    assert_ne!(app.local_mode.find.request_id, old_request);
    assert_eq!(app.local_mode.find.snapshot.as_ref(), Some(&valid));
    assert!(app.local_mode.find.parse_error.is_some());
    assert!(!app.local_mode.find.searching);

    let late = app.update(Msg::Local(LocalMsg::FindResultsReady {
        request_id: old_request,
        generation,
        snapshot: late_snapshot,
    }));
    assert!(late.is_empty());
    assert_eq!(app.local_mode.find.snapshot.as_ref(), Some(&valid));
    assert!(app.local_mode.find.parse_error.is_some());
}

#[test]
fn clearing_a_live_query_cancels_its_obsolete_worker() {
    let mut app = local_app_before_find();
    open_and_install_corpus(&mut app);
    app.local_mode.find.query = "Palette".to_owned();
    let live = app.submit_local_find_query();
    assert!(matches!(
        live.as_slice(),
        [Cmd::Local(LocalCmd::EvaluateFind { .. })]
    ));

    app.local_mode.find.query.clear();
    let cleared = app.submit_local_find_query();

    assert!(matches!(
        cleared.as_slice(),
        [Cmd::Local(LocalCmd::CancelFindEvaluations)]
    ));
    assert!(!app.local_mode.find.searching);
    assert!(app.local_mode.find.snapshot.is_none());
}

#[test]
fn enter_drills_into_collections_but_activates_tracks() {
    let mut collection_app = local_app_before_find();
    open_and_install_corpus(&mut collection_app);
    collection_app.local_mode.find.scope = LocalFindScope::Albums;
    submit_and_apply(&mut collection_app, "Aurora");
    collection_app.local_mode.find.focus = LocalFindFocus::Results;

    let drill = collection_app.update(Msg::Key(key(KeyCode::Enter)));
    assert_no_remote_search(&drill);
    assert!(drill.is_empty());
    assert_eq!(
        collection_app
            .local_mode
            .find
            .drill
            .as_ref()
            .expect("album drill")
            .track_ids
            .len(),
        2
    );
    assert_eq!(collection_app.mode, Mode::Search);

    let mut track_app = local_app_before_find();
    open_and_install_corpus(&mut track_app);
    track_app.local_mode.find.scope = LocalFindScope::Tracks;
    submit_and_apply(&mut track_app, "t:\"Palette Jam\"");
    track_app.local_mode.find.focus = LocalFindFocus::Results;

    let mut play = track_app.update(Msg::Key(key(KeyCode::Enter)));
    assert_no_remote_search(&play);
    assert!(
        play.iter()
            .flat_map(Cmd::player_commands)
            .any(|command| { matches!(command, PlayerCmd::Load(_)) })
    );
    assert!(track_app.local_mode.find.drill.is_none());
    admit_player_transition(&mut track_app, &mut play);
    assert_eq!(track_app.mode, Mode::Player);
    assert_eq!(
        track_app
            .queue
            .current()
            .and_then(|song| song.local_path.as_deref()),
        Some(std::path::Path::new("/music/Palette Jam.flac"))
    );
}

#[test]
fn escape_unwinds_drill_then_query_then_find_while_q_exits_directly() {
    let mut app = local_app_before_find();
    open_and_install_corpus(&mut app);
    app.local_mode.find.scope = LocalFindScope::Albums;
    submit_and_apply(&mut app, "Aurora");
    app.local_mode.find.focus = LocalFindFocus::Results;
    app.update(Msg::Key(key(KeyCode::Enter)));
    assert!(app.local_mode.find.drill.is_some());

    app.update(Msg::Key(key(KeyCode::Esc)));
    assert!(app.local_mode.find.drill.is_none());
    assert_eq!(app.local_mode.find.query, "Aurora");
    assert_eq!(app.mode, Mode::Search);

    app.update(Msg::Key(key(KeyCode::Esc)));
    assert!(app.local_mode.find.query.is_empty());
    assert_eq!(app.local_mode.find.focus, LocalFindFocus::Input);
    assert_eq!(app.mode, Mode::Search);

    app.update(Msg::Key(key(KeyCode::Esc)));
    assert_eq!(app.mode, Mode::Library);

    app.open_local_find();
    app.local_mode.find.scope = LocalFindScope::Albums;
    submit_and_apply(&mut app, "Aurora");
    app.local_mode.find.focus = LocalFindFocus::Results;
    app.update(Msg::Key(key(KeyCode::Enter)));
    assert!(app.local_mode.find.drill.is_some());
    app.update(Msg::Key(key(KeyCode::Char('q'))));
    assert_eq!(app.mode, Mode::Library);
}

#[test]
fn closing_find_preserves_the_underlying_local_drill_cursor_pane_and_scroll() {
    let mut app = local_app_before_find();
    app.local_mode.ui.section = LocalSection::Genres;
    app.local_mode.ui.pane = LocalPane::Sidebar;
    app.local_mode.ui.selected = 2;
    app.local_mode.ui.anchor = 1;
    app.local_mode
        .ui
        .drill
        .push(LocalDrill::Genre("Pop".to_owned()));
    app.bridges.library_scroll.resolve(10, 3, 20, 0);
    let scroll = app.bridges.library_scroll.offset();

    open_and_install_corpus(&mut app);
    app.local_mode.find.focus = LocalFindFocus::Results;
    app.update(Msg::Key(key(KeyCode::Char('q'))));

    assert_eq!(app.mode, Mode::Library);
    assert_eq!(app.local_mode.ui.section, LocalSection::Genres);
    assert_eq!(app.local_mode.ui.pane, LocalPane::Sidebar);
    assert_eq!(app.local_mode.ui.selected, 2);
    assert_eq!(app.local_mode.ui.anchor, 1);
    assert_eq!(
        app.local_mode.ui.drill,
        vec![LocalDrill::Genre("Pop".to_owned())]
    );
    assert_eq!(app.bridges.library_scroll.offset(), scroll);
}

#[test]
fn reopening_find_in_the_same_local_session_preserves_find_scroll() {
    let mut app = local_app_before_find();
    open_and_install_corpus(&mut app);
    app.bridges.local_find_scroll.view(3, 30);
    app.bridges.local_find_scroll.wheel(false, 8, 30);
    let offset = app.bridges.local_find_scroll.offset();

    app.close_local_find();
    let commands = app.open_local_find();

    assert!(commands.is_empty());
    assert_eq!(app.bridges.local_find_scroll.offset(), offset);
}

#[test]
fn local_navigation_round_trip_preserves_query_scope_drill_cursor_and_scroll() {
    let mut app = local_app_before_find();
    open_and_install_corpus(&mut app);
    app.local_mode.find.scope = LocalFindScope::Albums;
    submit_and_apply(&mut app, "Aurora");
    app.local_mode.find.focus = LocalFindFocus::Results;
    app.update(Msg::Key(key(KeyCode::Enter)));
    app.local_mode.find.selected = 1;
    app.bridges.local_find_scroll.view(1, 2);
    app.bridges.local_find_scroll.wheel(false, 1, 2);
    let offset = app.bridges.local_find_scroll.offset();

    app.close_local_find();
    let commands = app.open_local_find();

    assert!(commands.is_empty());
    assert_eq!(app.local_mode.find.query, "Aurora");
    assert_eq!(app.local_mode.find.scope, LocalFindScope::Albums);
    assert_eq!(app.local_mode.find.selected, 1);
    assert!(app.local_mode.find.drill.is_some());
    assert_eq!(app.bridges.local_find_scroll.offset(), offset);
}

#[test]
fn stale_generation_stamped_mouse_row_cannot_redirect_selection() {
    let mut app = local_app_before_find();
    open_and_install_corpus(&mut app);
    app.local_mode.find.scope = LocalFindScope::Tracks;
    let first = submit_and_apply(&mut app, "al:Aurora");
    let second = submit_and_apply(&mut app, "al:Aurora");
    assert_eq!(first.total_hits, 2);
    assert_eq!(second.total_hits, 2);
    assert_ne!(first.generation, second.generation);
    app.local_mode.find.focus = LocalFindFocus::Results;
    app.local_mode.find.selected = 0;

    let mut stale_stamp = app.local_find_pointer_stamp();
    stale_stamp.result_generation = first.generation;
    let stale = app.on_mouse_target(MouseTarget::LocalFindRow {
        index: 1,
        stamp: stale_stamp,
    });
    assert!(stale.is_empty());
    assert_eq!(app.local_mode.find.selected, 0);

    let live = app.on_mouse_target(MouseTarget::LocalFindRow {
        index: 1,
        stamp: app.local_find_pointer_stamp(),
    });
    assert!(live.is_empty());
    assert_eq!(app.local_mode.find.selected, 1);
}

#[test]
fn grouped_result_row_stamp_cannot_redirect_a_new_drill_view() {
    let mut app = local_app_before_find();
    open_and_install_corpus(&mut app);
    app.local_mode.find.scope = LocalFindScope::Albums;
    submit_and_apply(&mut app, "Aurora");
    app.local_mode.find.focus = LocalFindFocus::Results;
    let grouped_stamp = app.local_find_pointer_stamp();
    app.update(Msg::Key(key(KeyCode::Enter)));
    assert!(app.local_mode.find.drill.is_some());
    assert_eq!(app.local_mode.find.selected, 0);

    let stale = app.on_mouse_target(MouseTarget::LocalFindRow {
        index: 1,
        stamp: grouped_stamp,
    });
    assert!(stale.is_empty());
    assert_eq!(app.local_mode.find.selected, 0);

    let live = app.on_mouse_target(MouseTarget::LocalFindRow {
        index: 1,
        stamp: app.local_find_pointer_stamp(),
    });
    assert!(live.is_empty());
    assert_eq!(app.local_mode.find.selected, 1);
}

#[test]
fn result_refresh_retains_selected_stable_id_and_stale_generation_cannot_move_it() {
    let mut app = local_app_before_find();
    open_and_install_corpus(&mut app);
    app.local_mode.find.scope = LocalFindScope::Tracks;
    let first = submit_and_apply(&mut app, "IU");
    assert_eq!(first.total_hits, 2);
    app.local_mode.find.selected = 1;
    let selected_id = app.local_find_hit_at(1).expect("selected hit").id.clone();

    app.local_mode.find.query = "IU".to_owned();
    let live_commands = app.submit_local_find_query();
    let (request_id, generation, corpus, query, scope, sort) = evaluate_command(&live_commands);
    let mut reordered = corpus.search(&query, scope, sort, request_id);
    reordered.groups[0].hits.reverse();

    let stale = LocalFindSnapshot {
        generation: first.generation,
        ..first.clone()
    };
    app.update(Msg::Local(LocalMsg::FindResultsReady {
        request_id: first.generation,
        generation,
        snapshot: stale,
    }));
    assert_eq!(app.local_mode.find.selected, 1);

    app.update(Msg::Local(LocalMsg::FindResultsReady {
        request_id,
        generation,
        snapshot: reordered,
    }));
    assert_eq!(
        app.local_find_hit_at(app.local_mode.find.selected)
            .map(|hit| &hit.id),
        Some(&selected_id)
    );
    assert_eq!(app.local_mode.find.selected, 0);
}

#[test]
fn refine_cancel_discards_drafts_and_apply_submits_them_transactionally() {
    let mut app = local_app_before_find();
    open_and_install_corpus(&mut app);
    submit_and_apply(&mut app, "IU");
    app.local_mode.find.focus = LocalFindFocus::Results;

    assert!(app.update(Msg::Key(key(KeyCode::Char('/')))).is_empty());
    assert!(app.local_mode.find.refine_popup.open);
    app.update(Msg::Key(key(KeyCode::Right)));
    app.update(Msg::Key(key(KeyCode::Down)));
    app.update(Msg::Key(key(KeyCode::Right)));
    assert_eq!(app.local_mode.find.scope, LocalFindScope::All);
    assert_eq!(app.local_mode.find.sort, LocalFindSort::Relevance);
    assert_eq!(
        app.local_mode.find.refine_popup.draft_scope,
        LocalFindScope::Tracks
    );
    assert_eq!(
        app.local_mode.find.refine_popup.draft_sort,
        LocalFindSort::Title
    );

    app.update(Msg::Key(key(KeyCode::Esc)));
    assert!(!app.local_mode.find.refine_popup.open);
    assert_eq!(app.local_mode.find.scope, LocalFindScope::All);
    assert_eq!(app.local_mode.find.sort, LocalFindSort::Relevance);

    app.update(Msg::Key(key(KeyCode::Char('/'))));
    app.update(Msg::Key(key(KeyCode::Right)));
    app.update(Msg::Key(key(KeyCode::Down)));
    app.update(Msg::Key(key(KeyCode::Right)));
    let apply = app.update(Msg::Key(key(KeyCode::Enter)));
    let (_, _, _, _, scope, sort) = evaluate_command(&apply);
    assert_eq!(scope, LocalFindScope::Tracks);
    assert_eq!(sort, LocalFindSort::Title);
    assert_eq!(app.local_mode.find.scope, LocalFindScope::Tracks);
    assert_eq!(app.local_mode.find.sort, LocalFindSort::Title);
    assert!(!app.local_mode.find.refine_popup.open);
}

#[test]
fn launchpad_rows_map_to_safe_queries_and_scan_commands() {
    for (row, expected) in [
        (0, "sort:recent"),
        (1, "is:local-only"),
        (2, "is:lossless"),
        (3, "missing:artist"),
        (4, "missing:album"),
        (5, "missing:cover"),
    ] {
        let mut app = local_app_before_find();
        open_and_install_corpus(&mut app);
        app.local_mode.find.scope = LocalFindScope::Playlists;
        let commands = app.activate_local_find_launchpad(row);
        assert_no_remote_search(&commands);
        assert_eq!(app.local_mode.find.query, expected);
        assert_eq!(app.local_mode.find.scope, LocalFindScope::Tracks);
        assert!(matches!(
            commands.as_slice(),
            [Cmd::Local(LocalCmd::EvaluateFind { .. })]
        ));
    }

    let mut app = local_app_before_find();
    open_and_install_corpus(&mut app);
    let commands = app.activate_local_find_launchpad(6);
    assert_no_remote_search(&commands);
    assert!(matches!(
        commands.as_slice(),
        [Cmd::Local(LocalCmd::ScanRoots { .. })]
    ));
}

#[test]
fn scope_commands_open_the_requested_blank_launchpad_instead_of_looping() {
    let mut app = local_app_before_find();
    open_and_install_corpus(&mut app);
    submit_and_apply(&mut app, "> albums");
    app.local_mode.find.focus = LocalFindFocus::Results;

    let commands = app.update(Msg::Key(key(KeyCode::Enter)));

    assert_no_remote_search(&commands);
    assert!(commands.is_empty());
    assert_eq!(app.local_mode.find.scope, LocalFindScope::Albums);
    assert!(app.local_mode.find.query.is_empty());
    assert!(app.local_mode.find.snapshot.is_none());
    assert_eq!(app.local_mode.find.focus, LocalFindFocus::Input);
}

#[test]
fn rebuild_command_requires_explicit_confirmation_before_scanner_admission() {
    let mut app = local_app_before_find();
    open_and_install_corpus(&mut app);
    submit_and_apply(&mut app, "> rebuild");
    app.local_mode.find.focus = LocalFindFocus::Results;

    let opened = app.update(Msg::Key(key(KeyCode::Enter)));
    assert!(opened.is_empty());
    assert!(app.local_mode.find.pending_rebuild_confirm);
    assert!(!app.local_mode.index.scanning);

    let admitted = app.update(Msg::Key(key(KeyCode::Enter)));
    assert!(matches!(
        admitted.as_slice(),
        [Cmd::Local(LocalCmd::ScanRoots { .. })]
    ));
    assert!(!app.local_mode.find.pending_rebuild_confirm);
    assert!(app.local_mode.index.scanning);

    let mut cancelled = local_app_before_find();
    open_and_install_corpus(&mut cancelled);
    submit_and_apply(&mut cancelled, "> rebuild");
    cancelled.local_mode.find.focus = LocalFindFocus::Results;
    cancelled.update(Msg::Key(key(KeyCode::Enter)));
    let commands = cancelled.update(Msg::Key(key(KeyCode::Esc)));
    assert!(commands.is_empty());
    assert!(!cancelled.local_mode.find.pending_rebuild_confirm);
    assert!(!cancelled.local_mode.index.scanning);
}

#[test]
fn rebuild_confirmation_blocks_context_menu_routes() {
    let mut app = local_app_before_find();
    open_and_install_corpus(&mut app);
    submit_and_apply(&mut app, "> rebuild");
    app.local_mode.find.focus = LocalFindFocus::Results;
    app.update(Msg::Key(key(KeyCode::Enter)));
    assert!(app.local_mode.find.pending_rebuild_confirm);
    app.register_mouse_button(
        Rect::new(2, 2, 8, 1),
        MouseTarget::LocalFindRow {
            index: 0,
            stamp: app.local_find_pointer_stamp(),
        },
    );

    assert!(
        app.update(Msg::MouseRightClick { col: 3, row: 2 })
            .is_empty()
    );
    assert!(app.update(Msg::Key(shift(KeyCode::F(10)))).is_empty());
    assert!(app.overlays.context_menu.is_none());
    assert!(app.local_mode.find.pending_rebuild_confirm);
}

#[test]
fn recovery_rows_and_shortcuts_match_dynamic_scan_error_availability() {
    let mut app = local_app_before_find();
    open_and_install_corpus(&mut app);
    let empty = submit_and_apply(&mut app, "does-not-exist-anywhere");
    assert_eq!(empty.total_hits, 0);
    app.local_mode.find.focus = LocalFindFocus::Results;
    assert_eq!(app.local_find_rows_len(), 3);

    render_app(&app);
    let without_errors: Vec<usize> = app
        .hits
        .regions()
        .iter()
        .filter_map(|region| match &region.target {
            MouseTarget::LocalFindLaunchpad { index, .. } => Some(*index),
            _ => None,
        })
        .collect();
    assert_eq!(without_errors, vec![7, 6, 8]);

    app.local_mode.index.errors.push(crate::local::ScanError {
        path: "/music/broken.flac".into(),
        message: "bad fixture".to_owned(),
    });
    assert_eq!(app.local_find_rows_len(), 4);
    render_app(&app);
    let with_errors: Vec<usize> = app
        .hits
        .regions()
        .iter()
        .filter_map(|region| match &region.target {
            MouseTarget::LocalFindLaunchpad { index, .. } => Some(*index),
            _ => None,
        })
        .collect();
    assert_eq!(with_errors, vec![7, 6, 8, 9]);

    app.local_mode.find.selected = 3;
    app.local_mode.index.errors.clear();
    app.ensure_local_find_corpus();
    assert_eq!(app.local_mode.find.selected, 2);
    app.local_mode.index.errors.push(crate::local::ScanError {
        path: "/music/broken.flac".into(),
        message: "bad fixture".to_owned(),
    });
    app.local_mode.find.selected = 3;

    app.update(Msg::Key(key(KeyCode::Char('!'))));
    assert_eq!(app.mode, Mode::Library);
    assert_eq!(app.local_mode.ui.section, LocalSection::ScanErrors);

    let mut clear = local_app_before_find();
    open_and_install_corpus(&mut clear);
    submit_and_apply(&mut clear, "does-not-exist-anywhere");
    clear.local_mode.find.focus = LocalFindFocus::Results;
    clear.update(Msg::Key(key(KeyCode::Char('c'))));
    assert!(clear.local_mode.find.query.is_empty());
    assert_eq!(clear.local_mode.find.focus, LocalFindFocus::Input);

    let mut no_index = super::local::app_with_local_deck_index(Vec::new());
    open_and_install_corpus(&mut no_index);
    assert_eq!(no_index.local_find_rows_len(), 3);
    no_index
        .local_mode
        .index
        .load_errors
        .push(crate::local::ScanError {
            path: "/music/index.json".into(),
            message: "bad index fixture".to_owned(),
        });
    assert_eq!(no_index.local_find_rows_len(), 3);
    no_index.local_mode.find.focus = LocalFindFocus::Results;
    let hidden_smart_view = no_index.update(Msg::Key(key(KeyCode::Char('1'))));
    assert!(hidden_smart_view.is_empty());
    assert!(no_index.local_mode.find.query.is_empty());
}

#[test]
fn empty_index_searches_downloaded_snapshot_and_rebuilds_on_its_revision() {
    let path = std::path::PathBuf::from("/downloads/Fallback Song.flac");
    let mut downloaded = Song::from_search(
        "dQw4w9WgXcQ",
        "Fallback Song",
        "IU / SUGA",
        "3:21",
        Some("Offline Album".to_owned()),
    );
    downloaded.artists = vec!["IU".to_owned(), "SUGA".to_owned()];
    downloaded.album_artist = Some("IU".to_owned());
    downloaded.album_artists = vec!["IU".to_owned(), "Various".to_owned()];
    downloaded.album_release_date = Some("2024-04-01".to_owned());
    downloaded.disc_number = Some(2);
    downloaded.track_number = Some(4);
    downloaded.isrc = Some("KRTEST000001".to_owned());
    downloaded.origin_key = Some("spotify:track:fallback".to_owned());
    downloaded.origin_url = Some("https://example.invalid/fallback".to_owned());
    downloaded.import_session_id = Some("session-fallback".to_owned());
    downloaded.import_source_order = Some(3);
    downloaded.duration_secs = Some(201);
    let downloaded = downloaded.with_local_path(path.clone());
    let mut duplicate = downloaded.clone();
    duplicate.title = "Duplicate must not replace first".to_owned();

    let mut app = super::local::app_with_local_deck_index(Vec::new());
    app.library_ui.downloaded = vec![
        downloaded,
        duplicate,
        Song::remote("pathless", "Remote only", "Artist", "1:00"),
    ];
    app.library_ui.downloaded_rev = 7;

    let commands = app.update(Msg::Key(ctrl(KeyCode::Char('f'))));
    assert_no_remote_search(&commands);
    let (generation, tracks, playlists, revision, options) = match commands.as_slice() {
        [
            Cmd::Local(LocalCmd::BuildFindCorpus {
                generation,
                tracks,
                playlists,
                revision,
                options,
            }),
        ] => (
            *generation,
            tracks.clone(),
            playlists.clone(),
            *revision,
            options.clone(),
        ),
        _ => panic!("empty-index Find should build from the downloaded snapshot"),
    };
    assert_eq!(revision.downloads, 7);
    assert_eq!(tracks.len(), 1, "pathless and duplicate rows are excluded");
    let track = &tracks[0];
    assert_eq!(track.path, path);
    assert_eq!(track.title, "Fallback Song", "first duplicate wins");
    assert_eq!(track.artist, vec!["IU", "SUGA"]);
    assert_eq!(track.album.as_deref(), Some("Offline Album"));
    assert_eq!(track.album_artist.as_deref(), Some("IU, Various"));
    assert_eq!(track.year, Some(2024));
    assert_eq!(track.duration_ms, Some(201_000));
    assert_eq!(track.linked_video_id.as_deref(), Some("dQw4w9WgXcQ"));
    assert_eq!(track.import_source_order, Some(3));

    let corpus = Arc::new(LocalFindCorpus::build(
        &tracks, &playlists, revision, &options,
    ));
    app.update(Msg::Local(LocalMsg::FindCorpusReady { generation, corpus }));
    app.local_mode.find.scope = LocalFindScope::Albums;
    submit_and_apply(&mut app, "Offline Album");
    app.local_mode.find.focus = LocalFindFocus::Results;
    app.update(Msg::Key(key(KeyCode::Enter)));
    assert_eq!(
        app.local_find_drill_track_at(0)
            .map(|track| track.path.as_path()),
        Some(path.as_path())
    );
    app.update(Msg::Key(key(KeyCode::Esc)));

    app.local_mode.find.scope = LocalFindScope::Tracks;
    let snapshot = submit_and_apply(&mut app, "fallback");
    assert_eq!(snapshot.total_hits, 1);
    app.local_mode.find.focus = LocalFindFocus::Results;
    let epoch = app.playback.position_epoch;
    let mut play = app.update(Msg::Key(key(KeyCode::Enter)));
    assert_no_remote_search(&play);
    assert!(
        play.iter()
            .flat_map(Cmd::player_commands)
            .any(|command| matches!(command, PlayerCmd::Load(_)))
    );
    admit_player_transition(&mut app, &mut play);
    assert!(app.playback.position_epoch > epoch);
    assert_eq!(
        app.queue
            .current()
            .and_then(|song| song.local_path.as_deref()),
        Some(std::path::Path::new("/downloads/Fallback Song.flac"))
    );

    app.library_ui.downloaded_rev = 8;
    let next_generation = app.local_mode.find.corpus_generation.wrapping_add(1).max(1);
    // Play-now deliberately moves to Player. Re-entering Find observes the fallback snapshot's
    // changed revision and rebuilds before exposing any stale action targets.
    let rebuild = app.update(Msg::Key(ctrl(KeyCode::Char('f'))));
    assert_no_remote_search(&rebuild);
    assert!(matches!(
        rebuild.as_slice(),
        [Cmd::Local(LocalCmd::BuildFindCorpus {
            generation,
            revision: LocalFindCorpusRevision { downloads: 8, .. },
            ..
        })] if *generation == next_generation
    ));
}

#[test]
fn active_find_proactively_rebuilds_for_playlist_and_option_revisions() {
    let mut app = local_app_before_find();
    open_and_install_corpus(&mut app);
    assert!(app.playlists_mut().create("Fresh mix").is_some());

    let playlist_rebuild = app.update(Msg::Resize);
    assert!(matches!(
        playlist_rebuild.as_slice(),
        [Cmd::Local(LocalCmd::BuildFindCorpus { revision, .. })]
            if revision.playlists == app.playlists.revision()
    ));

    let generation = app.local_mode.find.corpus_generation;
    let corpus = corpus_for(&app);
    app.update(Msg::Local(LocalMsg::FindCorpusReady { generation, corpus }));
    app.config.download_dir = Some("/different-download-root".into());

    let option_rebuild = app.update(Msg::Resize);
    assert!(matches!(
        option_rebuild.as_slice(),
        [Cmd::Local(LocalCmd::BuildFindCorpus { .. })]
    ));
}

#[test]
fn source_refresh_keeps_a_live_drill_and_selected_track_when_still_resolvable() {
    let mut app = local_app_before_find();
    open_and_install_corpus(&mut app);
    app.local_mode.find.scope = LocalFindScope::Albums;
    submit_and_apply(&mut app, "Aurora");
    app.local_mode.find.focus = LocalFindFocus::Results;
    app.update(Msg::Key(key(KeyCode::Enter)));
    app.local_mode.find.selected = 1;
    let selected = app
        .local_mode
        .find
        .drill
        .as_ref()
        .expect("album drill")
        .track_ids[1]
        .clone();

    app.local_mode.index.revision = app.local_mode.index.revision.wrapping_add(1);
    let rebuild = app.update(Msg::Resize);
    assert!(matches!(
        rebuild.as_slice(),
        [Cmd::Local(LocalCmd::BuildFindCorpus { .. })]
    ));
    let generation = app.local_mode.find.corpus_generation;
    let corpus = corpus_for(&app);
    let evaluate = app.update(Msg::Local(LocalMsg::FindCorpusReady { generation, corpus }));

    let drill = app.local_mode.find.drill.as_ref().expect("retained drill");
    assert_eq!(drill.corpus_revision.index, app.local_mode.index.revision);
    assert_eq!(drill.track_ids[app.local_mode.find.selected], selected);
    let refreshed = apply_evaluation(&mut app, &evaluate);
    assert_eq!(
        app.local_mode
            .find
            .drill
            .as_ref()
            .expect("drill survives grouped refresh")
            .track_ids[app.local_mode.find.selected],
        selected
    );
    assert_eq!(
        refreshed.corpus_revision.index,
        app.local_mode.index.revision
    );
}

#[test]
fn scan_requested_during_index_load_is_serialized_and_failure_is_retained() {
    let mut app = local_app_before_find();
    app.local_mode.index.loading = true;

    assert!(app.request_local_scan(true).is_empty());
    assert_eq!(app.local_mode.index.pending_rescan, Some(true));

    let commands = app.apply_local_msg(LocalMsg::IndexLoaded {
        index_path: None,
        index: crate::local::LocalIndex::default(),
        warnings: Vec::new(),
    });
    assert!(matches!(
        commands.as_slice(),
        [Cmd::Local(LocalCmd::ScanRoots { .. })]
    ));
    assert!(app.local_mode.index.pending_rescan.is_none());

    assert!(app.request_local_scan(false).is_empty());
    assert_eq!(app.local_mode.index.pending_rescan, Some(false));
    let retry = app.apply_local_msg(LocalMsg::ScanFailed {
        error: "fixture failure".to_owned(),
    });
    assert!(matches!(
        retry.as_slice(),
        [Cmd::Local(LocalCmd::ScanRoots { .. })]
    ));
    assert!(app.local_find_has_scan_errors());
    assert_eq!(app.local_mode.index.errors[0].message, "fixture failure");
    assert_eq!(app.status.kind, StatusKind::Info);
    assert!(app.status.text.contains("Retrying") || app.status.text.contains("다시 시도"));
}

#[test]
fn over_capacity_bulk_requires_confirmation_and_never_partially_mutates_early() {
    let mut app = local_app_before_find();
    open_and_install_corpus(&mut app);
    app.local_mode.find.scope = LocalFindScope::Albums;
    submit_and_apply(&mut app, "Aurora");
    app.local_mode.find.focus = LocalFindFocus::Results;
    app.queue.set(songs(Queue::max_len() - 1), 0);
    let before_revision = app.queue.rev();

    let overflow = app.update(Msg::Key(key(KeyCode::Char('a'))));
    assert_no_remote_search(&overflow);
    assert!(overflow.is_empty());
    let pending = app
        .local_mode
        .find
        .pending_bulk_confirm
        .as_ref()
        .expect("capacity confirmation");
    assert_eq!(pending.track_ids.len(), 2);
    assert_eq!(pending.accepted_count, 1);
    assert_eq!(app.queue.len(), Queue::max_len() - 1);
    assert_eq!(app.queue.rev(), before_revision);

    app.update(Msg::Key(key(KeyCode::Esc)));
    assert!(app.local_mode.find.pending_bulk_confirm.is_none());
    assert_eq!(app.queue.len(), Queue::max_len() - 1);
    assert_eq!(app.queue.rev(), before_revision);

    app.update(Msg::Key(key(KeyCode::Char('a'))));
    let mut accepted = app.update(Msg::Key(key(KeyCode::Enter)));
    assert_no_remote_search(&accepted);
    assert!(app.local_mode.find.pending_bulk_confirm.is_none());
    assert_eq!(
        app.queue.len(),
        Queue::max_len() - 1,
        "commit waits for admission"
    );
    admit_player_transition(&mut app, &mut accepted);
    assert_eq!(app.queue.len(), Queue::max_len());
}

#[test]
fn bulk_confirmation_rechecks_queue_capacity_and_requires_unchanged_second_confirm() {
    let mut app = local_app_before_find();
    open_and_install_corpus(&mut app);
    app.local_mode.find.scope = LocalFindScope::Albums;
    submit_and_apply(&mut app, "Aurora");
    app.local_mode.find.focus = LocalFindFocus::Results;
    app.queue.set(songs(Queue::max_len() - 1), 0);
    app.update(Msg::Key(key(KeyCode::Char('a'))));
    assert_eq!(
        app.local_mode
            .find
            .pending_bulk_confirm
            .as_ref()
            .map(|confirm| confirm.accepted_count),
        Some(1)
    );

    app.queue.set(songs(Queue::max_len() - 2), 0);
    let changed_revision = app.queue.rev();
    let first_confirm = app.update(Msg::Key(key(KeyCode::Enter)));
    assert!(first_confirm.is_empty());
    let recalculated = app
        .local_mode
        .find
        .pending_bulk_confirm
        .as_ref()
        .expect("changed queue requires a fresh confirmation");
    assert_eq!(recalculated.accepted_count, 2);
    assert_eq!(recalculated.queue_revision, changed_revision);
    assert!(recalculated.capacity_recalculated);
    assert_eq!(app.queue.len(), Queue::max_len() - 2);

    let mut accepted = app.update(Msg::Key(key(KeyCode::Enter)));
    assert!(app.local_mode.find.pending_bulk_confirm.is_none());
    assert_eq!(app.queue.len(), Queue::max_len() - 2);
    admit_player_transition(&mut app, &mut accepted);
    assert_eq!(app.queue.len(), Queue::max_len());
}

#[test]
fn bulk_confirmation_rejects_changed_result_generation_without_mutation() {
    let mut app = local_app_before_find();
    open_and_install_corpus(&mut app);
    app.local_mode.find.scope = LocalFindScope::Albums;
    submit_and_apply(&mut app, "Aurora");
    app.local_mode.find.focus = LocalFindFocus::Results;
    app.queue.set(songs(Queue::max_len() - 1), 0);
    app.update(Msg::Key(key(KeyCode::Char('a'))));
    let queue_revision = app.queue.rev();

    submit_and_apply(&mut app, "Sailing");
    let rejected = app.confirm_local_find_bulk();

    assert!(rejected.is_empty());
    assert!(app.local_mode.find.pending_bulk_confirm.is_none());
    assert_eq!(app.queue.rev(), queue_revision);
    assert_eq!(app.queue.len(), Queue::max_len() - 1);
    assert_eq!(app.status.kind, StatusKind::Info);
}

#[test]
fn local_find_modals_capture_right_click_and_wheel_before_the_result_list() {
    let mut app = local_app_before_find();
    open_and_install_corpus(&mut app);
    app.local_mode.find.scope = LocalFindScope::Tracks;
    submit_and_apply(&mut app, "IU");
    app.local_mode.find.focus = LocalFindFocus::Results;
    app.register_mouse_button(
        Rect::new(2, 2, 1, 1),
        MouseTarget::LocalFindRow {
            index: 0,
            stamp: app.local_find_pointer_stamp(),
        },
    );
    app.bridges.local_find_scroll.view(1, 20);
    app.bridges.local_find_scroll.wheel(false, 4, 20);
    let before_scroll = app.bridges.local_find_scroll.offset();

    app.open_local_find_refine();
    assert!(
        app.update(Msg::MouseRightClick { col: 2, row: 2 })
            .is_empty()
    );
    assert!(app.overlays.context_menu.is_none());
    assert!(app.local_mode.find.refine_popup.open);
    app.update(Msg::MouseScroll {
        up: false,
        col: 2,
        row: 2,
        ctrl: false,
    });
    assert_eq!(app.bridges.local_find_scroll.offset(), before_scroll);

    app.local_mode.find.refine_popup.open = false;
    app.queue.set(songs(Queue::max_len() - 1), 0);
    app.local_mode.find.scope = LocalFindScope::Albums;
    submit_and_apply(&mut app, "Aurora");
    app.local_mode.find.focus = LocalFindFocus::Results;
    app.update(Msg::Key(key(KeyCode::Char('a'))));
    assert!(app.local_mode.find.pending_bulk_confirm.is_some());
    assert!(
        app.update(Msg::MouseRightClick { col: 2, row: 2 })
            .is_empty()
    );
    assert!(app.overlays.context_menu.is_none());
    assert!(app.local_mode.find.pending_bulk_confirm.is_some());
}

#[test]
fn local_find_context_menu_uses_stable_identity_and_supports_keyboard_fallback() {
    let mut app = local_app_before_find();
    open_and_install_corpus(&mut app);
    app.local_mode.find.scope = LocalFindScope::Tracks;
    submit_and_apply(&mut app, "Palette");
    app.local_mode.find.focus = LocalFindFocus::Results;
    render_app(&app);
    let target = MouseTarget::LocalFindRow {
        index: 0,
        stamp: app.local_find_pointer_stamp(),
    };
    let (col, row) = button_center(&app, target.clone());

    let opened = app.update(Msg::MouseRightClick { col, row });
    assert!(opened.is_empty());
    let menu = app
        .overlays
        .context_menu
        .as_ref()
        .expect("Local Find right click should use the Local context menu");
    assert_eq!(menu.items.len(), 3);
    assert_eq!(app.queue.len(), 0);

    submit_and_apply(&mut app, "Blue");
    let stale = app.activate_context_menu_item(0);
    assert!(stale.is_empty());
    assert_eq!(app.queue.len(), 0);
    assert_eq!(app.status.kind, StatusKind::Info);

    app.local_mode.find.scope = LocalFindScope::Tracks;
    submit_and_apply(&mut app, "Palette");
    app.local_mode.find.focus = LocalFindFocus::Results;
    app.local_mode.find.selected = 0;
    render_app(&app);
    let rect = app
        .hits
        .rect_of_target(MouseTarget::LocalFindRow {
            index: 0,
            stamp: app.local_find_pointer_stamp(),
        })
        .expect("selected Local Find row is visible");
    app.update(Msg::Key(shift(KeyCode::F(10))));
    let keyboard_menu = app
        .overlays
        .context_menu
        .as_ref()
        .expect("Shift+F10 should open the selected Local Find row menu");
    assert_eq!(keyboard_menu.anchor_row, rect.y);
    assert_eq!(keyboard_menu.anchor_col, rect.x.saturating_add(1));
    let anchor = (keyboard_menu.anchor_col, keyboard_menu.anchor_row);
    let double = app.update(Msg::MouseRightDoubleClick {
        col: anchor.0,
        row: anchor.1,
    });
    assert!(
        double
            .iter()
            .flat_map(Cmd::player_commands)
            .any(|command| matches!(command, PlayerCmd::Load(_)))
    );
}

#[test]
fn local_find_collection_context_activate_drills_play_now_loads_and_command_is_activate_only() {
    let mut app = local_app_before_find();
    open_and_install_corpus(&mut app);
    app.local_mode.find.scope = LocalFindScope::Albums;
    submit_and_apply(&mut app, "Aurora");
    app.local_mode.find.focus = LocalFindFocus::Results;
    render_app(&app);
    let target = MouseTarget::LocalFindRow {
        index: 0,
        stamp: app.local_find_pointer_stamp(),
    };
    let (col, row) = button_center(&app, target);
    app.update(Msg::MouseRightClick { col, row });
    let mut play = app.activate_context_menu_item(1);
    assert!(
        play.iter()
            .flat_map(Cmd::player_commands)
            .any(|command| matches!(command, PlayerCmd::Load(_)))
    );
    admit_player_transition(&mut app, &mut play);

    app.mode = Mode::Search;
    app.local_mode.find.focus = LocalFindFocus::Results;
    render_app(&app);
    let (col, row) = button_center(
        &app,
        MouseTarget::LocalFindRow {
            index: 0,
            stamp: app.local_find_pointer_stamp(),
        },
    );
    app.update(Msg::MouseRightClick { col, row });
    app.activate_context_menu_item(0);
    assert!(app.local_mode.find.drill.is_some());

    app.local_mode.find.drill = None;
    app.mode = Mode::Search;
    app.local_mode.find.scope = LocalFindScope::All;
    submit_and_apply(&mut app, "> rescan");
    app.local_mode.find.focus = LocalFindFocus::Results;
    assert!(matches!(
        app.local_find_hit_at(0).map(|hit| &hit.id),
        Some(LocalFindHitId::Command(_))
    ));
    render_app(&app);
    let (col, row) = button_center(
        &app,
        MouseTarget::LocalFindRow {
            index: 0,
            stamp: app.local_find_pointer_stamp(),
        },
    );
    app.update(Msg::MouseRightClick { col, row });
    assert_eq!(
        app.overlays
            .context_menu
            .as_ref()
            .expect("command menu")
            .items
            .len(),
        1
    );
}

#[test]
fn local_find_typing_submit_refine_and_launchpad_never_emit_network_search() {
    let mut app = local_app_before_find();
    let corpus = open_and_install_corpus(&mut app);

    for ch in "Palette".chars() {
        let commands = app.update(Msg::Key(key(KeyCode::Char(ch))));
        assert_no_remote_search(&commands);
        assert!(matches!(
            commands.as_slice(),
            [Cmd::Local(LocalCmd::EvaluateFind { .. })]
        ));
    }
    let submit = app.update(Msg::Key(key(KeyCode::Enter)));
    assert_no_remote_search(&submit);

    let request_id = app.local_mode.find.request_id;
    let snapshot = corpus.search(
        &LocalFindQuery::parse(&app.local_mode.find.query).unwrap(),
        app.local_mode.find.scope,
        app.local_mode.find.sort,
        request_id,
    );
    let result = app.update(Msg::Local(LocalMsg::FindResultsReady {
        request_id,
        generation: app.local_mode.find.corpus_generation,
        snapshot,
    }));
    assert_no_remote_search(&result);

    app.local_mode.find.focus = LocalFindFocus::Results;
    assert_no_remote_search(&app.update(Msg::Key(key(KeyCode::Char('/')))));
    assert_no_remote_search(&app.update(Msg::Key(key(KeyCode::Esc))));
    assert_no_remote_search(&app.activate_local_find_launchpad(0));
}
