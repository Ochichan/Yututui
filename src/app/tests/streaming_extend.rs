use super::*;

#[test]
fn streaming_extend_resumes_playback_when_idle() {
    let mut app = App::new(100);
    app.queue.set(vec![Song::remote("a", "A", "x", "1:00")], 0);
    app.prefetch.loaded_video_id = None; // the seed ended before this refill landed
    let mut cmds = app.extend_queue_from_streaming(vec![Song::remote("b", "B", "y", "2:00")]);
    admit_player_transition(&mut app, &mut cmds);
    assert_loads_video(&cmds, "b");
    assert_eq!(app.prefetch.loaded_video_id.as_deref(), Some("b"));
    assert_eq!(app.streaming.consecutive_failures, 0);
    assert_eq!(
        app.why_gem_for("b").map(|model| model.slot.as_str()),
        Some("Balanced")
    );
}

#[test]
fn idle_streaming_success_waits_for_player_admission() {
    use crate::util::delivery::DeliveryError;

    let mut app = App::new(100);
    app.queue.set(vec![Song::remote("a", "A", "x", "1:00")], 0);
    app.prefetch.loaded_video_id = None;
    app.streaming.consecutive_failures = 2;
    app.streaming.pending_queue_revision = Some(app.queue.rev());
    app.status.kind = StatusKind::Info;
    app.status.text = "waiting for admission".to_owned();

    let cmds = app.extend_queue_from_streaming(vec![Song::remote("b", "B", "y", "2:00")]);

    assert_eq!(app.queue.len(), 1);
    assert_eq!(app.streaming.consecutive_failures, 2);
    assert_eq!(app.status.kind, StatusKind::Info);
    assert_eq!(app.status.text, "waiting for admission");
    assert!(app.why_gem_for("b").is_none());
    assert!(reject_player_transition(&mut app, cmds, DeliveryError::Busy).is_empty());
    assert!(
        app.streaming.pending_queue_revision.is_none(),
        "rejected idle admission terminates the refill generation"
    );
    assert_eq!(app.queue.len(), 1);
    assert_eq!(app.streaming.consecutive_failures, 2);
    assert_eq!(app.status.kind, StatusKind::Error);
    assert_ne!(
        app.status.text, "Queued 1 track(s)",
        "a rejected player intent must not publish the success toast"
    );
    assert!(app.why_gem_for("b").is_none());

    let mut retry = app.extend_queue_from_streaming(vec![Song::remote("b", "B", "y", "2:00")]);
    admit_player_transition(&mut app, &mut retry);
    assert!(app.queue.contains_video_id("b"));
    assert_eq!(app.streaming.consecutive_failures, 0);
    assert_ne!(app.status.text, "waiting for admission");
    assert!(app.why_gem_for("b").is_some());
}

#[test]
fn streaming_extend_prefetches_next_while_playing() {
    let mut app = App::new(100);
    app.queue.set(vec![Song::remote("a", "A", "x", "1:00")], 0);
    app.prefetch.loaded_video_id = Some("a".to_owned()); // still playing the seed
    let cmds = app.extend_queue_from_streaming(vec![Song::remote("b", "B", "y", "2:00")]);
    assert_no_load(&cmds);
    assert!(
        cmds.iter()
            .any(|c| matches!(c, Cmd::Resolve { video_id, .. } if video_id == "b")),
        "should prefetch the upcoming track's stream"
    );
}

#[test]
fn advance_over_all_unplayable_queue_terminates_under_repeat_all() {
    let mut app = App::new(100);
    // Nothing but non-video YouTube refs (channel / playlist ids) — every entry is unplayable.
    app.queue.set(
        vec![
            Song::remote("UCfLdIEPs1tYj4ieEdJnyNyw", "A", "A", ""),
            Song::remote("UCanotherchannelidhere00", "B", "B", ""),
            Song::remote("PL123456789012345", "C", "C", ""),
        ],
        0,
    );
    // Repeat-all makes `peek_next()` always `Some` — the old recursion wrapped forever and
    // overflowed the stack. The bounded loop must return without loading anything playable.
    app.queue.repeat = crate::queue::Repeat::All;
    let cmds = app.advance(true);
    assert_no_load(&cmds);
}
