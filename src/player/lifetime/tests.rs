use super::*;

#[test]
fn claimed_shutdown_publishes_cause_before_becoming_observable() {
    let shutdown = ShutdownLatch::new();
    let claimed_shutdown = shutdown.clone();
    let emergency_shutdown = shutdown.clone();
    let (entered_tx, entered_rx) = std::sync::mpsc::sync_channel(1);
    let (release_tx, release_rx) = std::sync::mpsc::sync_channel(1);
    let claimant = std::thread::spawn(move || {
        claimed_shutdown.try_trigger_with_before_publish(
            || {
                let _ = entered_tx.send(());
                release_rx.recv().unwrap();
            },
            || assert!(emergency_shutdown.is_triggered()),
        )
    });

    entered_rx.recv().unwrap();
    assert!(!shutdown.is_triggered());
    shutdown.trigger();
    assert!(
        !shutdown.is_triggered(),
        "a losing trigger cannot expose a claim whose cause is not published"
    );
    release_tx.send(()).unwrap();
    assert!(claimant.join().unwrap());
    assert!(shutdown.is_triggered());
}

fn temp_dir(name: &str) -> std::path::PathBuf {
    let mut bytes = [0u8; 8];
    getrandom::fill(&mut bytes).unwrap();
    let suffix = bytes.iter().map(|b| format!("{b:02x}")).collect::<String>();
    std::env::temp_dir().join(format!(
        "yututui-lifetime-{name}-{}-{suffix}",
        std::process::id()
    ))
}

#[tokio::test]
async fn signal_phase_escalates_to_hard_exit_only_on_a_repeat_signal() {
    // `request_signal_shutdown` touches the global media registry via `kill_mpv_now`.
    let _pid_guard = lock_mpv_pid_for_test().await;
    let shutdown = ShutdownLatch::new();
    let emit_count = std::cell::Cell::new(0usize);
    let emit = |_: SignalEvent| emit_count.set(emit_count.get() + 1);
    let mut phase = SignalPhase::Cooperative;

    // First signal: cooperative shutdown only — latch trips, owner is asked to quit,
    // and no hard exit is demanded.
    assert_eq!(
        advance_signal_phase(&mut phase, &shutdown, &emit, "SIGTERM", 143),
        None
    );
    assert!(shutdown.is_triggered());
    assert!(shutdown.was_triggered_by_signal());
    assert_eq!(emit_count.get(), 1);

    // Any repeat signal escalates with its own exit code and must not re-run the
    // cooperative path.
    assert_eq!(
        advance_signal_phase(&mut phase, &shutdown, &emit, "SIGINT", 130),
        Some(130)
    );
    assert_eq!(emit_count.get(), 1);
    assert_eq!(
        advance_signal_phase(&mut phase, &shutdown, &emit, "SIGHUP", 129),
        Some(129)
    );
    assert_eq!(emit_count.get(), 1);
    reset_media_registry_for_test();
}

#[tokio::test]
async fn terminal_failure_winner_cannot_be_reclassified_by_a_later_signal() {
    let _pid_guard = lock_mpv_pid_for_test().await;
    let shutdown = ShutdownLatch::new();
    assert!(shutdown.try_trigger_with_before_publish(|| {}, || {}));

    request_signal_shutdown(&shutdown, &|_| {});

    assert!(shutdown.is_triggered());
    assert!(!shutdown.was_triggered_by_signal());
    reset_media_registry_for_test();
}

#[tokio::test]
async fn shutdown_gate_permanently_rejects_a_late_guardian_registration() {
    let _pid_guard = lock_mpv_pid_for_test().await;
    kill_mpv_now();
    assert!(register_live_mpv(123_456, 123_456).is_err());
    reset_media_registry_for_test();
}

#[tokio::test]
async fn blocked_guardian_slot_upgrades_to_the_actual_mpv_atomically() {
    let _pid_guard = lock_mpv_pid_for_test().await;
    let mut registration = register_live_mpv(123_456, 123_456).unwrap();
    registration.publish_mpv_pid(654_321).unwrap();
    assert_eq!(take_mpv_pid(), Some(654_321));
    drop(registration);
    reset_media_registry_for_test();
}

#[tokio::test]
async fn owner_cannot_release_guardian_pid_during_emergency_signal_claim() {
    let _pid_guard = lock_mpv_pid_for_test().await;
    let registration = register_live_mpv(123_456, 654_321).unwrap();
    let entry = &MEDIA_PIDS[registration.slot];
    entry
        .state
        .compare_exchange(
            SLOT_LIVE,
            SLOT_EMERGENCY_KILL,
            Ordering::SeqCst,
            Ordering::SeqCst,
        )
        .expect("fixture claims emergency teardown");

    let (dropped_tx, dropped_rx) = std::sync::mpsc::sync_channel(1);
    let dropper = std::thread::spawn(move || {
        drop(registration);
        let _ = dropped_tx.send(());
    });
    assert!(
        dropped_rx
            .recv_timeout(std::time::Duration::from_millis(50))
            .is_err(),
        "normal owner teardown must retain the Child/pid while emergency signalling owns it"
    );

    entry.packed.store(0, Ordering::SeqCst);
    entry.state.store(SLOT_FREE, Ordering::SeqCst);
    dropped_rx
        .recv_timeout(std::time::Duration::from_secs(1))
        .expect("owner teardown resumes after emergency signalling releases the pid");
    dropper.join().unwrap();
    reset_media_registry_for_test();
}

#[tokio::test]
async fn stale_registration_drop_cannot_clear_a_reused_slot() {
    let _pid_guard = lock_mpv_pid_for_test().await;
    let mut stale = register_live_mpv(123_456, 654_321).unwrap();
    let slot = stale.slot;
    let entry = &MEDIA_PIDS[slot];
    entry
        .state
        .compare_exchange(
            SLOT_LIVE,
            SLOT_OWNER_CLEANUP,
            Ordering::SeqCst,
            Ordering::SeqCst,
        )
        .expect("fixture claims normal owner teardown");
    // Model the exact tail of `terminate_claimed_slot` without signalling a made-up PID from
    // the test process: ownership must be revoked before SLOT_FREE becomes reusable.
    stale.active = false;
    entry.packed.store(0, Ordering::SeqCst);
    entry.state.store(SLOT_FREE, Ordering::SeqCst);

    let replacement = register_live_mpv(234_567, 765_432).unwrap();
    assert_eq!(replacement.slot, slot, "fixture must reuse the exact slot");
    let replacement_packed = replacement.packed;
    drop(stale);

    assert_eq!(entry.state.load(Ordering::SeqCst), SLOT_LIVE);
    assert_eq!(entry.packed.load(Ordering::SeqCst), replacement_packed);
    drop(replacement);
    reset_media_registry_for_test();
}

#[cfg(unix)]
#[tokio::test]
async fn watchdog_kill_between_ready_and_mpv_publication_wins_fail_closed() {
    use std::process::Stdio;

    let _pid_guard = lock_mpv_pid_for_test().await;
    let mut command =
        crate::util::process::std_command("sleep", crate::util::process::ProcessProfile::Media);
    command
        .arg("30")
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    let mut guardian = command.spawn().expect("spawn blocked guardian fixture");
    let guardian_pid = guardian.id();
    let mut registration = register_live_mpv(guardian_pid, guardian_pid).unwrap();

    // Model the terminal watchdog firing after the real guardian emitted Ready but before the
    // owner published Ready's actual mpv pid into the temporary slot.
    kill_mpv_now();
    assert!(registration.publish_mpv_pid(654_321).is_err());
    guardian
        .wait()
        .expect("watchdog must terminate guardian fixture");

    drop(registration);
    reset_media_registry_for_test();
}

#[cfg(unix)]
#[tokio::test]
async fn out_of_band_shutdown_never_signals_the_recorded_actual_pid() {
    use std::process::Stdio;

    let _pid_guard = lock_mpv_pid_for_test().await;
    let spawn_fixture = || {
        let mut command =
            crate::util::process::std_command("sleep", crate::util::process::ProcessProfile::Media);
        command
            .arg("30")
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null());
        command.spawn().expect("spawn media registry fixture")
    };
    let mut unrelated_actual_pid = spawn_fixture();
    let mut guardian = spawn_fixture();
    let registration = register_live_mpv(unrelated_actual_pid.id(), guardian.id()).unwrap();

    kill_mpv_now();
    guardian
        .wait()
        .expect("out-of-band shutdown stops the guardian fixture");
    assert!(
        matches!(unrelated_actual_pid.try_wait(), Ok(None)),
        "owner registry must never signal the potentially reused actual pid"
    );

    let _ = unrelated_actual_pid.kill();
    let _ = unrelated_actual_pid.wait();
    drop(registration);
    reset_media_registry_for_test();
}

#[tokio::test]
async fn shutdown_latch_wakes_independently_of_a_full_owner_queue() {
    let (owner_tx, _owner_rx) = tokio::sync::mpsc::channel(1);
    owner_tx.try_send(()).expect("fill owner queue");

    let latch = ShutdownLatch::new();
    let waiter = latch.clone();
    let waiting = tokio::spawn(async move {
        waiter.wait().await;
    });
    tokio::task::yield_now().await;

    latch.trigger();
    tokio::time::timeout(std::time::Duration::from_secs(1), waiting)
        .await
        .expect("out-of-band latch must not wait for owner capacity")
        .expect("wait task must finish");
    assert!(latch.is_triggered());
    assert!(matches!(
        owner_tx.try_send(()),
        Err(tokio::sync::mpsc::error::TrySendError::Full(()))
    ));
}

#[tokio::test]
async fn shutdown_latch_wait_is_lost_wakeup_safe_and_monotonic() {
    let latch = ShutdownLatch::new();
    let wait_created_before_trigger = latch.wait();

    // An async-fn body is not polled until awaited. Triggering here exercises the edge in
    // which registration has not happened yet; the atomic pre-check must still complete it.
    latch.trigger();
    tokio::time::timeout(
        std::time::Duration::from_secs(1),
        wait_created_before_trigger,
    )
    .await
    .expect("pre-registration trigger must be observed");

    latch.trigger();
    tokio::time::timeout(std::time::Duration::from_secs(1), latch.wait())
        .await
        .expect("future waits stay ready after repeated triggers");
}

#[test]
fn v2_lifeline_retains_a_live_exact_app_identity() {
    let dir = temp_dir("register");
    std::fs::create_dir_all(&dir).unwrap();
    let path = registry_path(&dir);
    let marker = "00112233445566778899aabbccddeeff";
    append_lifeline(
        path.clone(),
        Lifeline {
            app_pid: std::process::id(),
            app_started_at: current_process_started_at().unwrap(),
            mpv_pid: 999_999,
            mpv_socket: String::new(),
            identity_marker: marker.to_owned(),
            written_at: unix_now(),
        },
    )
    .unwrap();

    let text = safe_fs::read_to_string_no_symlink(&path).unwrap();
    let registry: LifelineRegistry = serde_json::from_str(&text).unwrap();
    assert_eq!(registry.version, 2);
    assert_eq!(registry.records.len(), 1);
    let record = &registry.records[0];
    assert_eq!(record.app_pid, std::process::id());
    assert_eq!(record.mpv_pid, 999_999);
    assert_eq!(record.identity_marker, marker);
    assert!(record.written_at <= unix_now());

    reap_orphans(&dir);
    assert!(path.exists(), "a live exact app identity is retained");
    let _ = std::fs::remove_dir_all(dir);
}

#[test]
fn reap_orphans_discards_corrupt_and_stale_lifelines_without_killing() {
    let dir = temp_dir("bad-records");
    std::fs::create_dir_all(&dir).unwrap();
    let path = registry_path(&dir);

    std::fs::write(&path, "{not json").unwrap();
    reap_orphans(&dir);
    assert!(!path.exists());

    let stale = Lifeline {
        app_pid: 999_991,
        app_started_at: 0,
        mpv_pid: 999_992,
        mpv_socket: "/tmp/stale.sock".to_owned(),
        identity_marker: String::new(),
        written_at: unix_now().saturating_sub(8 * 24 * 3600),
    };
    safe_fs::write_private_atomic_json(&path, &stale).unwrap();
    reap_orphans(&dir);
    assert!(
        !path.exists(),
        "stale lifeline is discarded before pid lookup"
    );

    let _ = std::fs::remove_dir_all(dir);
}

#[cfg(target_os = "linux")]
#[test]
fn legacy_identityless_record_never_kills_a_live_process() {
    use std::process::Stdio;

    let dir = temp_dir("legacy-no-authority");
    std::fs::create_dir_all(&dir).unwrap();
    let path = registry_path(&dir);
    let mut command =
        crate::util::process::std_command("sleep", crate::util::process::ProcessProfile::Media);
    command
        .arg("30")
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    let mut candidate = command.spawn().expect("spawn legacy candidate");
    safe_fs::write_private_atomic_json(
        &path,
        &Lifeline {
            app_pid: u32::MAX,
            app_started_at: 0,
            mpv_pid: candidate.id(),
            mpv_socket: "/tmp/predictable-legacy.sock".to_owned(),
            identity_marker: String::new(),
            written_at: unix_now(),
        },
    )
    .unwrap();

    reap_orphans(&dir);
    assert!(!path.exists(), "legacy record is migrated away");
    assert!(
        matches!(candidate.try_wait(), Ok(None)),
        "a markerless record must never authorize termination"
    );

    let _ = candidate.kill();
    let _ = candidate.wait();
    let _ = std::fs::remove_dir_all(dir);
}

#[cfg(target_os = "linux")]
#[test]
fn exact_marker_reaper_pins_kills_and_retains_until_gone() {
    use std::process::Stdio;
    use std::time::{Duration, Instant};
    use sysinfo::{Pid, ProcessRefreshKind, ProcessStatus, ProcessesToUpdate, System, UpdateKind};

    struct ExactMarkerFixture {
        candidate: std::process::Child,
        dir: std::path::PathBuf,
    }

    impl Drop for ExactMarkerFixture {
        fn drop(&mut self) {
            let _ = self.candidate.kill();
            let _ = self.candidate.wait();
            let _ = std::fs::remove_dir_all(&self.dir);
        }
    }

    let dir = temp_dir("exact-reaper");
    std::fs::create_dir_all(&dir).unwrap();
    let path = registry_path(&dir);
    let marker = "0123456789abcdef0123456789abcdef";
    let marker_arg = format!("--script-opts-append=yututui-lifeline={marker}");

    // bash's exec -a gives a single-process fixture whose argv contains the exact marker. The
    // replacement shell stops itself with a builtin, avoiding a helper process and avoiding
    // coreutils implementations which dispatch on argv[0] and reject the marker as a name.
    let mut command =
        crate::util::process::std_command("bash", crate::util::process::ProcessProfile::Media);
    command
        .arg("-c")
        .arg("exec -a \"$0\" bash -c 'kill -STOP $$'")
        .arg(&marker_arg)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    let candidate = command.spawn().expect("spawn exact-marker candidate");
    let mut fixture = ExactMarkerFixture { candidate, dir };
    let record = Lifeline {
        app_pid: u32::MAX,
        app_started_at: 1,
        mpv_pid: fixture.candidate.id(),
        mpv_socket: String::new(),
        identity_marker: marker.to_owned(),
        written_at: unix_now(),
    };

    // Bash can expose the final marker before it reaches the stopping builtin. A recovery pass
    // in that window can conservatively retain a transiently unavailable argv, so wait until
    // procfs reports both the exact marker and the final stopped state.
    let identity_deadline = Instant::now() + Duration::from_secs(2);
    let pid = Pid::from_u32(record.mpv_pid);
    let mut system = System::new();
    let identity_ready = loop {
        system.refresh_processes_specifics(
            ProcessesToUpdate::Some(&[pid]),
            true,
            ProcessRefreshKind::nothing()
                .with_cmd(UpdateKind::Always)
                .without_tasks(),
        );
        if system.process(pid).is_some_and(|process| {
            matches!(
                process.status(),
                ProcessStatus::Stop | ProcessStatus::Tracing
            ) && exact_identity(process, &record) == ExactIdentity::Match
        }) {
            break true;
        }
        if Instant::now() >= identity_deadline {
            break false;
        }
        std::thread::sleep(Duration::from_millis(10));
    };
    assert!(
        identity_ready,
        "the exact-marker fixture must expose its final stopped argv before recovery"
    );

    append_lifeline(path.clone(), record).unwrap();

    reap_orphans(&fixture.dir);
    assert!(
        path.exists(),
        "a signalled process stays recorded until a later observation proves it gone"
    );

    let deadline = Instant::now() + Duration::from_secs(5);
    let exited = loop {
        match fixture.candidate.try_wait() {
            Ok(Some(_)) => break true,
            Ok(None) if Instant::now() < deadline => {
                std::thread::sleep(Duration::from_millis(10));
            }
            Ok(None) | Err(_) => break false,
        }
    };
    assert!(exited, "the pinned exact-marker target must be terminated");

    reap_orphans(&fixture.dir);
    assert!(
        !path.exists(),
        "the next recovery pass removes a record after the target is gone"
    );
}

#[test]
fn only_a_full_random_marker_grants_exact_identity() {
    let legacy = Lifeline {
        app_pid: 1,
        app_started_at: 0,
        mpv_pid: 2,
        mpv_socket: String::new(),
        identity_marker: String::new(),
        written_at: unix_now(),
    };
    assert!(!mpv_identity_matches_args(&[], &legacy));

    let record = Lifeline {
        app_pid: 1,
        app_started_at: 0,
        mpv_pid: 2,
        mpv_socket: "/tmp/ytm-ipc-abc.sock".to_owned(),
        identity_marker: String::new(),
        written_at: unix_now(),
    };
    assert!(!mpv_identity_matches_args(&[], &record));
    assert!(!mpv_identity_matches_args(
        &["mpv".to_owned(), "--idle=yes".to_owned()],
        &record
    ));
    assert!(!mpv_identity_matches_args(
        &[
            "mpv".to_owned(),
            "--input-ipc-server=/tmp/ytm-ipc-abc.sock".to_owned()
        ],
        &record
    ));

    let exact = Lifeline {
        app_pid: 1,
        app_started_at: 1,
        mpv_pid: 2,
        mpv_socket: String::new(),
        identity_marker: "00112233445566778899aabbccddeeff".to_owned(),
        written_at: 0,
    };
    assert!(mpv_identity_matches_args(
        &["--script-opts-append=yututui-lifeline=00112233445566778899aabbccddeeff".to_owned()],
        &exact,
    ));
    assert!(!mpv_identity_matches_args(
        &["--script-opts-append=yututui-lifeline=other".to_owned()],
        &exact,
    ));
    assert!(!mpv_identity_matches_args(
        &["--script-opts-append=yututui-lifeline=00112233445566778899aabbccddeeff0".to_owned()],
        &exact,
    ));
    assert!(!valid_identity_marker("00112233445566778899aabbccddeefg"));
    assert!(!valid_identity_marker("00112233445566778899aabbccddee"));
}

#[test]
fn v2_registry_keeps_multiple_media_records() {
    let dir = temp_dir("multi-record");
    std::fs::create_dir_all(&dir).unwrap();
    let path = registry_path(&dir);
    let record = |pid, marker: &str| Lifeline {
        app_pid: 101,
        app_started_at: 102,
        mpv_pid: pid,
        mpv_socket: String::new(),
        identity_marker: marker.to_owned(),
        written_at: unix_now(),
    };
    append_lifeline(
        path.clone(),
        record(201, "00112233445566778899aabbccddeeff"),
    )
    .unwrap();
    append_lifeline(
        path.clone(),
        record(202, "ffeeddccbbaa99887766554433221100"),
    )
    .unwrap();

    let records = read_lifeline_records(&path).unwrap();
    assert_eq!(records.len(), 2);
    assert!(records.iter().any(|record| record.mpv_pid == 201));
    assert!(records.iter().any(|record| record.mpv_pid == 202));
    let _ = std::fs::remove_dir_all(dir);
}

#[test]
fn exact_record_drop_preserves_other_guardian_and_legacy_migrates() {
    let dir = temp_dir("drop-one");
    std::fs::create_dir_all(&dir).unwrap();
    let path = registry_path(&dir);
    let legacy = Lifeline {
        app_pid: 10,
        app_started_at: 0,
        mpv_pid: 20,
        mpv_socket: "/tmp/legacy.sock".to_owned(),
        identity_marker: String::new(),
        written_at: unix_now(),
    };
    safe_fs::write_private_atomic_json(&path, &legacy).unwrap();

    let exact = |pid, marker: &str| Lifeline {
        app_pid: 30,
        app_started_at: 40,
        mpv_pid: pid,
        mpv_socket: String::new(),
        identity_marker: marker.to_owned(),
        written_at: unix_now(),
    };
    let marker_a = "00112233445566778899aabbccddeeff";
    let marker_b = "ffeeddccbbaa99887766554433221100";
    append_lifeline(path.clone(), exact(50, marker_a)).unwrap();
    append_lifeline(path.clone(), exact(51, marker_b)).unwrap();
    assert_eq!(read_lifeline_records(&path).unwrap().len(), 3);

    drop(DiskLifelineRegistration {
        path: path.clone(),
        app_pid: 30,
        mpv_pid: 50,
        identity_marker: marker_a.to_owned(),
    });
    let records = read_lifeline_records(&path).unwrap();
    assert_eq!(records.len(), 2);
    assert!(records.iter().any(|record| record.mpv_pid == 20));
    assert!(records.iter().any(|record| record.mpv_pid == 51));

    drop(DiskLifelineRegistration {
        path: path.clone(),
        app_pid: 30,
        mpv_pid: 51,
        identity_marker: marker_b.to_owned(),
    });
    let records = read_lifeline_records(&path).unwrap();
    assert_eq!(records.len(), 1);
    assert_eq!(records[0].mpv_pid, 20);
    let _ = std::fs::remove_dir_all(dir);
}

#[test]
fn lifeline_append_reports_durable_write_setup_failure() {
    let dir = temp_dir("write-failure");
    std::fs::create_dir_all(&dir).unwrap();
    let non_directory = dir.join("not-a-directory");
    std::fs::write(&non_directory, b"block child creation").unwrap();
    let record = Lifeline {
        app_pid: 1,
        app_started_at: 2,
        mpv_pid: 3,
        mpv_socket: String::new(),
        identity_marker: "00112233445566778899aabbccddeeff".to_owned(),
        written_at: unix_now(),
    };
    assert!(append_lifeline(non_directory.join("registry.json"), record).is_err());
    let _ = std::fs::remove_dir_all(dir);
}
