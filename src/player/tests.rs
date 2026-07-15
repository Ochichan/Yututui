use super::*;
#[cfg(unix)]
use crate::util::process::ProcessProfile;
#[cfg(unix)]
use crate::util::process_guard::ChildTreeGuard;

#[test]
fn file_generation_advances_only_for_admitted_load_and_stop_barriers() {
    let (tx, mut rx) = tokio::sync::mpsc::channel(4);
    let handle = PlayerHandle::test_handle(tx);
    assert_eq!(handle.current_file_generation(), 0);

    assert!(
        handle
            .send(PlayerCmd::load(
                "https://example.invalid/a",
                MediaSourceContext::OnDemand,
            ))
            .is_ok()
    );
    assert_eq!(handle.current_file_generation(), 1);
    assert!(handle.event_is_current(&PlayerEvent::file_scoped(
        1,
        PlayerEvent::Duration(Some(10.0)),
    )));
    assert!(!handle.event_is_current(&PlayerEvent::file_scoped(0, PlayerEvent::Eof)));

    assert!(handle.send(PlayerCmd::SetVolume(20)).is_ok());
    assert_eq!(handle.current_file_generation(), 1);
    assert!(handle.send(PlayerCmd::Stop).is_ok());
    assert_eq!(handle.current_file_generation(), 2);

    assert!(matches!(rx.try_recv(), Ok(PlayerCmd::Load(_))));
    assert!(matches!(rx.try_recv(), Ok(PlayerCmd::SetVolume(20))));
    assert!(matches!(rx.try_recv(), Ok(PlayerCmd::Stop)));
}

#[test]
fn admitted_media_projection_is_fail_closed_without_mutating_physical_runtime() {
    let (tx, _rx) = tokio::sync::mpsc::channel(4);
    let handle = PlayerHandle::test_handle(tx);
    let status = |effective, reason, generation, bytes| long_form_seek::CacheStatus {
        requested: crate::config::LongFormSeekOptimization::On,
        effective,
        reason,
        file_generation: generation,
        policy_revision: 9,
        file_cache_bytes: bytes,
        peak_file_cache_bytes: bytes,
    };

    let _ = handle
        .send(PlayerCmd::load(
            "https://example.invalid/a",
            MediaSourceContext::OnDemand,
        ))
        .unwrap();
    let projected = handle.long_form_seek_status();
    assert_eq!(projected.file_generation, Some(1));
    assert_eq!(
        projected.effective,
        long_form_seek::CacheEffectiveState::Probing
    );
    assert_eq!(
        projected.reason,
        long_form_seek::CacheReason::AwaitingMediaFacts
    );
    assert_eq!(projected.policy_revision, 0);

    handle.test_set_long_form_seek_runtime(
        status(
            long_form_seek::CacheEffectiveState::DiskActive,
            long_form_seek::CacheReason::OnEligibleMedia,
            Some(1),
            4096,
        ),
        Some(long_form_seek::CacheReason::ProbeFailed),
        Some(25),
    );
    assert_eq!(
        handle.long_form_seek_status().effective,
        long_form_seek::CacheEffectiveState::DiskActive
    );

    let _ = handle.send(PlayerCmd::Stop).unwrap();
    let stopped = handle.long_form_seek_status();
    assert_eq!(
        stopped.effective,
        long_form_seek::CacheEffectiveState::NoMedia
    );
    assert_eq!(stopped.reason, long_form_seek::CacheReason::NoMedia);
    assert_eq!(stopped.file_generation, None);
    assert_eq!(stopped.file_cache_bytes, 0);
    assert_eq!(stopped.policy_revision, 9);
    let raw = handle.long_form_seek_runtime_status();
    assert_eq!(
        raw.status.effective,
        long_form_seek::CacheEffectiveState::DiskActive
    );
    assert_eq!(raw.status.file_generation, Some(1));
    assert_eq!(raw.status.file_cache_bytes, 4096);
    assert_eq!(
        raw.last_failure,
        Some(long_form_seek::CacheReason::ProbeFailed)
    );
    assert_eq!(raw.last_cleanup_ms, Some(25));
}

#[test]
fn rejected_load_rolls_back_the_expected_file_generation() {
    let (tx, _rx) = tokio::sync::mpsc::channel(1);
    assert!(tx.try_send(PlayerCmd::SetVolume(1)).is_ok());
    let handle = PlayerHandle::test_handle(tx);

    assert_eq!(
        handle.send(PlayerCmd::load(
            "https://example.invalid/a",
            MediaSourceContext::OnDemand,
        )),
        Err(DeliveryError::Busy)
    );
    assert_eq!(handle.current_file_generation(), 0);
}

#[cfg(unix)]
#[tokio::test(flavor = "current_thread")]
async fn mpv_drop_terminates_media_process_group_descendants() {
    use std::process::Stdio;

    let _pid_guard = lifetime::lock_mpv_pid_for_test().await;
    let root = std::env::temp_dir().join(format!(
        "ytt-mpv-tree-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .expect("clock after epoch")
            .as_nanos()
    ));
    std::fs::create_dir_all(&root).expect("create mpv tree fixture");
    let pid_file = root.join("helper.pid");
    let script = format!("sleep 10 & echo $! > '{}'; wait", pid_file.display());
    let mut command = crate::util::process::std_command("sh", ProcessProfile::Media);
    command
        .args(["-c", &script])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    let child = command.spawn().expect("spawn fake long-lived mpv tree");
    let child_tree = ChildTreeGuard::for_std(&child, ProcessProfile::Media);
    let mpv = Mpv {
        child_tree,
        child: Some(child),
        guardian_lease: None,
        ipc_path: root.join("mpv.sock").to_string_lossy().into_owned(),
    };

    let helper_pid = tokio::time::timeout(std::time::Duration::from_secs(2), async {
        loop {
            if let Ok(contents) = std::fs::read_to_string(&pid_file)
                && let Ok(pid) = contents.trim().parse::<libc::pid_t>()
            {
                break pid;
            }
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("helper pid should be published");
    assert!(crate::util::process::process_exists_for_test(helper_pid));

    drop(mpv);

    tokio::time::timeout(std::time::Duration::from_secs(2), async {
        while crate::util::process::process_exists_for_test(helper_pid) {
            tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        }
    })
    .await
    .expect("mpv helper survived Mpv::drop");
    std::fs::remove_dir_all(root).expect("remove mpv tree fixture");
}

#[test]
fn pending_seek_coalescing_preserves_arrival_semantics() {
    let mut pending = PlayerPending::default();
    assert!(!pending.push(PlayerCmd::SeekRelative(2.0)).unwrap());
    assert!(pending.push(PlayerCmd::SeekRelative(3.5)).unwrap());
    assert!(pending.push(PlayerCmd::interactive_seek(12.0)).unwrap());
    assert!(!pending.push(PlayerCmd::SeekRelative(4.0)).unwrap());

    assert!(matches!(
        pending.cmds.pop_front(),
        Some(PlayerCmd::SeekAbsolute {
            seconds: secs,
            precision: SeekPrecision::InteractiveFast,
        }) if (secs - 12.0).abs() < f64::EPSILON
    ));
    assert!(matches!(
        pending.cmds.pop_front(),
        Some(PlayerCmd::SeekRelative(secs)) if (secs - 4.0).abs() < f64::EPSILON
    ));
    assert!(pending.cmds.is_empty());
}

#[test]
fn pending_keeps_latest_volume_and_filter_value() {
    let mut pending = PlayerPending::default();
    assert!(!pending.push(PlayerCmd::SetVolume(20)).unwrap());
    assert!(pending.push(PlayerCmd::SetVolume(30)).unwrap());
    assert!(
        !pending
            .push(PlayerCmd::AfCommand {
                label: "eq".to_owned(),
                param: "gain".to_owned(),
                value: "1".to_owned(),
            })
            .unwrap()
    );
    assert!(
        pending
            .push(PlayerCmd::AfCommand {
                label: "eq".to_owned(),
                param: "gain".to_owned(),
                value: "2".to_owned(),
            })
            .unwrap()
    );

    match pending.cmds.pop_front() {
        Some(PlayerCmd::SetVolume(vol)) => assert_eq!(vol, 30),
        _ => panic!("expected latest volume"),
    }
    match pending.cmds.pop_front() {
        Some(PlayerCmd::AfCommand {
            label,
            param,
            value,
        }) => {
            assert_eq!(label, "eq");
            assert_eq!(param, "gain");
            assert_eq!(value, "2");
        }
        _ => panic!("expected latest af command"),
    }
    assert!(pending.cmds.is_empty());
}

#[test]
fn critical_barriers_bound_coalescing_and_toggle_pairs_cancel() {
    let mut pending = PlayerPending::default();
    assert!(!pending.push(PlayerCmd::SetVolume(20)).unwrap());
    assert!(
        !pending
            .push(PlayerCmd::load("old", MediaSourceContext::OnDemand))
            .unwrap()
    );
    assert!(
        !pending
            .push(PlayerCmd::load("new", MediaSourceContext::OnDemand))
            .unwrap()
    );
    assert!(!pending.push(PlayerCmd::SetVolume(30)).unwrap());
    assert!(!pending.push(PlayerCmd::CyclePause).unwrap());
    assert!(pending.push(PlayerCmd::CyclePause).unwrap());

    assert!(matches!(
        pending.cmds.pop_front(),
        Some(PlayerCmd::SetVolume(20))
    ));
    assert!(matches!(
        pending.cmds.pop_front(),
        Some(PlayerCmd::Load(url)) if url == "old"
    ));
    assert!(matches!(
        pending.cmds.pop_front(),
        Some(PlayerCmd::Load(url)) if url == "new"
    ));
    assert!(matches!(
        pending.cmds.pop_front(),
        Some(PlayerCmd::SetVolume(30))
    ));
    assert!(pending.cmds.is_empty());
}

#[tokio::test]
async fn full_player_lane_defers_control_with_one_ordered_drainer() {
    let (tx, mut rx) = mpsc::channel(1);
    assert!(tx.try_send(PlayerCmd::SetVolume(1)).is_ok());
    let handle = PlayerHandle::test_handle(tx);

    assert_eq!(handle.send(PlayerCmd::Stop), Ok(DeliveryReceipt::Deferred));

    assert!(matches!(rx.recv().await, Some(PlayerCmd::SetVolume(1))));
    assert!(matches!(rx.recv().await, Some(PlayerCmd::Stop)));
}

#[tokio::test]
async fn full_player_lane_coalesces_latest_value_without_reordering_control() {
    let (tx, mut rx) = mpsc::channel(1);
    assert!(tx.try_send(PlayerCmd::Stop).is_ok());
    let handle = PlayerHandle::test_handle(tx);

    assert_eq!(
        handle.send(PlayerCmd::SetVolume(40)),
        Ok(DeliveryReceipt::Deferred)
    );
    assert!(matches!(
        handle.send(PlayerCmd::SetVolume(80)),
        Ok(DeliveryReceipt::Coalesced {
            replaced_existing: true,
            ..
        })
    ));

    assert!(matches!(rx.recv().await, Some(PlayerCmd::Stop)));
    assert!(matches!(rx.recv().await, Some(PlayerCmd::SetVolume(80))));
}

#[tokio::test(flavor = "current_thread")]
async fn command_batch_enters_backlog_atomically_and_preserves_order() {
    let (tx, mut rx) = mpsc::channel(4);
    let handle = PlayerHandle::test_handle(tx);

    assert_eq!(
        handle.send_batch(vec![
            PlayerCmd::Stop,
            PlayerCmd::SetVolume(42),
            PlayerCmd::interactive_seek(19.0),
        ]),
        Ok(DeliveryReceipt::Deferred)
    );

    // A multi-command batch never exposes a direct-channel prefix. The spawned drainer
    // cannot run on this current-thread runtime until this task yields.
    assert!(matches!(
        rx.try_recv(),
        Err(mpsc::error::TryRecvError::Empty)
    ));
    {
        let pending = handle
            .pending
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        assert_eq!(pending.cmds.len(), 3);
        assert!(pending.drainer_running);
    }

    assert!(matches!(rx.recv().await, Some(PlayerCmd::Stop)));
    assert!(matches!(rx.recv().await, Some(PlayerCmd::SetVolume(42))));
    assert!(matches!(
        rx.recv().await,
        Some(PlayerCmd::SeekAbsolute {
            seconds: secs,
            precision: SeekPrecision::InteractiveFast,
        }) if (secs - 19.0).abs() < f64::EPSILON
    ));
}

#[test]
fn saturated_batch_does_not_commit_a_coalesced_prefix() {
    let (tx, _rx) = mpsc::channel(1);
    let handle = PlayerHandle::test_handle(tx);
    {
        let mut pending = handle
            .pending
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        for _ in 0..PLAYER_PENDING_MAX - 1 {
            assert!(!pending.push(PlayerCmd::Stop).unwrap());
        }
        assert!(
            !pending
                .push(PlayerCmd::load("old", MediaSourceContext::OnDemand))
                .unwrap()
        );
        // Keep admission on the backlog-only path without needing a Tokio runtime.
        pending.drainer_running = true;
    }

    // Neither command fits. Atomic staging must preserve the old Load instead of
    // publishing a partial recovery transaction.
    assert_eq!(
        handle.send_batch(vec![
            PlayerCmd::load("new", MediaSourceContext::OnDemand),
            PlayerCmd::Stop,
        ]),
        Err(DeliveryError::Busy)
    );

    let pending = handle
        .pending
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    assert_eq!(pending.cmds.len(), PLAYER_PENDING_MAX);
    assert!(matches!(
        pending.cmds.back(),
        Some(PlayerCmd::Load(url)) if url == "old"
    ));
}

#[test]
fn batch_coalescing_respects_barriers_and_never_revokes_accepted_loads() {
    let mut pending = PlayerPending::default();
    let staged = pending
        .stage_batch(vec![
            PlayerCmd::SetVolume(10),
            PlayerCmd::load("track", MediaSourceContext::OnDemand),
            PlayerCmd::SetVolume(20),
            PlayerCmd::SetVolume(30),
        ])
        .unwrap();
    pending.cmds = staged.cmds;

    assert!(matches!(
        pending.cmds.pop_front(),
        Some(PlayerCmd::SetVolume(10))
    ));
    assert!(matches!(
        pending.cmds.pop_front(),
        Some(PlayerCmd::Load(url)) if url == "track"
    ));
    assert!(matches!(
        pending.cmds.pop_front(),
        Some(PlayerCmd::SetVolume(30))
    ));
    assert!(pending.cmds.is_empty());

    for _ in 0..PLAYER_PENDING_MAX - 1 {
        assert!(!pending.push(PlayerCmd::Stop).unwrap());
    }
    assert!(
        !pending
            .push(PlayerCmd::load("old", MediaSourceContext::OnDemand))
            .unwrap()
    );
    assert!(matches!(
        pending.stage_batch(vec![
            PlayerCmd::load("new", MediaSourceContext::OnDemand),
            PlayerCmd::load("newest", MediaSourceContext::OnDemand),
        ]),
        Err(DeliveryError::Busy)
    ));
    assert_eq!(pending.cmds.len(), PLAYER_PENDING_MAX);
    assert!(matches!(
        pending.cmds.back(),
        Some(PlayerCmd::Load(url)) if url == "old"
    ));
}

#[tokio::test(flavor = "current_thread")]
async fn separate_track_loads_stay_distinct_behind_a_full_player_lane() {
    let (tx, mut rx) = mpsc::channel(1);
    assert!(tx.try_send(PlayerCmd::Stop).is_ok());
    let handle = PlayerHandle::test_handle(tx);

    assert_eq!(
        handle.send(PlayerCmd::load("track-b", MediaSourceContext::OnDemand)),
        Ok(DeliveryReceipt::Deferred)
    );
    assert_eq!(
        handle.send(PlayerCmd::load("track-c", MediaSourceContext::OnDemand)),
        Ok(DeliveryReceipt::Deferred)
    );
    {
        let pending = handle
            .pending
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        assert_eq!(pending.cmds.len(), 2);
        assert!(matches!(
            pending.cmds.front(),
            Some(PlayerCmd::Load(url)) if url == "track-b"
        ));
        assert!(matches!(
            pending.cmds.back(),
            Some(PlayerCmd::Load(url)) if url == "track-c"
        ));
    }

    assert!(matches!(rx.recv().await, Some(PlayerCmd::Stop)));
    assert!(matches!(
        rx.recv().await,
        Some(PlayerCmd::Load(url)) if url == "track-b"
    ));
    assert!(matches!(
        rx.recv().await,
        Some(PlayerCmd::Load(url)) if url == "track-c"
    ));
}

#[test]
fn closed_player_lane_rejects_the_whole_batch() {
    let (tx, rx) = mpsc::channel(2);
    drop(rx);
    let handle = PlayerHandle::test_handle(tx);

    assert_eq!(
        handle.send_batch(vec![PlayerCmd::Stop, PlayerCmd::SetVolume(20)]),
        Err(DeliveryError::Closed)
    );
    let pending = handle
        .pending
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    assert!(pending.closed);
    assert!(pending.cmds.is_empty());
}

#[test]
fn empty_player_batch_is_never_admitted() {
    let (open_tx, _open_rx) = mpsc::channel(1);
    let open_handle = PlayerHandle::test_handle(open_tx);
    assert_eq!(open_handle.send_batch(Vec::new()), Err(DeliveryError::Busy));

    let (closed_tx, closed_rx) = mpsc::channel(1);
    drop(closed_rx);
    let closed_handle = PlayerHandle::test_handle(closed_tx);
    assert_eq!(
        closed_handle.send_batch(Vec::new()),
        Err(DeliveryError::Busy)
    );
}

#[test]
fn closed_player_lane_reports_closed() {
    let (tx, rx) = mpsc::channel(1);
    drop(rx);
    let handle = PlayerHandle::test_handle(tx);

    assert_eq!(handle.send(PlayerCmd::Stop), Err(DeliveryError::Closed));
}

#[tokio::test(flavor = "current_thread")]
async fn active_player_drainer_does_not_admit_after_lane_closes() {
    let (tx, rx) = mpsc::channel(1);
    assert!(tx.try_send(PlayerCmd::Stop).is_ok());
    let handle = PlayerHandle::test_handle(tx);

    assert_eq!(
        handle.send(PlayerCmd::SetVolume(40)),
        Ok(DeliveryReceipt::Deferred)
    );
    // Keep the drainer unpolled until after the receiver closes, then make
    // admission synchronously observe the sender's closed state.
    drop(rx);
    assert_eq!(
        handle.send(PlayerCmd::SetVolume(80)),
        Err(DeliveryError::Closed)
    );
    assert_eq!(handle.send(PlayerCmd::Stop), Err(DeliveryError::Closed));

    let pending = handle
        .pending
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    assert!(pending.closed);
    assert!(!pending.drainer_running);
    assert!(pending.cmds.is_empty());
}

#[test]
fn full_player_lane_outside_runtime_reports_busy() {
    let (tx, _rx) = mpsc::channel(1);
    assert!(tx.try_send(PlayerCmd::Stop).is_ok());
    let handle = PlayerHandle::test_handle(tx);

    assert_eq!(handle.send(PlayerCmd::CyclePause), Err(DeliveryError::Busy));
}

#[tokio::test]
async fn player_pending_backlog_is_bounded() {
    let (tx, _rx) = mpsc::channel(1);
    assert!(tx.try_send(PlayerCmd::Stop).is_ok());
    let handle = PlayerHandle::test_handle(tx);

    for _ in 0..PLAYER_PENDING_MAX {
        assert!(handle.send(PlayerCmd::Stop).is_ok());
    }
    assert_eq!(handle.send(PlayerCmd::Stop), Err(DeliveryError::Busy));
}

#[test]
fn shared_cache_status_retains_last_failure_and_completed_cleanup_without_identity() {
    let status = |effective, reason, generation| long_form_seek::CacheStatus {
        requested: crate::config::LongFormSeekOptimization::On,
        effective,
        reason,
        file_generation: generation,
        policy_revision: 1,
        file_cache_bytes: 0,
        peak_file_cache_bytes: 0,
    };
    let mut shared = SharedLongFormSeekStatus::new(status(
        long_form_seek::CacheEffectiveState::EnablePending,
        long_form_seek::CacheReason::OnEligibleMedia,
        Some(7),
    ));
    shared.update(status(
        long_form_seek::CacheEffectiveState::RamOnly,
        long_form_seek::CacheReason::PropertyRejected,
        Some(7),
    ));
    shared.update(status(
        long_form_seek::CacheEffectiveState::DisablePending,
        long_form_seek::CacheReason::MediaClosed,
        Some(7),
    ));
    shared.update(status(
        long_form_seek::CacheEffectiveState::NoMedia,
        long_form_seek::CacheReason::MediaClosed,
        None,
    ));

    assert_eq!(
        shared.runtime.last_failure,
        Some(long_form_seek::CacheReason::PropertyRejected)
    );
    assert!(shared.runtime.last_cleanup_ms.is_some());
}

#[test]
fn shared_cache_history_survives_player_recycle() {
    let status = |effective, reason, generation| long_form_seek::CacheStatus {
        requested: crate::config::LongFormSeekOptimization::On,
        effective,
        reason,
        file_generation: generation,
        policy_revision: 1,
        file_cache_bytes: 0,
        peak_file_cache_bytes: 0,
    };
    let history = Arc::new(Mutex::new(LongFormSeekHistory::default()));
    let mut first = SharedLongFormSeekStatus::with_history(
        status(
            long_form_seek::CacheEffectiveState::DiskActive,
            long_form_seek::CacheReason::OnEligibleMedia,
            Some(7),
        ),
        Arc::clone(&history),
    );
    first.update(status(
        long_form_seek::CacheEffectiveState::RamOnly,
        long_form_seek::CacheReason::PropertyRejected,
        Some(7),
    ));
    first.update(status(
        long_form_seek::CacheEffectiveState::DisablePending,
        long_form_seek::CacheReason::MediaClosed,
        Some(7),
    ));
    drop(first);

    let mut replacement = SharedLongFormSeekStatus::with_history(
        status(
            long_form_seek::CacheEffectiveState::NoMedia,
            long_form_seek::CacheReason::NoMedia,
            None,
        ),
        history,
    );
    assert_eq!(
        replacement.runtime.last_failure,
        Some(long_form_seek::CacheReason::PropertyRejected)
    );
    replacement.update(status(
        long_form_seek::CacheEffectiveState::NoMedia,
        long_form_seek::CacheReason::MediaClosed,
        None,
    ));
    assert!(replacement.runtime.last_cleanup_ms.is_some());
}
