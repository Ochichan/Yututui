use super::*;

#[test]
fn capacity_release_ignores_reordered_stale_true_until_a_fresh_probe() {
    use crate::recorder::job::{RecorderEvent, RecorderJob};

    let mut app = recording_app(crate::recorder::RecordingMode::Everything);
    feed_title(&mut app, "A - One");
    feed_title(&mut app, "B - Two");
    backdate_current(&mut app, 60);
    feed_title(&mut app, "C - Three");
    let owner_id = app.recorder.history.front().unwrap().id;
    let mut teardown = app.on_recorder_event(RecorderEvent::CapacityBlocked {
        id: owner_id,
        pending_count: 128,
        pending_bytes: 1024,
    });
    admit_player_transition(&mut app, &mut teardown);

    let retry = app.on_recorder_event(RecorderEvent::Saved {
        id: 999,
        final_path: "/tmp/old.mkv".into(),
        recovery_owned: false,
        durability_warning: None,
        capacity_available: true,
    });
    assert!(retry.iter().any(|effect| matches!(
        effect,
        Cmd::Recorder(RecorderJob::Save { id, .. }) if *id == owner_id
    )));

    let no_capacity = app.on_recorder_event(RecorderEvent::Saved {
        id: owner_id,
        final_path: "/tmp/owner.mkv".into(),
        recovery_owned: false,
        durability_warning: None,
        capacity_available: false,
    });
    assert!(
        no_capacity
            .iter()
            .all(|effect| !matches!(effect, Cmd::Recorder(RecorderJob::ProbeCapacity { .. })))
    );
    assert!(app.recorder.capacity_blocked);
    assert!(app.recorder.capacity_owner_settled);

    let stale_true = app.on_recorder_event(RecorderEvent::Saved {
        id: 998,
        final_path: "/tmp/stale.mkv".into(),
        recovery_owned: false,
        durability_warning: None,
        capacity_available: true,
    });
    assert!(app.recorder.capacity_blocked);
    assert!(stale_true.iter().any(|effect| matches!(
        effect,
        Cmd::Recorder(RecorderJob::ProbeCapacity { owner_id: id, .. }) if *id == owner_id
    )));
    assert!(
        app.on_recorder_event(RecorderEvent::CapacityProbed {
            owner_id,
            capacity_available: false,
        })
        .is_empty()
    );
    assert!(app.recorder.capacity_blocked);

    let probe_again = app.on_recorder_event(RecorderEvent::AlreadySettled {
        id: 997,
        capacity_available: true,
    });
    assert!(probe_again.iter().any(|effect| matches!(
        effect,
        Cmd::Recorder(RecorderJob::ProbeCapacity { owner_id: id, .. }) if *id == owner_id
    )));
    let mut resumed = app.on_recorder_event(RecorderEvent::CapacityProbed {
        owner_id,
        capacity_available: true,
    });
    assert!(!app.recorder.capacity_blocked);
    admit_player_transition(&mut app, &mut resumed);
}
