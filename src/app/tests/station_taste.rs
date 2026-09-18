use super::*;
use crate::streaming::TasteEdit;
use crate::util::delivery::DeliveryError;

fn streaming_app_with_duplicate_current() -> App {
    let mut app = App::new(100);
    let tracks = vec![
        Song::remote("id0", "t0", "Alpha", "0:10"),
        Song::remote("id1", "t1", "Beta", "0:10"),
        Song::remote("id0", "t0 copy", "Alpha", "0:10"),
    ];
    app.queue.set(tracks, 0);
    app.mode = Mode::Player;
    let song = app.queue.current().cloned();
    let mut cmds = app.load_song(song);
    admit_player_transition(&mut app, &mut cmds);
    app.autoplay_streaming = true;
    app.ai.available = false;
    app.config.streaming.ai.enabled = false;
    app
}

#[test]
fn ban_track_purges_every_matching_row_and_records_a_skip() {
    let mut app = streaming_app_with_duplicate_current();
    let skip_events = app.streaming.session_events.len();
    let play_count = app.signals.play_count("id0");

    let mut cmds = app.update(Msg::Key(key(KeyCode::Char('B'))));
    admit_player_transition(&mut app, &mut cmds);

    assert!(!app.queue.contains_video_id("id0"));
    assert_eq!(current(&app), "id1");
    assert_eq!(app.queue.len(), 1);
    assert_eq!(app.signals.play_count("id0"), play_count + 1);
    assert_eq!(app.streaming.session_events.len(), skip_events + 1);
    assert!(matches!(
        app.streaming
            .session_events
            .back()
            .map(|event| event.outcome),
        Some(Outcome::Skip | Outcome::QuickSkip)
    ));
    assert!(
        app.streaming
            .taste
            .rejects_song(&Song::remote("id0", "t0", "Alpha", "0:10"))
    );
    let station = app.recommendation_station_state_for_test("id1");
    assert!(station.banned_track_ids.contains("id0"));
    assert!(
        !station.banned_track_ids.contains("id1"),
        "the surviving track is not banned"
    );
}

#[test]
fn rejected_ban_leaves_queue_signals_and_taste_untouched() {
    let mut app = streaming_app_with_duplicate_current();
    let before_ids: Vec<String> = app.queue.video_ids().map(str::to_owned).collect();
    let before_rev = app.queue.rev();
    let before_taste = app.streaming.taste.clone();
    let before_signals = app.signals.play_count("id0");
    let before_events = app.streaming.session_events.len();

    let cmds = app.update(Msg::Key(key(KeyCode::Char('B'))));
    assert!(reject_player_transition(&mut app, cmds, DeliveryError::Busy).is_empty());

    let after_ids: Vec<String> = app.queue.video_ids().map(str::to_owned).collect();
    assert_eq!(after_ids, before_ids);
    assert_eq!(app.queue.rev(), before_rev);
    assert_eq!(app.streaming.taste, before_taste);
    assert_eq!(app.signals.play_count("id0"), before_signals);
    assert_eq!(app.streaming.session_events.len(), before_events);
    assert_eq!(current(&app), "id0");
}

#[test]
fn ban_artist_purges_matching_rows() {
    let mut app = streaming_app_with_duplicate_current();
    let mut cmds = app.update(Msg::Key(key(KeyCode::Char('A'))));
    admit_player_transition(&mut app, &mut cmds);

    assert_eq!(app.queue.len(), 1);
    assert_eq!(current(&app), "id1");
    assert!(
        app.streaming
            .taste
            .rejects_song(&Song::remote("id0", "t0", "Alpha", "0:10"))
    );
    assert!(
        !app.streaming
            .taste
            .rejects_song(&Song::remote("id1", "t1", "Beta", "0:10"))
    );
}

#[test]
fn station_card_opens_and_esc_closes_while_streaming() {
    let mut app = streaming_app_with_duplicate_current();
    app.update(Msg::Key(key(KeyCode::Char('e'))));
    assert!(app.overlays.station_card.is_some());
    app.update(Msg::Key(key(KeyCode::Esc)));
    assert!(app.overlays.station_card.is_none());
}

#[test]
fn station_card_e_closes_without_typing_into_the_field() {
    let mut app = streaming_app_with_duplicate_current();
    app.update(Msg::Key(key(KeyCode::Char('e'))));
    assert!(app.overlays.station_card.is_some());
    app.update(Msg::Key(key(KeyCode::Char('e'))));
    assert!(app.overlays.station_card.is_none());
}

#[test]
fn off_state_station_chords_keep_global_and_player_meanings() {
    let mut app = app_playing(3, 0);
    assert!(!app.streaming_active());
    let collapsed = app.config.control_box_collapsed();
    let preset = app.audio.preset;
    let animations = app.config.animations.master;

    app.update(Msg::Key(key(KeyCode::Char('B'))));
    assert_ne!(app.config.control_box_collapsed(), collapsed);
    assert!(app.streaming.taste.counts().is_empty());

    app.update(Msg::Key(key(KeyCode::Char('A'))));
    assert_ne!(app.config.animations.master, animations);
    assert!(app.streaming.taste.counts().is_empty());

    let mut cmds = app.update(Msg::Key(key(KeyCode::Char('e'))));
    assert!(app.overlays.station_card.is_none());
    admit_player_transition(&mut app, &mut cmds);
    assert_ne!(app.audio.preset, preset);
}

#[test]
fn station_card_enter_seeds_the_current_artist() {
    let mut app = streaming_app_with_duplicate_current();
    app.update(Msg::Key(key(KeyCode::Char('e'))));
    let cmds = app.update(Msg::Key(key(KeyCode::Enter)));
    assert_eq!(app.streaming.taste.counts().seeds, 1);
    assert!(
        cmds.iter()
            .any(|cmd| matches!(cmd, Cmd::StreamingFallback { .. })),
        "a seed edit force-refills the live station"
    );
}

#[test]
fn empty_artist_ban_does_not_write_taste() {
    let mut app = App::new(100);
    app.queue
        .set(vec![Song::remote("bare", "Untitled", "", "0:10")], 0);
    app.mode = Mode::Player;
    let song = app.queue.current().cloned();
    let mut cmds = app.load_song(song);
    admit_player_transition(&mut app, &mut cmds);
    app.autoplay_streaming = true;

    app.update(Msg::Key(key(KeyCode::Char('A'))));
    assert!(app.streaming.taste.counts().is_empty());
    assert_eq!(current(&app), "bare");
}

#[test]
fn gem_off_station_state_contains_the_session_ban() {
    let mut app = streaming_app_with_duplicate_current();
    let edit = TasteEdit::ban_track(app.queue.current().expect("current")).expect("id");
    assert_eq!(
        app.streaming.taste.apply(edit),
        crate::streaming::TasteOutcome::Applied
    );
    let station = app.recommendation_station_state_for_test("id1");
    assert!(station.banned_track_ids.contains("id0"));
    assert!(!app.ai.available);
    assert!(!app.config.streaming.ai.enabled);
}

#[test]
fn ban_last_track_still_requests_a_refill_from_the_banned_seed() {
    let mut app = App::new(100);
    app.queue
        .set(vec![Song::remote("only", "Solo", "Alpha", "0:10")], 0);
    app.mode = Mode::Player;
    let song = app.queue.current().cloned();
    let mut cmds = app.load_song(song);
    admit_player_transition(&mut app, &mut cmds);
    app.autoplay_streaming = true;
    app.ai.available = false;
    app.config.streaming.ai.enabled = false;

    let mut cmds = app.update(Msg::Key(key(KeyCode::Char('B'))));
    admit_player_transition(&mut app, &mut cmds);

    assert_eq!(app.queue.len(), 0);
    assert!(
        app.streaming
            .taste
            .rejects_song(&Song::remote("only", "Solo", "Alpha", "0:10"))
    );
    assert!(
        cmds.iter().any(|cmd| matches!(
            cmd,
            Cmd::StreamingFallback {
                seed_video_id,
                ..
            } if seed_video_id == "only"
        )),
        "empty-queue Stop still seeds a refill from the banned current track"
    );
}

#[test]
fn clicking_the_taste_chip_opens_the_station_card() {
    let mut app = streaming_app_with_duplicate_current();
    let backend = ratatui::backend::TestBackend::new(120, 24);
    let mut terminal = ratatui::Terminal::new(backend).unwrap();
    terminal
        .draw(|frame| crate::ui::render(frame, &app))
        .unwrap();
    let (col, row) = button_center(&app, MouseTarget::StationCard);
    app.update(Msg::MouseClick {
        col,
        row,
        multi: false,
    });
    assert!(app.overlays.station_card.is_some());
}
