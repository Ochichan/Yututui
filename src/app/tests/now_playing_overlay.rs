use super::*;

#[test]
fn identify_key_needs_a_radio_stream() {
    // Music mode: an info note, no overlay.
    let mut app = app_playing(1, 0);
    app.ai.available = true;
    let cmds = app.update(Msg::Key(key(KeyCode::Char('i'))));
    assert!(cmds.is_empty());
    assert!(app.overlays.now_playing_overlay.is_none());
    assert_eq!(app.status.kind, StatusKind::Info);
}

#[test]
fn identify_opens_from_icy_metadata_without_dj_gem_or_an_api_call() {
    // DJ Gem OFF: the card still opens and shows the stream's song, synchronously.
    let mut app = radio_with_title("Artist - Track");
    assert!(!app.ai.available);
    let cmds = app.update(Msg::Key(key(KeyCode::Char('i'))));
    assert!(
        cmds.is_empty(),
        "populated from ICY metadata — never an API call"
    );
    assert!(matches!(
        app.overlays.now_playing_overlay.as_ref().map(|o| &o.state),
        Some(NowPlayingOverlayState::Playing { artist, title })
            if artist.as_deref() == Some("Artist") && title == "Track"
    ));
    // The `i` key still toggles the card closed.
    let cmds = app.update(Msg::Key(key(KeyCode::Char('i'))));
    assert!(cmds.is_empty());
    assert!(app.overlays.now_playing_overlay.is_none());
}

#[test]
fn identify_without_metadata_shows_no_metadata() {
    let mut app = radio_playing("groove");
    let cmds = app.update(Msg::Key(key(KeyCode::Char('i'))));
    assert!(cmds.is_empty());
    let overlay = app
        .overlays
        .now_playing_overlay
        .as_ref()
        .expect("overlay opens");
    assert_eq!(overlay.state, NowPlayingOverlayState::NoMetadata);
}

#[test]
fn identify_flags_obvious_station_content() {
    let mut app = radio_with_title("Werbung");
    app.update(Msg::Key(key(KeyCode::Char('i'))));
    assert_eq!(
        app.overlays.now_playing_overlay.as_ref().map(|o| &o.state),
        Some(&NowPlayingOverlayState::StationContent)
    );
    // Station content is neither favoritable nor askable.
    assert!(!app.now_playing_can_favorite());
    app.ai.available = true;
    assert!(!app.now_playing_can_ask());
}

#[test]
fn open_card_repopulates_live_on_title_change() {
    let mut app = radio_card("Artist - Track");
    assert!(matches!(
        app.overlays.now_playing_overlay.as_ref().map(|o| &o.state),
        Some(NowPlayingOverlayState::Playing { title, .. }) if title == "Track"
    ));
    // The song flips under the open card → it re-populates from the fresh ICY title.
    app.update(PlayerMsg::Metadata(serde_json::json!({
        "icy-title": "Other - Song"
    })));
    assert!(matches!(
        app.overlays.now_playing_overlay.as_ref().map(|o| &o.state),
        Some(NowPlayingOverlayState::Playing { title, .. }) if title == "Song"
    ));
}

#[test]
fn early_open_before_first_tick_fills_in_when_metadata_arrives() {
    // Opened right after tuning in, before mpv surfaces the first ICY tick.
    let mut app = radio_playing("groove");
    app.update(Msg::Key(key(KeyCode::Char('i'))));
    assert_eq!(
        app.overlays.now_playing_overlay.as_ref().map(|o| &o.state),
        Some(&NowPlayingOverlayState::NoMetadata)
    );
    // The first metadata tick lands → the open card fills in on its own.
    app.update(PlayerMsg::Metadata(serde_json::json!({
        "icy-title": "Artist - Track"
    })));
    assert!(matches!(
        app.overlays.now_playing_overlay.as_ref().map(|o| &o.state),
        Some(NowPlayingOverlayState::Playing { title, .. }) if title == "Track"
    ));
}

#[test]
fn overlay_favorite_resolves_then_adds_to_music_favorites_once() {
    // Favoriting is AI-free: DJ Gem stays OFF here.
    let mut app = radio_card("Artist - Track");

    let cmds = app.update(Msg::Key(key(KeyCode::Char('f'))));
    let (seq, query) = resolve_track_cmd(&cmds).expect("one resolve");
    assert_eq!(query, "Artist Track");
    assert!(
        app.overlays
            .now_playing_overlay
            .as_ref()
            .is_some_and(|o| o.resolving)
    );

    // A second press while resolving is a no-op (debounced).
    let cmds = app.update(Msg::Key(key(KeyCode::Char('f'))));
    assert!(resolve_track_cmd(&cmds).is_none());

    let cmds = app.update(Msg::TrackResolved {
        seq,
        result: Ok(vec![
            Song::remote("vid1", "Track", "Artist", "3:00"),
            Song::remote("vid2", "Track (Live)", "Artist", "4:00"),
        ]),
    });
    assert!(
        cmds.iter()
            .any(|c| matches!(c, Cmd::Persist(PersistCmd::Library)))
    );
    assert!(
        app.library.is_favorite("vid1"),
        "best match lands in favorites"
    );
    assert!(
        app.library.favorites.iter().any(|s| s.video_id == "vid1"),
        "a Youtube-source track routes to the MUSIC favorites"
    );
    assert!(
        app.library
            .radio_favorites
            .iter()
            .all(|s| s.video_id != "vid1")
    );
    assert_eq!(app.status.kind, StatusKind::Info);
    assert!(app.status.text.contains("Track"));

    // Repeat press: the resolved song rides the overlay — no re-search, and the
    // toggle-precheck must NOT remove the favorite.
    let cmds = app.update(Msg::Key(key(KeyCode::Char('f'))));
    assert!(resolve_track_cmd(&cmds).is_none());
    assert!(app.library.is_favorite("vid1"));

    // Re-open the card for the same title: the cache carries the resolution too.
    app.update(Msg::Key(key(KeyCode::Char('i'))));
    app.update(Msg::Key(key(KeyCode::Char('i'))));
    let cmds = app.update(Msg::Key(key(KeyCode::Char('f'))));
    assert!(
        resolve_track_cmd(&cmds).is_none(),
        "cache-attached resolution reused"
    );
    assert!(app.library.is_favorite("vid1"));
}

#[test]
fn overlay_favorite_resolve_failures_keep_the_overlay_and_write_nothing() {
    let mut app = radio_card("Artist - Track");
    let cmds = app.update(Msg::Key(key(KeyCode::Char('f'))));
    let (seq, _) = resolve_track_cmd(&cmds).expect("resolve");

    // Empty result: an error toast, the overlay stays, nothing is written.
    let cmds = app.update(Msg::TrackResolved {
        seq,
        result: Ok(Vec::new()),
    });
    assert!(cmds.is_empty());
    assert!(app.overlays.now_playing_overlay.is_some());
    assert_eq!(app.status.kind, StatusKind::Error);
    assert!(app.library.favorites.is_empty());

    // Stale seq: dropped entirely.
    let cmds = app.update(Msg::Key(key(KeyCode::Char('f'))));
    let (seq2, _) = resolve_track_cmd(&cmds).expect("second resolve");
    app.update(Msg::TrackResolved {
        seq: seq2.wrapping_sub(1),
        result: Ok(vec![Song::remote("vid9", "Wrong", "X", "1:00")]),
    });
    assert!(
        app.library.favorites.is_empty(),
        "stale reply must not write"
    );
    assert!(
        app.overlays
            .now_playing_overlay
            .as_ref()
            .is_some_and(|o| o.resolving)
    );
}

#[test]
fn overlay_ask_ai_is_gated_on_dj_gem_and_seeds_a_rich_block() {
    // DJ Gem OFF: the ask action is hidden and pressing its key does nothing.
    let mut app = radio_card("Artist - Track");
    assert!(!app.now_playing_can_ask());
    let cmds = app.update(Msg::Key(key(KeyCode::Char('g'))));
    assert!(ask_ai_prompt(&cmds).is_none());
    assert!(
        app.overlays.now_playing_overlay.is_some(),
        "the card stays put"
    );

    // DJ Gem ON: the ask action hands off with a labeled, enriched block.
    app.ai.available = true;
    assert!(app.now_playing_can_ask());
    let cmds = app.update(Msg::Key(key(KeyCode::Char('g'))));
    let prompt = ask_ai_prompt(&cmds).expect("one AskAi");
    assert_eq!(app.mode, Mode::Ai);
    assert!(
        app.overlays.now_playing_overlay.is_none(),
        "the card closes on handoff"
    );
    assert!(app.ai.thinking);
    // The model gets the labeled block: station, untrusted raw title, local split, a
    // standing "may be mislabeled" caution, and the rich structured request.
    assert!(prompt.starts_with("<now_playing>"));
    assert!(prompt.contains("station: Station groove — KR / MP3"));
    assert!(prompt.contains("raw_title (untrusted, as sent by the radio stream): Artist - Track"));
    assert!(prompt.contains("parsed (best-effort local split): artist=Artist · title=Track"));
    assert!(prompt.contains("may be mislabeled"));
    assert!(prompt.contains("similar tracks"));
    assert!(prompt.contains("</now_playing>"));
    // The transcript shows the compact line, not the block.
    let last = app.ai.messages.last().expect("transcript line");
    assert_eq!(last.role, AiRole::User);
    assert!(!last.text.contains("<now_playing>"));
    assert!(last.text.contains("Track — Artist"));
}

#[test]
fn overlay_ask_ai_respects_the_thinking_guard() {
    // DJ Gem connected but busy → an info note; the card stays put and nothing is sent.
    let mut app = radio_card("Artist - Track");
    app.ai.available = true;
    app.ai.thinking = true;
    let cmds = app.update(Msg::Key(key(KeyCode::Char('g'))));
    assert!(ask_ai_prompt(&cmds).is_none());
    assert!(app.overlays.now_playing_overlay.is_some());
    assert_eq!(app.status.kind, StatusKind::Info);
}

#[test]
fn identify_overlay_swallows_player_keys_and_esc_closes() {
    let mut app = radio_with_title("Artist - Track");
    app.update(Msg::Key(key(KeyCode::Char('i'))));
    assert!(app.overlays.now_playing_overlay.is_some());

    // `n` (next track) must not leak through to the player underneath.
    let cmds = app.update(Msg::Key(key(KeyCode::Char('.'))));
    assert!(cmds.is_empty());
    assert_eq!(current(&app), "rad:groove");
    assert!(app.overlays.now_playing_overlay.is_some());

    app.update(Msg::Key(key(KeyCode::Esc)));
    assert!(app.overlays.now_playing_overlay.is_none());
}
