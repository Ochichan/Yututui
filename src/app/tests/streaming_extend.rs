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
