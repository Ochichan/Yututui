use super::*;

#[test]
fn g_enters_ai_from_player_and_library() {
    let mut app = app_playing(1, 0);
    app.update(Msg::Key(key(KeyCode::Char('g'))));
    assert_eq!(app.mode, Mode::Ai);
    assert_eq!(app.ai.focus, AiFocus::Input);
    // And from the library view.
    let mut app = app_playing(1, 0);
    app.update(Msg::Key(key(KeyCode::Char('l'))));
    app.update(Msg::Key(key(KeyCode::Char('g'))));
    assert_eq!(app.mode, Mode::Ai);
}

#[test]
fn ai_submit_without_key_shows_onboarding_error() {
    let mut app = app_playing(1, 0); // ai_available defaults to false
    app.update(Msg::Key(key(KeyCode::Char('g'))));
    for c in "play jazz".chars() {
        app.update(Msg::Key(key(KeyCode::Char(c))));
    }
    let cmds = app.update(Msg::Key(key(KeyCode::Enter)));
    assert!(ask_ai(&cmds).is_none(), "no AskAi without a key");
    assert!(!app.ai.thinking);
    // Transcript holds the user prompt plus an error line.
    assert_eq!(app.ai.messages.last().unwrap().role, AiRole::Error);
    assert!(
        app.ai
            .messages
            .iter()
            .any(|m| m.role == AiRole::User && m.text == "play jazz")
    );
}

#[test]
fn ai_submit_with_key_emits_ask_and_sets_thinking() {
    let mut app = app_playing(1, 0);
    app.ai.available = true;
    app.update(Msg::Key(key(KeyCode::Char('g'))));
    for c in "play lofi".chars() {
        app.update(Msg::Key(key(KeyCode::Char(c))));
    }
    let cmds = app.update(Msg::Key(key(KeyCode::Enter)));
    assert_eq!(ask_ai(&cmds), Some("play lofi"));
    assert!(app.ai.thinking);
    assert!(app.ai.input.is_empty());
    // A second submit while thinking is ignored (no duplicate request).
    for c in "more".chars() {
        app.update(Msg::Key(key(KeyCode::Char(c))));
    }
    let cmds = app.update(Msg::Key(key(KeyCode::Enter)));
    assert!(ask_ai(&cmds).is_none());
}

#[test]
fn ai_play_tracks_on_empty_queue_starts_playback() {
    let mut app = App::new(100);
    assert!(app.queue.is_empty());
    let cmds = app.update(AiMsg::PlayTracks(songs(3)));
    assert_eq!(current(&app), "id0");
    assert_loads_video(&cmds, "id0");
}

#[test]
fn ai_enqueue_reports_count_and_extends() {
    let _guard = crate::i18n::lock_for_test();
    let mut app = app_playing(2, 0); // queue has id0, id1
    app.update(AiMsg::Enqueue(songs(3)));
    assert_eq!(app.queue.len(), 5);
    assert!(app.status.text.contains("Queued"));
}

#[test]
fn ai_error_clears_thinking() {
    let mut app = app_playing(1, 0);
    app.ai.thinking = true;
    app.update(AiMsg::Error("boom".to_owned()));
    assert!(!app.ai.thinking);
    assert_eq!(app.ai.messages.last().unwrap().role, AiRole::Error);
}

#[test]
fn ai_empty_chat_is_not_appended() {
    let mut app = app_playing(1, 0);
    app.update(AiMsg::Chat("   ".to_owned()));
    assert!(app.ai.messages.is_empty());
    app.update(AiMsg::Chat("here you go".to_owned()));
    assert_eq!(app.ai.messages.len(), 1);
}

#[test]
fn ai_transcript_scrolls_history_and_new_chat_snaps_to_latest() {
    let mut app = App::new(100);
    app.mode = Mode::Ai;
    for i in 0..30 {
        app.ai.messages.push(AiMessage {
            role: AiRole::Ai,
            text: format!("message {i}"),
        });
    }

    let backend = TestBackend::new(80, 18);
    let mut terminal = Terminal::new(backend).unwrap();
    terminal.draw(|f| crate::ui::render(f, &app)).unwrap();
    let viewport = app.bridges.ai_transcript_scroll.viewport();
    let content_len = app.bridges.ai_transcript_copy_lines.borrow().len();
    assert!(content_len > viewport, "transcript should overflow");
    assert_eq!(
        app.bridges.ai_transcript_scroll.offset(),
        content_len - viewport,
        "first render should show the newest chat"
    );
    let row = app
        .hits
        .regions()
        .iter()
        .find_map(|b| match b.target {
            MouseTarget::AiTranscriptRow(_) => Some(b.rect),
            _ => None,
        })
        .expect("rendered transcript row");

    app.update(Msg::MouseScroll {
        up: true,
        col: row.x,
        row: row.y,
        ctrl: false,
    });
    assert!(
        app.bridges.ai_transcript_scroll.offset() < content_len - viewport,
        "wheel up should move to older chat"
    );

    app.update(AiMsg::Chat("fresh answer".to_owned()));
    terminal.draw(|f| crate::ui::render(f, &app)).unwrap();
    let content_len = app.bridges.ai_transcript_copy_lines.borrow().len();
    assert_eq!(
        app.bridges.ai_transcript_scroll.offset(),
        content_len - app.bridges.ai_transcript_scroll.viewport(),
        "new chat should snap back to the latest line"
    );
}

#[test]
fn dragging_ai_transcript_rows_copies_selection() {
    let _guard = crate::i18n::lock_for_test();
    let mut app = App::new(100);
    app.mode = Mode::Ai;
    app.ai.messages.push(AiMessage {
        role: AiRole::User,
        text: "play jazz".to_owned(),
    });
    app.ai.messages.push(AiMessage {
        role: AiRole::Ai,
        text: "queued something mellow".to_owned(),
    });

    let backend = TestBackend::new(80, 18);
    let mut terminal = Terminal::new(backend).unwrap();
    terminal.draw(|f| crate::ui::render(f, &app)).unwrap();
    let rows: Vec<Rect> = app
        .hits
        .regions()
        .iter()
        .filter_map(|b| match b.target {
            MouseTarget::AiTranscriptRow(_) => Some(b.rect),
            _ => None,
        })
        .collect();
    assert!(rows.len() >= 2, "need at least two transcript rows");

    app.update(Msg::MouseClick {
        col: rows[0].x,
        row: rows[0].y,
        multi: false,
    });
    app.update(Msg::MouseDrag {
        col: rows[1].x,
        row: rows[1].y,
    });
    app.update(Msg::MouseLeftUp);

    assert_eq!(app.status.kind, StatusKind::Info);
    assert_eq!(
        app.status.text,
        t!(
            "✓ Chat selection copied to clipboard",
            "✓ 선택한 채팅이 클립보드에 복사됐어요"
        )
    );
}

#[test]
fn ai_submit_button_matches_enter_submit() {
    let mut app = App::new(100);
    app.mode = Mode::Ai;
    app.ai.available = true;
    app.ai.input = "play lofi".to_owned();

    let backend = TestBackend::new(80, 18);
    let mut terminal = Terminal::new(backend).unwrap();
    terminal.draw(|f| crate::ui::render(f, &app)).unwrap();
    let button = app
        .hits
        .regions()
        .iter()
        .find(|b| b.target == MouseTarget::AiSubmit)
        .map(|b| b.rect)
        .expect("rendered DJ Gem submit button");

    let cmds = app.update(Msg::MouseClick {
        col: button.x,
        row: button.y,
        multi: false,
    });
    assert_eq!(ask_ai(&cmds), Some("play lofi"));
    assert!(app.ai.thinking);
    assert!(app.ai.input.is_empty());
}

#[test]
fn ai_model_label_renders_under_prompt_and_not_on_nav_border() {
    let mut app = App::new(100);
    app.mode = Mode::Ai;
    app.ai.model = crate::ai::GeminiModel::Latest;

    let buf = render_app_buffer(&app, 80, 24);
    assert!(
        !buffer_row(&buf, 0).contains("Latest"),
        "model label should no longer ride the top nav border"
    );
    assert!(
        (0..buf.area.height).any(|y| buffer_row(&buf, y).contains("Model: Latest")),
        "model label should render below the prompt"
    );

    let model = app
        .hits
        .regions()
        .iter()
        .find(|b| b.target == MouseTarget::AiModel)
        .map(|b| b.rect)
        .expect("model label hit rect");
    let input = app
        .hits
        .regions()
        .iter()
        .find(|b| b.target == MouseTarget::AiInput)
        .map(|b| b.rect)
        .expect("prompt input hit rect");
    assert!(
        model.y > input.y,
        "model label should sit below prompt input: model={model:?} input={input:?}"
    );
}

#[test]
fn clicking_ai_model_label_cycles_model_live_and_persists() {
    let mut app = App::new(100);
    app.mode = Mode::Ai;
    app.ai.model = crate::ai::GeminiModel::FlashLite;
    app.config.gemini_model = crate::ai::GeminiModel::FlashLite;
    let next = app.ai.model.cycled(true);

    let cmds = click_target(&mut app, MouseTarget::AiModel);

    assert_eq!(app.ai.model, next);
    assert_eq!(app.config.gemini_model, next);
    assert_eq!(save_config(&cmds).unwrap().gemini_model, next);
    assert!(
        cmds.iter()
            .any(|c| matches!(c, Cmd::SetAiModel(model) if *model == next)),
        "click should hot-swap the running DJ Gem actor"
    );
    assert_eq!(app.status.kind, StatusKind::Info);
    assert!(app.status.text.contains(next.label()));
}

#[test]
fn ai_suggestion_rows_are_clickable_choices() {
    let mut app = App::new(100);
    app.mode = Mode::Ai;
    app.ai.messages.push(AiMessage {
        role: AiRole::User,
        text: "hide onboarding art".to_owned(),
    });
    app.ai.suggestions = songs(4);

    let backend = TestBackend::new(80, 20);
    let mut terminal = Terminal::new(backend).unwrap();
    terminal.draw(|f| crate::ui::render(f, &app)).unwrap();
    let row = app
        .hits
        .regions()
        .iter()
        .find_map(|b| match b.target {
            MouseTarget::AiSuggestionRow(2) => Some(b.rect),
            _ => None,
        })
        .expect("rendered DJ Gem suggestion row");

    app.update(Msg::MouseClick {
        col: row.x,
        row: row.y,
        multi: false,
    });
    assert_eq!(app.ai.focus, AiFocus::Suggestions);
    assert_eq!(app.ai.suggestions_selected, 2);
}

#[test]
fn ai_streaming_circuit_breaker_disables_after_repeated_empties() {
    let _guard = crate::i18n::lock_for_test();
    let mut app = app_playing(1, 0);
    app.autoplay_streaming = true;
    for _ in 0..AUTOPLAY_MAX_FAILURES {
        app.update(AiMsg::Enqueue(Vec::new())); // resolves nothing
    }
    assert!(
        !app.autoplay_streaming,
        "streaming disabled after repeated empty extends"
    );
    assert!(app.status.text.contains("Autoplay stopped"));
}

#[test]
fn autoplay_extends_when_queue_runs_low() {
    let mut app = app_playing(2, 0); // remaining = 1 (<= threshold)
    app.ai.available = true;
    app.autoplay_streaming = true;
    // A manual next advances and should fetch the candidate pool first (both DJ Gem and non-DJ Gem
    // paths share one pool; the DJ Gem reranks it once it returns).
    let cmds = app.update(Msg::Key(key(KeyCode::Char('.'))));
    assert!(
        streaming_fallback(&cmds).is_some(),
        "autoplay should fetch a candidate pool"
    );
    assert!(
        ask_ai(&cmds).is_none(),
        "no free-form DJ Gem streaming prompt anymore"
    );
    assert!(app.streaming.pending);
    assert!(
        !app.ai.thinking,
        "the rerank only starts once the pool returns"
    );
    // The cooldown / in-flight guard blocks an immediate second request.
    let cmds = app.update(Msg::Key(key(KeyCode::Char('.'))));
    assert!(streaming_fallback(&cmds).is_none());
}

#[test]
fn radio_tab_entries_do_not_feed_station_state() {
    let mut app = app_playing(1, 0);
    let normal_favorite = Song::remote("fav-song", "Favorite", "Song Artist", "0:10");
    let normal_history = Song::remote("hist-song", "History", "History Artist", "0:10");
    let radio_favorite = radio_station("fav-radio");
    let radio_recent = radio_station("recent-radio");

    app.library.favorites.push(normal_favorite);
    app.library.history.push_front(normal_history);
    app.library.radio_favorites.push(radio_favorite.clone());
    app.library.radios.push_front(radio_recent.clone());

    let st = app.build_station_state("id0");
    let normal_fav_artist = crate::signals::normalize_artist("Song Artist");
    let radio_artist = crate::signals::normalize_artist("KR / MP3");

    assert!(st.favorite_artist_keys.contains(&normal_fav_artist));
    assert!(!st.favorite_artist_keys.contains(&radio_artist));
    assert!(!st.recent_track_ids.contains(&radio_favorite.video_id));
    assert!(!st.recent_track_ids.contains(&radio_recent.video_id));
    assert!(!st.recent_artist_keys.contains(&radio_artist));
}

#[test]
fn ai_streaming_hands_a_local_shortlist_to_the_reranker() {
    let mut app = app_playing(1, 0); // current id0 is already in history
    let current = app.queue.current().cloned().unwrap();
    app.library
        .record_play(&Song::remote("prev2", "previous two", "artist b", "0:10"));
    app.library
        .record_play(&Song::remote("prev1", "previous one", "artist a", "0:10"));
    app.library.record_play(&current); // current can be present in history; don't duplicate it.
    app.ai.available = true;
    app.autoplay_streaming = true;

    // The fetched pool flows through the local engine; a diverse shortlist goes to the DJ Gem.
    let cmds = app.update(StreamingMsg::Results {
        seed_video_id: "id0".to_owned(),
        candidates: vec![
            (
                Song::remote("cand1", "Track One", "band one", "3:00"),
                CandidateSource::WatchPlaylist,
            ),
            (
                Song::remote("cand2", "Track Two", "band two", "3:10"),
                CandidateSource::YtdlpStreaming,
            ),
            (
                Song::remote("cand3", "Track Three", "band three", "3:20"),
                CandidateSource::WatchPlaylist,
            ),
        ],
    });

    let (seed_id, prompt) = ai_rerank(&cmds).expect("a DJ Gem rerank command");
    assert_eq!(seed_id, "id0");
    // Compact protocol header + candidate pack.
    assert!(prompt.contains("TASK|streaming_next"));
    assert!(prompt.contains("CANDS"));
    // Recent session context (current + the two previous tracks).
    assert!(prompt.contains("- Current: t0 — a"));
    assert!(prompt.contains("- Previous 1: previous one — artist a"));
    assert!(prompt.contains("- Previous 2: previous two — artist b"));
    // Candidates appear by title under opaque cids; the raw video ids stay hidden so the
    // model can't read rank off them.
    assert!(prompt.contains("Track One"));
    assert!(prompt.contains("Track Two"));
    assert!(
        !prompt.contains("cand1"),
        "raw video ids must not leak into the pack"
    );
    assert!(app.ai.thinking, "the rerank is in flight");
    assert!(
        app.streaming.pending_rerank.is_some(),
        "shortlist + local pick stashed for validation"
    );
    assert!(!app.streaming.pending, "the pool fetch is done");
}

#[test]
fn smart_gate_skips_the_ai_call_and_enqueues_the_local_pick() {
    let _guard = crate::i18n::lock_for_test();
    let mut app = app_playing(1, 0); // current id0, remaining 0 → a refill is due
    app.ai.available = true;
    app.autoplay_streaming = true;
    // smart_gate is on by default; a negative ambiguity gap forces the score-gap branch to read
    // as confident, so this test isolates the gated local path.
    app.config.streaming.ai.smart_gate = true;
    app.config.streaming.ai.ambiguity_gap = -1.0;

    let before = app.queue.len();
    let src = CandidateSource::YtdlpStreaming;
    let cmds = app.update(StreamingMsg::Results {
        seed_video_id: "id0".to_owned(),
        candidates: vec![
            (Song::remote("cand1", "Track One", "band one", "3:00"), src),
            (Song::remote("cand2", "Track Two", "band two", "3:10"), src),
            (
                Song::remote("cand3", "Track Three", "band three", "3:20"),
                src,
            ),
        ],
    });

    assert!(
        ai_rerank(&cmds).is_none(),
        "gated: no DJ Gem rerank command spent"
    );
    assert!(
        !app.ai.thinking,
        "gated path never marks the assistant as thinking"
    );
    assert!(
        app.streaming.pending_rerank.is_none(),
        "gated path stashes nothing to validate"
    );
    assert!(
        app.queue.len() > before,
        "gated refill enqueues the local pick directly"
    );
}

#[test]
fn ai_result_cache_replays_an_identical_refill_without_a_second_call() {
    let _guard = crate::i18n::lock_for_test();
    let mut app = app_playing(1, 0); // current id0
    app.ai.available = true;
    app.autoplay_streaming = true;
    // Force the call through the gate so this exercises the cache, not the smart gate.
    app.config.streaming.ai.ambiguity_gap = 1.0;

    let src = CandidateSource::YtdlpStreaming;
    let candidates = vec![
        (Song::remote("cand1", "Track One", "band one", "3:00"), src),
        (Song::remote("cand2", "Track Two", "band two", "3:10"), src),
        (
            Song::remote("cand3", "Track Three", "band three", "3:20"),
            src,
        ),
    ];

    // First refill misses the cache → a DJ Gem call goes out, the rerank is stashed, and (on the DJ Gem
    // path) the queue is left untouched, so the next refill recomputes the *same* cache key.
    let cmds = app.update(StreamingMsg::Results {
        seed_video_id: "id0".to_owned(),
        candidates: candidates.clone(),
    });
    assert!(ai_rerank(&cmds).is_some(), "first refill spends a call");
    let pending = app
        .streaming
        .pending_rerank
        .as_ref()
        .expect("rerank stashed");
    let key = pending.cache_key;
    let cached_id = pending.cid_map[0].video_id.clone(); // a real shortlist track

    // Seed the cache as if that rerank had resolved to `cached_id`, then clear the in-flight flags
    // (queue/history untouched → the next identical refill keys to the same entry).
    app.ai_cache_store(key, vec![cached_id.clone()]);
    app.streaming.pending_rerank = None;
    app.ai.thinking = false;
    app.streaming.pending = false;
    app.streaming.last_extend = None;

    // Second identical refill hits the cache → no call; the cached ordering is enqueued directly.
    let cmds = app.update(StreamingMsg::Results {
        seed_video_id: "id0".to_owned(),
        candidates,
    });
    assert!(
        ai_rerank(&cmds).is_none(),
        "cache hit: no second DJ Gem call"
    );
    assert!(
        !app.ai.thinking,
        "cache hit never marks the assistant as thinking"
    );
    assert!(
        app.streaming.pending_rerank.is_none(),
        "cache hit stashes nothing to validate"
    );
    assert!(
        app.queue.contains_video_id(&cached_id),
        "cached ordering enqueued"
    );
}

#[test]
fn ai_set_station_profile_applies_mode_and_avoids_artists() {
    let _guard = crate::i18n::lock_for_test();
    let mut app = app_playing(1, 0); // current id0

    let cmds = app.update(AiMsg::SetStationProfile {
        query: "rainy day".to_owned(),
        explore: Some("tight".to_owned()),
        avoid_artists: vec!["Nickelback".to_owned()],
    });

    // The explore level drives the live engine mode, and the profile is stashed for persistence.
    assert_eq!(
        app.config.streaming.mode,
        crate::streaming::StreamingMode::Focused
    );
    assert_eq!(
        app.station.active.as_ref().expect("station stashed").query,
        "rainy day"
    );
    // Both the station and the (now-mode-changed) config are persisted.
    assert!(
        cmds.iter()
            .any(|c| matches!(c, Cmd::Persist(PersistCmd::StationProfile)))
    );
    assert!(
        cmds.iter()
            .any(|c| matches!(c, Cmd::Persist(PersistCmd::Config(_))))
    );

    // The avoided artist flows into the station state every refill reads.
    let st = app.build_station_state("id0");
    let want = crate::signals::normalize_artist("Nickelback");
    assert!(
        st.banned_artist_keys.contains(&want),
        "avoided artist is banned in refills"
    );
}

#[test]
fn a_plain_start_streaming_without_hints_leaves_no_station() {
    // The reducer only stamps a profile when the tool passes shaping hints; this asserts the
    // engine default holds when none are given (the tool simply omits the AiSetStationProfile msg).
    let app = app_playing(1, 0);
    assert!(app.station.active.is_none());
    assert!(app.build_station_state("id0").banned_artist_keys.is_empty());
}

#[test]
fn station_patch_folds_feedback_into_avoid_list_and_clears_inflight() {
    let _guard = crate::i18n::lock_for_test();
    let mut app = app_playing(1, 0);
    app.station.active = Some(crate::station::StationProfile::from_intent(
        "late night",
        Some("wide"),
        &[],
    ));
    app.streaming.feedback_in_flight = true;

    let cmds = app.update(AiMsg::StationPatch {
        down_artists: vec!["Nickelback".to_owned()],
        boost_artists: vec![],
    });

    // The in-flight guard always clears so the next streak can fire again.
    assert!(
        !app.streaming.feedback_in_flight,
        "in-flight guard cleared on patch"
    );
    // The down-voted artist is now avoided in every refill, and the change is persisted.
    let want = crate::signals::normalize_artist("Nickelback");
    assert!(
        app.station
            .active
            .as_ref()
            .unwrap()
            .avoid_artist_keys
            .contains(&want)
    );
    assert!(
        cmds.iter()
            .any(|c| matches!(c, Cmd::Persist(PersistCmd::StationProfile)))
    );
}

#[test]
fn empty_station_patch_clears_inflight_without_persisting() {
    let _guard = crate::i18n::lock_for_test();
    let mut app = app_playing(1, 0);
    app.station.active = Some(crate::station::StationProfile::from_intent("q", None, &[]));
    app.streaming.feedback_in_flight = true;

    // An empty patch (the off-path summary failed or found nothing) still clears the guard, but a
    // no-op change must not trigger a pointless save.
    let cmds = app.update(AiMsg::StationPatch {
        down_artists: vec![],
        boost_artists: vec![],
    });
    assert!(!app.streaming.feedback_in_flight);
    assert!(
        !cmds
            .iter()
            .any(|c| matches!(c, Cmd::Persist(PersistCmd::StationProfile))),
        "no save on a no-op patch"
    );
}

#[test]
fn feedback_summary_fires_once_per_skip_streak_when_gated_open() {
    let _guard = crate::i18n::lock_for_test();
    let mut app = app_playing(1, 0);
    app.ai.available = true;
    app.station.active = Some(crate::station::StationProfile::from_intent(
        "drive",
        Some("balanced"),
        &[],
    ));
    // A trailing skip streak at the trigger threshold (FEEDBACK_STREAK = 3).
    for _ in 0..3 {
        app.record_session_event("some artist", Outcome::QuickSkip, 0.05);
    }

    // First call past the gate: dispatches a summary and arms the in-flight guard.
    let cmd = app.maybe_summarize_feedback();
    assert!(
        matches!(cmd, Some(Cmd::SummarizeFeedback { .. })),
        "streak + active station → summary"
    );
    assert!(app.streaming.feedback_in_flight);
    // A second call while one is in flight is a no-op (single-flight).
    assert!(
        app.maybe_summarize_feedback().is_none(),
        "in-flight guard suppresses duplicates"
    );
}

#[test]
fn feedback_summary_is_skipped_without_an_active_station() {
    let _guard = crate::i18n::lock_for_test();
    let mut app = app_playing(1, 0);
    app.ai.available = true;
    for _ in 0..3 {
        app.record_session_event("x", Outcome::Skip, 0.1);
    }
    // No station to refine → nothing to learn, so no call (and no guard armed).
    assert!(app.maybe_summarize_feedback().is_none());
    assert!(!app.streaming.feedback_in_flight);
}

#[test]
fn streaming_ai_picks_enqueue_validated_ids_and_top_up_from_local() {
    let mut app = app_playing(2, 0); // queue id0 (current), id1
    app.ai.available = true;
    app.autoplay_streaming = true;
    app.ai.thinking = true;
    app.streaming.pending_rerank = Some(PendingRerank {
        seed_video_id: "id0".to_owned(),
        mode: crate::streaming::StreamingMode::Balanced,
        shortlist: vec![
            Song::remote("s1", "S1", "a", "3:00"),
            Song::remote("s2", "S2", "b", "3:00"),
        ],
        local_pick: vec![
            Song::remote("s2", "S2", "b", "3:00"),
            Song::remote("s1", "S1", "a", "3:00"),
        ],
        cid_map: vec![
            crate::streaming::PackedCand {
                cid: "c1".to_owned(),
                video_id: "s1".to_owned(),
            },
            crate::streaming::PackedCand {
                cid: "c2".to_owned(),
                video_id: "s2".to_owned(),
            },
        ],
        cache_key: 0,
    });

    // DJ Gem picks one valid cid + one hallucinated cid (dropped); the gap tops up from local.
    app.update(StreamingMsg::AiPicks {
        seed_video_id: "id0".to_owned(),
        picks: vec![
            AiPick {
                cid: "c1".to_owned(),
                role: Some("core".to_owned()),
                reasons: vec!["u".to_owned()],
            },
            AiPick {
                cid: "HALLUCINATED".to_owned(),
                role: None,
                reasons: vec![],
            },
        ],
        conf: Some(0.8),
    });

    assert!(!app.ai.thinking, "rerank finished");
    assert!(app.streaming.pending_rerank.is_none(), "pending consumed");
    assert!(
        app.queue.contains_video_id("s1"),
        "valid DJ Gem id enqueued"
    );
    assert!(
        app.queue.contains_video_id("s2"),
        "topped up from local pick"
    );
    assert!(
        !app.queue.contains_video_id("HALLUCINATED"),
        "hallucinated id dropped"
    );
}

#[test]
fn streaming_ai_picks_for_a_stale_seed_are_ignored() {
    let mut app = app_playing(2, 0);
    app.ai.available = true;
    app.autoplay_streaming = true;
    app.ai.thinking = true;
    app.streaming.pending_rerank = Some(PendingRerank {
        seed_video_id: "current-seed".to_owned(),
        mode: crate::streaming::StreamingMode::Balanced,
        shortlist: vec![Song::remote("s1", "S1", "a", "3:00")],
        local_pick: vec![Song::remote("s1", "S1", "a", "3:00")],
        cid_map: vec![crate::streaming::PackedCand {
            cid: "c1".to_owned(),
            video_id: "s1".to_owned(),
        }],
        cache_key: 0,
    });

    // A result for a different (older) seed must not consume the in-flight rerank.
    app.update(StreamingMsg::AiPicks {
        seed_video_id: "old-seed".to_owned(),
        picks: vec![AiPick {
            cid: "c1".to_owned(),
            role: None,
            reasons: vec![],
        }],
        conf: None,
    });
    assert!(
        app.streaming.pending_rerank.is_some(),
        "stale result leaves the current rerank intact"
    );
    assert!(!app.queue.contains_video_id("s1"));
}

#[test]
fn why_ai_overlay_explains_the_last_ai_rerank() {
    let _guard = crate::i18n::lock_for_test();
    let mut app = app_playing(2, 0); // queue id0 (current), id1
    app.ai.available = true;
    app.autoplay_streaming = true;
    app.ai.thinking = true;
    app.streaming.pending_rerank = Some(PendingRerank {
        seed_video_id: "id0".to_owned(),
        mode: crate::streaming::StreamingMode::Balanced,
        shortlist: vec![
            Song::remote("s1", "First Song", "Artist One", "3:00"),
            Song::remote("s2", "Second Song", "Artist Two", "3:00"),
        ],
        local_pick: vec![Song::remote("s2", "Second Song", "Artist Two", "3:00")],
        cid_map: vec![
            crate::streaming::PackedCand {
                cid: "c1".to_owned(),
                video_id: "s1".to_owned(),
            },
            crate::streaming::PackedCand {
                cid: "c2".to_owned(),
                video_id: "s2".to_owned(),
            },
        ],
        cache_key: 0,
    });

    app.update(StreamingMsg::AiPicks {
        seed_video_id: "id0".to_owned(),
        picks: vec![
            AiPick {
                cid: "c1".to_owned(),
                role: Some("bridge".to_owned()),
                reasons: vec!["tr".to_owned(), "u".to_owned()],
            },
            AiPick {
                cid: "c2".to_owned(),
                role: Some("core".to_owned()),
                reasons: vec!["co".to_owned()],
            },
        ],
        conf: Some(0.75),
    });

    // The explanation is stashed, with cids resolved to real tracks in the model's order.
    let explain = app
        .streaming
        .last_explain
        .as_ref()
        .expect("explanation stashed for the overlay");
    assert_eq!(explain.conf, Some(0.75));
    assert_eq!(explain.picks.len(), 2);
    assert_eq!(explain.picks[0].title, "First Song");
    assert_eq!(explain.picks[0].artist, "Artist One");
    assert_eq!(explain.picks[0].role.as_deref(), Some("bridge"));
    assert_eq!(explain.picks[0].reasons, vec!["tr", "u"]);
    assert_eq!(explain.picks[1].title, "Second Song");

    // `w` opens the overlay; `w` again dismisses it.
    assert!(!app.overlays.why_ai_visible);
    app.apply_radio_mode_confirm(RadioModeConfirm::Enter);
    assert!(app.radio_dedicated_mode);
    app.update(Msg::Key(key(KeyCode::Char('w'))));
    assert!(
        app.overlays.why_ai_visible,
        "w opens the Why-DJ Gem overlay in Radio mode"
    );
    app.update(Msg::Key(key(KeyCode::Char('w'))));
    assert!(!app.overlays.why_ai_visible, "w again dismisses it");
}

#[test]
fn why_ai_without_a_rerank_shows_a_note_not_an_overlay() {
    let _guard = crate::i18n::lock_for_test();
    let mut app = app_playing(2, 0);
    app.status.text.clear();
    assert!(app.streaming.last_explain.is_none());

    app.update(Msg::Key(key(KeyCode::Char('w'))));
    assert!(
        !app.overlays.why_ai_visible,
        "no overlay opens without a prior DJ Gem rerank"
    );
    assert!(
        !app.status.text.is_empty(),
        "a transient note is shown instead"
    );
}

#[test]
fn why_ai_overlay_renders_the_resolved_picks() {
    let _guard = crate::i18n::lock_for_test();
    let mut app = app_playing(2, 0);
    app.streaming.last_explain = Some(StreamingAiExplain {
        conf: Some(0.82),
        picks: vec![
            ExplainPick {
                title: "Bridge Track".to_owned(),
                artist: "Some Artist".to_owned(),
                role: Some("bridge".to_owned()),
                reasons: vec!["tr".to_owned(), "u".to_owned()],
            },
            ExplainPick {
                title: "Core Track".to_owned(),
                artist: "Another Artist".to_owned(),
                role: Some("core".to_owned()),
                reasons: vec![],
            },
        ],
    });
    app.overlays.why_ai_visible = true;

    let backend = TestBackend::new(80, 24);
    let mut terminal = Terminal::new(backend).unwrap();
    terminal.draw(|f| crate::ui::render(f, &app)).unwrap(); // must not panic
    let buf = terminal.backend().buffer().clone();
    let text: String = buf
        .content()
        .iter()
        .map(|c| c.symbol().to_owned())
        .collect();
    assert!(
        text.contains("Bridge Track"),
        "overlay shows the first resolved track"
    );
    assert!(
        text.contains("Core Track"),
        "overlay shows the second resolved track"
    );
}

#[test]
fn autoplay_uses_streaming_fallback_without_ai_key() {
    let mut app = app_playing(2, 0); // remaining = 1 (<= threshold)
    app.autoplay_streaming = true;

    let cmds = app.update(Msg::Key(key(KeyCode::Char('.'))));
    assert!(
        ask_ai(&cmds).is_none(),
        "no Gemini request without an API key"
    );
    let (seed, seed_video_id, exclude_ids) =
        streaming_fallback(&cmds).expect("a fallback streaming command");
    assert_eq!(seed_video_id, "id1");
    assert!(seed.contains("t1"));
    assert!(exclude_ids.iter().any(|id| id == "id0"));
    assert!(exclude_ids.iter().any(|id| id == "id1"));
    assert!(app.streaming.pending);

    let cmds = app.maybe_autoplay_extend();
    assert!(
        streaming_fallback(&cmds).is_none(),
        "pending fallback blocks duplicate requests"
    );
}

#[test]
fn streaming_results_run_through_local_engine_and_clear_pending() {
    let _guard = crate::i18n::lock_for_test();
    fastrand::seed(7);
    let mut app = app_playing(2, 0);
    app.autoplay_streaming = true;
    app.streaming.pending = true;

    // The local engine excludes the seed (id0) and the already-queued track (id1), dedups
    // the repeated id2, and ranks the rest. Distinct artists + normal durations keep the
    // two survivors out of the artist-cooldown / duration hard filters, so both enqueue.
    let src = CandidateSource::YtdlpStreaming;
    app.update(StreamingMsg::Results {
        seed_video_id: "id0".to_owned(),
        candidates: vec![
            (Song::remote("id0", "current", "a", "3:00"), src), // == seed, dropped
            (Song::remote("id2", "New Song", "c", "3:00"), src), // kept
            (Song::remote("id2", "New Song", "c", "3:00"), src), // canonical duplicate, deduped
            (Song::remote("id1", "queued", "b", "3:00"), src),  // already queued, dropped
            (Song::remote("id3", "Another", "d", "3:00"), src), // kept
        ],
    });

    assert!(!app.streaming.pending, "results clear the in-flight guard");
    assert_eq!(
        app.queue.len(),
        4,
        "two new tracks added to the queue of two"
    );
    assert!(app.queue.contains_video_id("id2"));
    assert!(app.queue.contains_video_id("id3"));
    assert!(app.status.text.contains("Queued 2"));
}

#[test]
fn streaming_error_uses_circuit_breaker() {
    let _guard = crate::i18n::lock_for_test();
    let mut app = app_playing(1, 0);
    app.autoplay_streaming = true;

    for _ in 0..AUTOPLAY_MAX_FAILURES {
        app.streaming.pending = true;
        app.update(StreamingMsg::Error {
            seed_video_id: "id0".to_owned(),
            error: "yt-dlp failed".to_owned(),
        });
    }

    assert!(!app.streaming.pending);
    assert!(!app.autoplay_streaming);
    assert!(app.status.text.contains("Autoplay stopped"));
}

#[test]
fn ai_create_and_play_playlist_roundtrip() {
    let mut app = App::new(100);
    let cmds = app.update(AiMsg::CreatePlaylist("Focus".to_owned()));
    assert!(
        cmds.iter()
            .any(|c| matches!(c, Cmd::Persist(PersistCmd::Playlists)))
    );
    let cmds = app.update(AiMsg::AddToPlaylist {
        playlist: "Focus".to_owned(),
        songs: songs(2),
    });
    assert!(
        cmds.iter()
            .any(|c| matches!(c, Cmd::Persist(PersistCmd::Playlists)))
    );
    assert_eq!(app.playlists.find("Focus").unwrap().songs.len(), 2);
    let cmds = app.update(AiMsg::PlayPlaylist("Focus".to_owned()));
    assert_eq!(current(&app), "id0");
    assert_loads_video(&cmds, "id0");
}

// --- M5: library (favorites + history) ----------------------------------
