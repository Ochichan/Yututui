//! Dedicated-Local-mode daemon regressions kept separate from the size-pinned engine suite.

use super::*;

fn song(id: &str) -> Song {
    Song::remote(id, format!("title-{id}"), "artist", "3:00")
}

#[test]
fn local_mode_suppresses_normal_and_forced_top_up_without_losing_preference() {
    let mut engine = tests::engine_with_queue(&["seed"]);
    engine.streaming = true;
    engine.config.autoplay_streaming = Some(true);
    engine.last_mode = LastMode::Local;

    assert!(!engine.streaming_active());
    assert!(engine.maybe_autoplay_extend().is_empty());
    assert!(engine.force_autoplay_extend().is_empty());
    assert!(!engine.streaming_pending);
    let local_status = engine.status();
    assert!(
        !local_status.streaming,
        "wire state reports the effective value"
    );
    assert!(
        local_status.settings.autoplay_streaming,
        "settings keep the saved preference"
    );
    assert_eq!(engine.config.autoplay_streaming, Some(true));

    engine.last_mode = LastMode::Normal;
    assert!(engine.streaming_active());
    assert!(matches!(
        engine.force_autoplay_extend().as_slice(),
        [EngineEffect::StreamingFallback { seed_video_id, .. }] if seed_video_id == "seed"
    ));
}

#[test]
fn session_snapshot_preserves_local_mode_queue() {
    let mut engine = tests::engine_with_queue(&["local-a", "local-b"]);
    engine.last_mode = LastMode::Local;
    engine.queue.next(false);
    engine.inactive_normal_queue = Some({
        let mut queue = Queue::default();
        queue.set(vec![song("normal")], 0);
        queue.snapshot()
    });
    engine.inactive_radio_queue = Some({
        let mut queue = Queue::default();
        queue.set(vec![tests::radio_station("radio")], 0);
        queue.snapshot()
    });

    let cache = engine.session_cache_snapshot();

    assert_eq!(cache.last_mode, LastMode::Local);
    assert_eq!(
        cache.local_queue.as_ref().map(|snapshot| snapshot.cursor),
        Some(1)
    );
    assert_eq!(
        cache
            .normal_queue
            .as_ref()
            .map(|snapshot| snapshot.songs.len()),
        Some(1)
    );
    assert_eq!(
        cache
            .radio_queue
            .as_ref()
            .map(|snapshot| snapshot.songs.len()),
        Some(1)
    );
}
