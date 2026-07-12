use super::tests::engine_with_queue;
use super::*;

fn external_song(id: &str, title: &str) -> Song {
    Song::from_source(
        crate::search_source::SearchSource::Jamendo,
        id,
        title,
        "artist",
        "2:00",
        crate::api::PlayableRef::DirectUrl {
            source: crate::search_source::SearchSource::Jamendo,
            url: format!("https://cdn.example/{id}.mp3"),
        },
    )
}

fn group(song: Song) -> crate::api::GuiSearchGroup {
    crate::api::GuiSearchGroup {
        source: crate::search_source::SearchSource::Jamendo,
        songs: vec![song],
        error: None,
    }
}

fn wire_id(id: &str) -> String {
    crate::api::gui_search_row_id(&external_song(id, "wire identity"))
}

fn requester(session_id: u64, page_id: &str) -> RequesterKey {
    RequesterKey::new(session_id, Some(page_id.to_owned()))
}

#[tokio::test]
async fn equal_tickets_preserve_distinct_session_and_page_requesters() {
    let mut engine = engine_with_queue(&[]);
    let scope_a = requester(1, "page-a");
    let scope_b = requester(2, "page-b");

    for (scope, query) in [(scope_a.clone(), "alpha"), (scope_b.clone(), "beta")] {
        let (response, shutdown, effects) = engine
            .handle_session_remote(
                RemoteCommand::RunSearch {
                    ticket: 1,
                    query: query.to_owned(),
                    source: crate::search_source::SearchSource::All,
                },
                scope.clone(),
            )
            .await;
        assert!(response.ok);
        assert!(!shutdown);
        assert!(matches!(
            effects.as_slice(),
            [EngineEffect::GuiSearch { requester, ticket: 1, query: effect_query, .. }]
                if requester.session_id() == scope.session_id()
                    && requester.page_id() == scope.page_id()
                    && effect_query == query
        ));
    }

    let song_a = external_song("a", "A");
    let song_b = external_song("b", "B");
    assert!(engine.complete_gui_search(&scope_a, 1, &[group(song_a.clone())]));
    assert!(engine.complete_gui_search(&scope_b, 1, &[group(song_b.clone())]));
    assert_eq!(
        engine
            .resolve_video_id(Some(&scope_a), &wire_id("a"))
            .unwrap()
            .title,
        "A"
    );
    assert!(
        engine
            .resolve_video_id(Some(&scope_a), &wire_id("b"))
            .is_none(),
        "session A must not resolve session B's visible row"
    );
    assert_eq!(
        engine
            .resolve_video_id(Some(&scope_b), &wire_id("b"))
            .unwrap()
            .title,
        "B"
    );
}

#[tokio::test]
async fn duplicate_active_attempt_is_coalesced_and_ticket_reuse_is_fenced() {
    let mut engine = engine_with_queue(&[]);
    let scope = requester(3, "page");
    let command = RemoteCommand::RunSearch {
        ticket: 4,
        query: "same query".to_owned(),
        source: crate::search_source::SearchSource::All,
    };

    let (first, _, first_effects) = engine
        .handle_session_remote(command.clone(), scope.clone())
        .await;
    let (retry, _, retry_effects) = engine.handle_session_remote(command, scope.clone()).await;
    assert!(first.ok && retry.ok);
    assert!(matches!(
        first_effects.as_slice(),
        [EngineEffect::GuiSearch { .. }]
    ));
    assert!(
        retry_effects.is_empty(),
        "retry must join the active attempt"
    );

    let (conflict, _, conflict_effects) = engine
        .handle_session_remote(
            RemoteCommand::RunSearch {
                ticket: 4,
                query: "different query".to_owned(),
                source: crate::search_source::SearchSource::All,
            },
            scope.clone(),
        )
        .await;
    assert_eq!(conflict.reason.as_deref(), Some("ticket_conflict"));
    assert!(conflict_effects.is_empty());

    let (newer, _, _) = engine
        .handle_session_remote(
            RemoteCommand::RunSearch {
                ticket: 5,
                query: "newer".to_owned(),
                source: crate::search_source::SearchSource::All,
            },
            scope.clone(),
        )
        .await;
    assert!(newer.ok);
    let (stale, _, stale_effects) = engine
        .handle_session_remote(
            RemoteCommand::RunSearch {
                ticket: 4,
                query: "same query".to_owned(),
                source: crate::search_source::SearchSource::All,
            },
            scope,
        )
        .await;
    assert_eq!(stale.reason.as_deref(), Some("stale_ticket"));
    assert!(stale_effects.is_empty());
}

#[tokio::test]
async fn older_completion_cannot_replace_a_newer_ticket_on_the_same_page() {
    let mut engine = engine_with_queue(&[]);
    let scope = requester(7, "same-page");
    for (ticket, query) in [(1, "old"), (2, "new")] {
        let (response, _, _) = engine
            .handle_session_remote(
                RemoteCommand::RunSearch {
                    ticket,
                    query: query.to_owned(),
                    source: crate::search_source::SearchSource::All,
                },
                scope.clone(),
            )
            .await;
        assert!(response.ok);
    }

    assert!(engine.complete_gui_search(&scope, 2, &[group(external_song("new", "new"))],));
    assert!(!engine.gui_search_is_current(&scope, 1));
    assert!(!engine.complete_gui_search(&scope, 1, &[group(external_song("old", "old"))],));
    assert!(
        engine
            .resolve_video_id(Some(&scope), &wire_id("new"))
            .is_some()
    );
    assert!(
        engine
            .resolve_video_id(Some(&scope), &wire_id("old"))
            .is_none()
    );
}

#[tokio::test]
async fn older_completion_cannot_push_after_newer_ticket_on_the_same_live_page() {
    let (hub, session, mut rx) =
        crate::remote::test_register(crate::remote::SessionTuning::default());
    assert_eq!(
        session.admit_subscribe(Some("same-page"), || Some(true)),
        crate::remote::SubscribeIngress::Accepted
    );
    let mut publisher = crate::remote::publish::Publisher::new(hub);
    assert!(publisher.handle_subscribe(
        &crate::remote::publish::test_view(&crate::queue::Queue::default()),
        &session,
        Some("same-page"),
        1,
        &[crate::remote::proto::Topic::Search],
    ));
    while rx.try_recv().is_ok() {}
    let scope = crate::remote::RemoteSessionScope::new(session, Some("same-page".to_string()));
    let requester = RequesterKey::new(scope.session_id(), scope.page_id().map(str::to_owned));
    let mut engine = engine_with_queue(&[]);
    for (ticket, query) in [(1, "old"), (2, "new")] {
        let (response, _, _) = engine
            .handle_session_remote(
                RemoteCommand::RunSearch {
                    ticket,
                    query: query.to_owned(),
                    source: crate::search_source::SearchSource::All,
                },
                requester.clone(),
            )
            .await;
        assert!(response.ok);
    }

    let long_source_id = "x".repeat(crate::api::MAX_PROVIDER_ID_CHARS + 20);
    let newest_song = external_song(&long_source_id, "new");
    let raw_id = newest_song.video_id.clone();
    let expected_wire_id = crate::api::gui_search_row_id(&newest_song);
    let newest = group(newest_song);
    let newest_pending = super::super::PendingGuiSearch {
        requester_key: requester.clone(),
        requester: scope.clone(),
        ticket: 2,
        query: "new".to_owned(),
        source: crate::search_source::SearchSource::All,
    };
    assert!(super::super::route_gui_search_completion(
        &mut engine,
        &publisher,
        &newest_pending,
        std::slice::from_ref(&newest),
    ));
    let Ok(crate::remote::SessionLine::Event { topic, payload, .. }) = rx.try_recv() else {
        panic!("search completion was not pushed");
    };
    assert_eq!(topic, crate::remote::proto::Topic::Search);
    let event: crate::remote::proto::PushEvent = serde_json::from_slice(&payload).unwrap();
    let crate::remote::proto::PushEvent::SearchCompleted { groups, .. } = event else {
        panic!("unexpected search event");
    };
    let pushed_wire_id = &groups[0].tracks[0].video_id;
    assert_eq!(pushed_wire_id, &expected_wire_id);
    assert_ne!(pushed_wire_id, &raw_id);
    assert!(pushed_wire_id.len() <= crate::remote::proto::REMOTE_MAX_TRACK_ID_BYTES);

    let stale = group(external_song("old", "old"));
    let stale_pending = super::super::PendingGuiSearch {
        requester_key: requester.clone(),
        requester: scope.clone(),
        ticket: 1,
        query: "old".to_owned(),
        source: crate::search_source::SearchSource::All,
    };
    assert!(!super::super::route_gui_search_completion(
        &mut engine,
        &publisher,
        &stale_pending,
        std::slice::from_ref(&stale),
    ));
    assert!(matches!(
        rx.try_recv(),
        Err(tokio::sync::mpsc::error::TryRecvError::Empty)
    ));
    assert!(
        engine
            .resolve_video_id(Some(&requester), &expected_wire_id)
            .is_some()
    );
    assert!(
        engine
            .resolve_video_id(Some(&requester), &wire_id("old"))
            .is_none()
    );
}

#[test]
fn requester_index_is_session_bounded_and_page_replacement_isolated() {
    let mut engine = engine_with_queue(&[]);
    let stable = requester(500, "stable");
    engine.index_gui_search(&stable, &[group(external_song("stable", "stable"))]);
    for page in 0..crate::remote::MAX_SESSIONS + 2 {
        let churn = requester(501, &format!("page-{page}"));
        engine.index_gui_search(&churn, &[group(external_song(&format!("p{page}"), "row"))]);
    }
    assert!(
        engine
            .resolve_video_id(Some(&stable), &wire_id("stable"))
            .is_some()
    );

    for id in 0..crate::remote::MAX_SESSIONS + 2 {
        let scope = requester(1000 + id as u64, "page");
        engine.index_gui_search(&scope, &[group(external_song(&id.to_string(), "row"))]);
    }
    assert_eq!(engine.gui_search_index.len(), crate::remote::MAX_SESSIONS);
    let oldest = requester(1000, "page");
    assert!(
        engine
            .resolve_video_id(Some(&oldest), &wire_id("0"))
            .is_none()
    );

    let old_page = requester(99, "old");
    let new_page = requester(99, "new");
    engine.index_gui_search(&old_page, &[group(external_song("old", "old"))]);
    engine.index_gui_search(&new_page, &[group(external_song("new", "new"))]);
    assert!(
        engine
            .resolve_video_id(Some(&new_page), &wire_id("old"))
            .is_none()
    );
    assert!(
        engine
            .resolve_video_id(Some(&new_page), &wire_id("new"))
            .is_some()
    );
}

#[test]
fn value_index_retains_only_the_bounded_fifo_without_live_session_handles() {
    let live = requester(1, "live");
    let stale = requester(2, "old");
    let replacement = requester(3, "replacement");
    let mut engine = engine_with_queue(&[]);
    engine.index_gui_search(&live, &[group(external_song("live", "live"))]);
    for id in 0..crate::remote::MAX_SESSIONS.saturating_sub(2) {
        let filler = requester(100 + id as u64, "page");
        engine.index_gui_search(
            &filler,
            &[group(external_song(&format!("f{id}"), "filler"))],
        );
    }
    engine.index_gui_search(&stale, &[group(external_song("old", "old"))]);
    assert_eq!(engine.gui_search_index.len(), crate::remote::MAX_SESSIONS);

    engine.index_gui_search(
        &replacement,
        &[group(external_song("replacement", "replacement"))],
    );

    assert_eq!(engine.gui_search_index.len(), crate::remote::MAX_SESSIONS);
    assert!(
        engine
            .resolve_video_id(Some(&live), &wire_id("live"))
            .is_none(),
        "value-only rows may remain inert until the deterministic FIFO cap evicts them"
    );
    assert!(
        engine
            .resolve_video_id(Some(&replacement), &wire_id("replacement"))
            .is_some()
    );
}

#[tokio::test]
async fn mixed_stale_track_selection_is_rejected_without_queue_mutation() {
    let mut engine = engine_with_queue(&["seed"]);
    let requester = requester(70, "results");
    let visible = external_song("known", "Known");
    let visible_id = crate::api::gui_search_row_id(&visible);
    engine.index_gui_search(&requester, &[group(visible)]);
    engine.streaming = true;
    let before_rev = engine.queue.rev();
    let before_len = engine.queue.len();
    let before_current = engine.queue.current().map(|song| song.video_id.clone());

    for command in [
        RemoteCommand::PlayTracks {
            video_ids: vec![visible_id.clone(), "gui:jamendo:missing".to_owned()],
        },
        RemoteCommand::EnqueueTracks {
            video_ids: vec![visible_id.clone(), "gui:jamendo:missing".to_owned()],
        },
    ] {
        let (response, shutdown, effects) = engine
            .handle_session_remote(command, requester.clone())
            .await;
        assert_eq!(response.reason.as_deref(), Some("stale_results"));
        assert!(!shutdown);
        assert!(effects.is_empty());
        assert!(!engine.streaming_pending);
        assert_eq!(engine.queue.rev(), before_rev);
        assert_eq!(engine.queue.len(), before_len);
        assert_eq!(
            engine.queue.current().map(|song| song.video_id.clone()),
            before_current
        );
    }
}

#[tokio::test]
async fn insufficient_capacity_rejects_entire_track_selection_without_mutation() {
    let mut engine = engine_with_queue(&[]);
    let fill = (0..REMOTE_MAX_TRACK_IDS - 1)
        .map(|id| Song::remote(format!("fill-{id}"), "fill", "artist", "1:00"))
        .collect();
    assert_eq!(engine.queue.extend(fill), REMOTE_MAX_TRACK_IDS - 1);
    let before_rev = engine.queue.rev();
    let before_len = engine.queue.len();
    let before_current = engine.queue.current().map(|song| song.video_id.clone());
    let selection = vec!["aaaaaaaaaaa".to_owned(), "bbbbbbbbbbb".to_owned()];

    for command in [
        RemoteCommand::PlayTracks {
            video_ids: selection.clone(),
        },
        RemoteCommand::EnqueueTracks {
            video_ids: selection.clone(),
        },
    ] {
        let (response, shutdown, effects) = engine.handle_remote(command).await;
        assert_eq!(response.reason.as_deref(), Some("queue_full"));
        assert!(!shutdown);
        assert!(effects.is_empty());
        assert_eq!(engine.queue.rev(), before_rev);
        assert_eq!(engine.queue.len(), before_len);
        assert_eq!(
            engine.queue.current().map(|song| song.video_id.clone()),
            before_current
        );
    }
}
