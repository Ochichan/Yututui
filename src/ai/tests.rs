use super::*;

fn ctx() -> AiContext {
    AiContext {
        current_track: Some("Song — Artist".to_owned()),
        current_radio_station: None,
        current_radio_now_playing: None,
        queue_upcoming: vec!["Next — Artist".to_owned()],
        queue_len: 3,
        queue_remaining: 2,
        recent_history: vec!["Old — Artist".to_owned()],
        favorites: vec!["Fave — Artist".to_owned()],
        playlists: vec![PlaylistInfo {
            id: "mix".to_owned(),
            name: "Mix".to_owned(),
            count: 4,
        }],
        search: crate::search_source::SearchConfig::default(),
        authenticated: true,
        autoplay_streaming: false,
        repeat_on: false,
    }
}

fn romanize_item(key: &str, title: &str, artist: &str) -> RomanizeItem {
    RomanizeItem {
        key: key.to_owned(),
        title: title.to_owned(),
        artist: artist.to_owned(),
    }
}

fn test_actor() -> AiActor {
    AiActor {
        client: GeminiClient::new("test-key").unwrap(),
        model: GeminiModel::FlashLite,
        emit: Arc::new(|_| {}),
        call_times: VecDeque::new(),
        history: Vec::new(),
    }
}

#[test]
fn ai_handle_sends_each_command_without_mutating_payloads() {
    let (tx, mut rx) = mpsc::channel(1);
    let (model_updates, mut model_rx) = model_control::channel(GeminiModel::FlashLite);
    let handle = AiHandle { tx, model_updates };

    assert!(
        handle
            .ask("play something".to_owned(), Box::new(ctx()))
            .is_ok()
    );
    match rx.try_recv().unwrap() {
        AiCmd::Ask { prompt, context } => {
            assert_eq!(prompt, "play something");
            assert_eq!(context.queue_len, 3);
            assert_eq!(context.favorites, vec!["Fave — Artist"]);
        }
        _ => panic!("expected Ask command"),
    }

    assert!(
        handle
            .rerank("seed-video".to_owned(), "CANDS...".to_owned())
            .is_ok()
    );
    match rx.try_recv().unwrap() {
        AiCmd::Rerank {
            seed_video_id,
            prompt,
        } => {
            assert_eq!(seed_video_id, "seed-video");
            assert_eq!(prompt, "CANDS...");
        }
        _ => panic!("expected Rerank command"),
    }

    assert!(
        handle
            .summarize_feedback("SESSION|played|artist".to_owned())
            .is_ok()
    );
    match rx.try_recv().unwrap() {
        AiCmd::SummarizeFeedback { digest } => {
            assert_eq!(digest, "SESSION|played|artist");
        }
        _ => panic!("expected SummarizeFeedback command"),
    }

    let expected_items = vec![romanize_item("k0", "좋은 날", "아이유")];
    assert!(handle.romanize(42, expected_items.clone()).is_ok());
    match rx.try_recv().unwrap() {
        AiCmd::Romanize { request_id, items } => {
            assert_eq!(request_id, 42);
            assert_eq!(items, expected_items);
        }
        _ => panic!("expected Romanize command"),
    }

    assert!(handle.set_model(GeminiModel::Latest).is_ok());
    assert_eq!(model_rx.take_latest(), GeminiModel::Latest);
}

#[test]
fn spawn_rejects_keys_that_cannot_be_sent_as_headers() {
    assert!(
        spawn("bad\r\nkey", GeminiModel::FlashLite, |_| {}).is_none(),
        "invalid header bytes must fail before an actor is spawned"
    );
}

#[test]
fn thinking_guard_clears_the_spinner_on_drop() {
    let seen = Arc::new(std::sync::Mutex::new(Vec::new()));
    let captured = Arc::clone(&seen);
    let emit: EventSink = Arc::new(move |event| {
        if let AiEvent::Thinking(value) = event {
            captured.lock().unwrap().push(value);
        }
    });

    {
        let _guard = ThinkingGuard(emit);
    }

    assert_eq!(*seen.lock().unwrap(), vec![false]);
}

#[tokio::test]
async fn throttle_prunes_expired_calls_and_records_the_current_one() {
    let mut actor = test_actor();
    actor
        .call_times
        .push_back(Instant::now() - RATE_WINDOW - Duration::from_secs(1));
    actor
        .call_times
        .push_back(Instant::now() - Duration::from_millis(5));

    actor.throttle().await;

    assert_eq!(actor.call_times.len(), 2);
    assert!(
        actor
            .call_times
            .iter()
            .all(|time| time.elapsed() < RATE_WINDOW),
        "expired calls are not allowed to count against the rolling limit"
    );
}

#[tokio::test]
async fn actor_run_applies_set_model_and_exits_when_channel_closes() {
    let (tx, rx) = mpsc::channel(1);
    let (model_updates, model_rx) = model_control::channel(GeminiModel::Flash);
    let mut actor = test_actor();
    actor.model = GeminiModel::Flash;

    assert!(model_updates.send(GeminiModel::Latest).is_ok());
    drop(tx);

    actor.run(rx, model_rx).await;
    assert_eq!(model_updates.applied_generation(), 1);
}

#[test]
fn context_summary_includes_key_state() {
    let s = context_summary(&ctx());
    assert!(s.contains("Now playing: Song — Artist"));
    assert!(s.contains("2 remaining"));
    assert!(s.contains("Mix (4)"));
    assert!(s.contains("Signed in: yes"));
}

#[test]
fn context_summary_includes_radio_stream_metadata() {
    let mut ctx = ctx();
    ctx.current_track = Some("Groove Radio — US / MP3 / 128k".to_owned());
    ctx.current_radio_station = ctx.current_track.clone();
    ctx.current_radio_now_playing = Some("The Track — The Artist".to_owned());

    let s = context_summary(&ctx);

    assert!(s.contains("Current radio station: Groove Radio"));
    assert!(s.contains("Current radio stream track: The Track — The Artist"));
}

#[test]
fn context_summary_warns_when_radio_stream_metadata_is_absent() {
    let mut ctx = ctx();
    ctx.current_track = Some("Groove Radio — US / MP3 / 128k".to_owned());
    ctx.current_radio_station = ctx.current_track.clone();

    let s = context_summary(&ctx);

    assert!(s.contains("Current radio stream track: unavailable"));
}

#[test]
fn parse_rerank_picks_reads_ids_and_conf() {
    let (picks, conf) = parse_rerank_picks(r#"{"ids":["a","b","c"],"conf":0.9}"#).unwrap();
    let cids: Vec<&str> = picks.iter().map(|p| p.cid.as_str()).collect();
    assert_eq!(cids, vec!["a", "b", "c"]);
    assert_eq!(conf, Some(0.9));
    // roles/reasons absent → defaulted, not an error.
    assert!(
        picks
            .iter()
            .all(|p| p.role.is_none() && p.reasons.is_empty())
    );
}

#[test]
fn parse_rerank_picks_zips_roles_and_reasons_onto_ids() {
    let (picks, _) = parse_rerank_picks(
        r#"{"ids":["a","b"],"roles":["bridge","core"],"reasons":[["tr"],["co","u"]],"conf":0.7}"#,
    )
    .unwrap();
    assert_eq!(picks[0].role.as_deref(), Some("bridge"));
    assert_eq!(picks[0].reasons, vec!["tr"]);
    assert_eq!(picks[1].role.as_deref(), Some("core"));
    assert_eq!(picks[1].reasons, vec!["co", "u"]);
}

#[test]
fn parse_rerank_picks_tolerates_a_code_fence() {
    let (picks, _) = parse_rerank_picks("```json\n{\"ids\":[\"x\"]}\n```").unwrap();
    assert_eq!(picks[0].cid, "x");
}

#[test]
fn parse_rerank_picks_rejects_garbage_and_empty() {
    assert!(parse_rerank_picks("not json").is_none());
    assert!(
        parse_rerank_picks(r#"{"ids":[]}"#).is_none(),
        "empty ids → fall back to local"
    );
    assert!(parse_rerank_picks(r#"{"other":1}"#).is_none());
}

#[test]
fn rerank_request_is_json_only_with_thinking_off_and_no_tools() {
    let req = build_rerank_request("seed + candidates");
    assert!(req.tools.is_none(), "reranker must not expose tools");
    let v = serde_json::to_value(&req).unwrap();
    let gc = &v["generationConfig"];
    assert_eq!(gc["responseMimeType"], "application/json");
    assert_eq!(gc["thinkingConfig"]["thinkingBudget"], 0);
    assert_eq!(gc["maxOutputTokens"], RERANK_MAX_TOKENS);
    let props = &gc["responseSchema"]["properties"];
    assert!(props.get("ids").is_some());
    assert!(props.get("roles").is_some(), "schema must expose roles");
    assert!(props.get("reasons").is_some(), "schema must expose reasons");
}

#[test]
fn parse_feedback_patch_reads_both_arrays_and_trims_blanks() {
    let (down, boost) =
        parse_feedback_patch(r#"{"down_artists":["Nickelback"," "],"boost_artists":["  ABBA "]}"#)
            .unwrap();
    assert_eq!(down, vec!["Nickelback"]);
    assert_eq!(boost, vec!["ABBA"], "names are trimmed and blanks dropped");
}

#[test]
fn parse_feedback_patch_allows_a_valid_empty_object_as_a_noop() {
    // A well-formed object with no/empty arrays is a valid no-op patch, not a parse failure.
    assert_eq!(parse_feedback_patch("{}"), Some((vec![], vec![])));
    let (down, boost) = parse_feedback_patch(r#"{"down_artists":[]}"#).unwrap();
    assert!(down.is_empty() && boost.is_empty());
}

#[test]
fn parse_feedback_patch_tolerates_a_code_fence_and_rejects_garbage() {
    let (down, _) = parse_feedback_patch("```json\n{\"down_artists\":[\"X\"]}\n```").unwrap();
    assert_eq!(down, vec!["X"]);
    assert!(parse_feedback_patch("not json").is_none());
    assert!(
        parse_feedback_patch("[1,2,3]").is_none(),
        "a non-object is unusable"
    );
}

#[test]
fn feedback_request_is_json_only_with_thinking_off_and_no_tools() {
    let req = build_feedback_request("STATION|...\nSESSION|...");
    assert!(
        req.tools.is_none(),
        "feedback summary must not expose tools"
    );
    let v = serde_json::to_value(&req).unwrap();
    let gc = &v["generationConfig"];
    assert_eq!(gc["responseMimeType"], "application/json");
    assert_eq!(gc["thinkingConfig"]["thinkingBudget"], 0);
    assert_eq!(gc["maxOutputTokens"], FEEDBACK_MAX_TOKENS);
    let props = &gc["responseSchema"]["properties"];
    assert!(props.get("down_artists").is_some());
    assert!(props.get("boost_artists").is_some());
}

#[test]
fn romanize_request_is_json_only_with_index_ids_and_thinking_off() {
    let items = vec![
        romanize_item("song-a", "좋은 날", "아이유"),
        romanize_item("song-b", "アイドル", "YOASOBI"),
    ];
    let req = build_romanize_request(&items);

    assert!(req.tools.is_none(), "romanizer must not expose tools");
    let v = serde_json::to_value(&req).unwrap();
    let prompt: serde_json::Value =
        serde_json::from_str(v["contents"][0]["parts"][0]["text"].as_str().unwrap()).unwrap();
    assert_eq!(prompt["items"][0]["id"], "0");
    assert_eq!(prompt["items"][0]["title"], "좋은 날");
    assert_eq!(prompt["items"][1]["id"], "1");
    assert_eq!(prompt["items"][1]["artist"], "YOASOBI");

    let gc = &v["generationConfig"];
    assert_eq!(gc["responseMimeType"], "application/json");
    assert_eq!(gc["thinkingConfig"]["thinkingBudget"], 0);
    assert_eq!(gc["maxOutputTokens"], ROMANIZE_MAX_TOKENS);
    let schema_item = &gc["responseSchema"]["properties"]["items"]["items"];
    assert!(
        schema_item["required"]
            .as_array()
            .unwrap()
            .contains(&"id".into())
    );
    assert!(
        v["systemInstruction"]["parts"][0]["text"]
            .as_str()
            .unwrap()
            .contains("MusicLatinizer")
    );
}

#[test]
fn parse_romanized_titles_maps_ids_trims_and_clamps_confidence() {
    let items = vec![
        romanize_item("song-a", "좋은 날", "아이유"),
        romanize_item("song-b", "Plain", "Artist"),
        romanize_item("song-c", "밤편지", "아이유"),
    ];
    let parsed = parse_romanized_titles(
            "```json\n{\"items\":[\
             {\"id\":\"0\",\"title_latin\":\" Joheun Nal \",\"artist_latin\":\" IU \",\"confidence\":1.7},\
             {\"id\":\"1\",\"title_latin\":\"\",\"artist_latin\":\"\"},\
             {\"id\":\"2\",\"title_latin\":\"Bam Pyeonji\",\"artist_latin\":\"\",\"confidence\":-0.4}\
             ]}\n```",
            &items,
        )
        .unwrap();

    assert_eq!(parsed.len(), 2, "empty title+artist entries are ignored");
    assert_eq!(parsed[0].key, "song-a");
    assert_eq!(parsed[0].title, "Joheun Nal");
    assert_eq!(parsed[0].artist, "IU");
    assert_eq!(parsed[0].confidence, Some(1.0));
    assert_eq!(parsed[1].key, "song-c");
    assert_eq!(parsed[1].title, "Bam Pyeonji");
    assert_eq!(parsed[1].artist, "");
    assert_eq!(parsed[1].confidence, Some(0.0));
}

#[test]
fn parse_romanized_titles_rejects_unusable_payloads() {
    let items = vec![romanize_item("song-a", "좋은 날", "아이유")];

    assert!(parse_romanized_titles("not json", &items).is_none());
    assert!(parse_romanized_titles(r#"{"items":{}}"#, &items).is_none());
    assert!(
        parse_romanized_titles(
            r#"{"items":[{"id":"9","title_latin":"X","artist_latin":"Y"}]}"#,
            &items
        )
        .is_none(),
        "unknown ids would not map back to the original title key"
    );
}

fn turn(role: HistoryRole, text: &str) -> HistoryTurn {
    HistoryTurn {
        role,
        text: text.to_owned(),
    }
}

#[test]
fn trim_history_drops_whole_pairs_oldest_first() {
    let mut history: Vec<HistoryTurn> = (0..6)
        .flat_map(|i| {
            [
                turn(HistoryRole::User, &format!("q{i}")),
                turn(HistoryRole::Model, &format!("a{i}")),
            ]
        })
        .collect();
    trim_history(&mut history);
    assert_eq!(history.len(), HISTORY_MAX_TURNS);
    assert_eq!(history[0].text, "q1", "the oldest pair goes first");
    assert_eq!(
        history[0].role,
        HistoryRole::User,
        "trimming must never leave a leading model turn"
    );

    // The char backstop also trims pair-wise…
    let mut history = vec![
        turn(HistoryRole::User, "old"),
        turn(HistoryRole::Model, &"x".repeat(HISTORY_MAX_CHARS)),
        turn(HistoryRole::User, "new"),
        turn(HistoryRole::Model, "answer"),
    ];
    trim_history(&mut history);
    assert_eq!(history.len(), 2);
    assert_eq!(history[0].text, "new");

    // …and a single oversized pair is dropped outright, never half-trimmed.
    let mut history = vec![
        turn(HistoryRole::User, "q"),
        turn(HistoryRole::Model, &"x".repeat(HISTORY_MAX_CHARS + 1)),
    ];
    trim_history(&mut history);
    assert!(history.is_empty());
}

#[test]
fn chat_contents_leads_with_history_and_puts_context_only_on_the_current_turn() {
    let history = vec![
        turn(HistoryRole::User, "what's playing?"),
        turn(HistoryRole::Model, "아이돌 — YOASOBI."),
    ];
    let contents = chat_contents(&history, "CTX-BLOCK\nUser request: tell me more".to_owned());
    assert_eq!(contents.len(), 3);
    assert_eq!(contents[0].role.as_deref(), Some("user"));
    assert_eq!(contents[1].role.as_deref(), Some("model"));
    assert_eq!(contents[2].role.as_deref(), Some("user"));
    assert_eq!(contents[0].joined_text(), "what's playing?");
    assert_eq!(contents[1].joined_text(), "아이돌 — YOASOBI.");
    // The live context block rides only the current turn; history stays verbatim.
    assert!(contents[2].joined_text().contains("CTX-BLOCK"));
    assert!(!contents[0].joined_text().contains("CTX-BLOCK"));
    // Empty history still opens (and ends) with the required user turn.
    let contents = chat_contents(&[], "hi".to_owned());
    assert_eq!(contents.len(), 1);
    assert_eq!(contents[0].role.as_deref(), Some("user"));
}
