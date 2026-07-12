#[cfg(any(unix, windows))]
use std::fs;
#[cfg(unix)]
use std::os::unix::fs::PermissionsExt;
#[cfg(unix)]
use std::sync::Mutex;
#[cfg(any(unix, windows))]
use std::time::{SystemTime, UNIX_EPOCH};

use super::*;

#[cfg(unix)]
fn import_claim(
    session_id: &str,
    row_id: &str,
    source_order: u32,
    expected_key: &str,
) -> crate::transfer::artifact_identity::ImportDownloadClaim {
    crate::transfer::artifact_identity::test_import_claim(
        session_id,
        row_id,
        source_order,
        expected_key,
    )
}

fn test_download_handle(tx: Sender<AdmittedDownloadCmd>) -> DownloadHandle {
    let (cancel, _cancel_rx) = watch::channel(false);
    let (_done, done_rx) = watch::channel(false);
    DownloadHandle {
        tx,
        pending: Arc::new(SyncMutex::new(PendingDownloadCommands::default())),
        outstanding: Arc::new(OutstandingDownloads::default()),
        cancel,
        done: done_rx,
    }
}

async fn supervise_test_download_actor(
    actor: tokio::task::JoinHandle<()>,
    handle: &DownloadHandle,
    sink: EventSink,
    done: watch::Sender<bool>,
) {
    let terminal_outbox = Arc::new(TerminalOutbox::default());
    let cancel = handle.cancel.subscribe();
    terminal_outbox::supervise(TerminalSupervisor {
        actor,
        pending: Arc::clone(&handle.pending),
        outstanding: Arc::clone(&handle.outstanding),
        done_tx: done,
        outbox: terminal_outbox,
        emit: sink,
        cancel,
    })
    .await;
}

#[cfg(unix)]
fn accepted_event_sink() -> EventSink {
    Arc::new(|_| Ok(DeliveryReceipt::Enqueued))
}

#[cfg(unix)]
fn save_import_download_session(request: &ImportDownloadRequest) {
    use crate::transfer::session::{ImportSession, ImportSessionRow, ImportSessionRowStatus};

    ImportSession {
        session_id: request.session_id.clone(),
        session_instance_id: request.claim.session_instance_id.clone(),
        rows: vec![ImportSessionRow {
            row_id: request.row_id.clone(),
            source_order: request.source_order,
            status: ImportSessionRowStatus::Matched,
            title: request.song.title.clone(),
            artists: vec![request.song.artist.clone()],
            source_key: "spotify:track:download-fixture".to_owned(),
            selected_key: Some(request.claim.expected_key.clone()),
            download_claim: Some(request.claim.clone()),
            ..ImportSessionRow::default()
        }],
        ..ImportSession::default()
    }
    .save()
    .expect("save import download fixture session");
}

#[test]
fn parses_progress_lines() {
    assert_eq!(parse_percent("download:  45.3%"), Some(45.3));
    assert_eq!(parse_percent("download:100.0%"), Some(100.0));
    assert_eq!(parse_percent("download:   0.0%"), Some(0.0));
}

#[test]
fn ignores_non_progress_lines() {
    assert_eq!(parse_percent("[download] Destination: foo.m4a"), None);
    assert_eq!(parse_percent("download:n/a"), None);
    assert_eq!(parse_percent(""), None);
}

#[test]
fn audio_profiles_map_to_expected_ytdlp_args() {
    assert_eq!(
        DownloadAudioProfile::CompatibleM4a.ytdlp_args(),
        [
            "-f",
            "bestaudio",
            "-x",
            "--audio-format",
            "m4a",
            "--audio-quality",
            "0"
        ]
    );
    assert_eq!(
        DownloadAudioProfile::PreferNativeM4a.ytdlp_args(),
        [
            "-f",
            "ba[ext=m4a]/ba",
            "-x",
            "--audio-format",
            "m4a",
            "--audio-quality",
            "0"
        ]
    );
    assert_eq!(
        DownloadAudioProfile::KeepNative.ytdlp_args(),
        ["-f", "bestaudio", "-x", "--audio-format", "best"]
    );
}

#[test]
fn output_template_embeds_id_and_has_no_subdirs() {
    // The id tag is what `Song::local_file` recovers on a rescan, so it must be present.
    assert_eq!(OUTPUT_TEMPLATE, "%(title)s [%(id)s].%(ext)s");
    assert!(OUTPUT_TEMPLATE.contains("[%(id)s]"));
    assert!(!OUTPUT_TEMPLATE.contains('/'));
    assert!(!OUTPUT_TEMPLATE.contains('\\'));
}

#[cfg(windows)]
fn windows_job_test_root(label: &str) -> PathBuf {
    std::env::temp_dir().join(format!(
        "ytt-{label}-{}-{}",
        std::process::id(),
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ))
}

#[cfg(windows)]
fn windows_job_test_command(started: &Path, survived: &Path) -> tokio::process::Command {
    const PARENT_TEST: &str = "download::tests::windows_job_parent_helper";
    let mut cmd = tokio::process::Command::new(std::env::current_exe().unwrap());
    cmd.args(["--ignored", "--exact", PARENT_TEST, "--test-threads=1"])
        .env("YTT_JOB_TEST_STARTED", started)
        .env("YTT_JOB_TEST_SURVIVED", survived)
        .env("YTT_JOB_TEST_SURVIVE_DELAY_MS", "1500")
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .kill_on_drop(true);
    cmd
}

#[cfg(windows)]
async fn windows_job_assignment_supported() -> bool {
    let root = windows_job_test_root("job-support-probe");
    let started = root.join("grandchild.started");
    let survived = root.join("grandchild.survived");
    fs::create_dir_all(&root).unwrap();
    let mut child = windows_job_test_command(&started, &survived)
        .spawn()
        .expect("spawn Windows Job Object support probe");
    let mut guard = ChildTreeGuard::for_tokio(&child, crate::util::process::ProcessProfile::YtDlp);
    let owns_job = guard.owns_job();
    guard.terminate();
    let _ = tokio::time::timeout(Duration::from_secs(5), child.wait()).await;
    let _ = fs::remove_dir_all(root);
    owns_job
}

#[cfg(windows)]
#[tokio::test]
async fn windows_process_guard_terminates_the_ytdlp_child_tree() {
    let root = windows_job_test_root("download-job");
    let started = root.join("grandchild.started");
    let survived = root.join("grandchild.survived");
    let _ = fs::remove_dir_all(&root);
    fs::create_dir_all(&root).unwrap();

    let mut cmd = windows_job_test_command(&started, &survived);
    let mut child = cmd.spawn().expect("spawn inert Windows child tree");
    let mut guard = ChildTreeGuard::for_tokio(&child, crate::util::process::ProcessProfile::YtDlp);
    let owns_job = guard.owns_job();

    tokio::time::timeout(Duration::from_secs(5), async {
        while !started.exists() {
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .expect("Windows grandchild did not start");

    guard.terminate();

    let _status = tokio::time::timeout(Duration::from_secs(5), child.wait())
        .await
        .expect("Windows child did not exit after closing its Job Object")
        .expect("wait for terminated Windows child");

    tokio::time::sleep(Duration::from_millis(2_000)).await;
    if owns_job {
        assert!(
            !survived.exists(),
            "the inherited grandchild outlived its kill-on-close Job Object"
        );
    } else {
        assert!(
            survived.exists(),
            "without a Job Object only the documented direct-child fallback is guaranteed"
        );
    }
    let _ = fs::remove_dir_all(root);
}

#[cfg(windows)]
#[tokio::test]
async fn windows_tokio_output_timeout_terminates_the_ytdlp_child_tree() {
    let owns_job = windows_job_assignment_supported().await;
    let root = windows_job_test_root("tokio-output-timeout-job");
    let started = root.join("grandchild.started");
    let survived = root.join("grandchild.survived");
    fs::create_dir_all(&root).unwrap();

    let mut cmd = windows_job_test_command(&started, &survived);
    cmd.stdout(Stdio::piped()).stderr(Stdio::piped());
    let result = crate::util::process::tokio_output_limited(
        cmd,
        crate::util::process::ProcessProfile::YtDlp,
        Duration::from_secs(1),
        128,
    )
    .await;
    assert!(result.is_err(), "the child tree should reach the timeout");
    assert!(started.exists(), "the grandchild fixture must have started");

    tokio::time::sleep(Duration::from_millis(2_000)).await;
    assert_eq!(
        survived.exists(),
        !owns_job,
        "timeout cleanup must kill the tree with a Job Object and document direct-child fallback"
    );
    let _ = fs::remove_dir_all(root);
}

#[cfg(windows)]
#[tokio::test]
async fn windows_dropped_tokio_output_terminates_the_ytdlp_child_tree() {
    let owns_job = windows_job_assignment_supported().await;
    let root = windows_job_test_root("tokio-output-cancel-job");
    let started = root.join("grandchild.started");
    let survived = root.join("grandchild.survived");
    fs::create_dir_all(&root).unwrap();

    let mut cmd = windows_job_test_command(&started, &survived);
    cmd.stdout(Stdio::piped()).stderr(Stdio::piped());
    let task = tokio::spawn(crate::util::process::tokio_output_limited(
        cmd,
        crate::util::process::ProcessProfile::YtDlp,
        Duration::from_secs(30),
        128,
    ));
    tokio::time::timeout(Duration::from_secs(5), async {
        while !started.exists() {
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .expect("Windows grandchild did not start before cancellation");

    task.abort();
    assert!(matches!(task.await, Err(error) if error.is_cancelled()));
    tokio::time::sleep(Duration::from_millis(2_000)).await;
    assert_eq!(
        survived.exists(),
        !owns_job,
        "drop cleanup must kill the tree with a Job Object and document direct-child fallback"
    );
    let _ = fs::remove_dir_all(root);
}

#[cfg(windows)]
#[test]
#[ignore = "subprocess fixture for the Windows Job Object test"]
fn windows_job_parent_helper() {
    const GRANDCHILD_TEST: &str = "download::tests::windows_job_grandchild_helper";
    if std::env::var_os("YTT_JOB_TEST_STARTED").is_none() {
        return;
    }
    // Give the owner enough time to assign this process before it creates descendants. Windows
    // then automatically places the grandchild in the same job.
    std::thread::sleep(Duration::from_millis(250));
    let mut grandchild = std::process::Command::new(std::env::current_exe().unwrap())
        .args(["--ignored", "--exact", GRANDCHILD_TEST, "--test-threads=1"])
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .expect("spawn Windows Job Object grandchild fixture");
    let _ = grandchild.wait();
}

#[cfg(windows)]
#[test]
#[ignore = "subprocess fixture for the Windows Job Object test"]
fn windows_job_grandchild_helper() {
    let Some(started) = std::env::var_os("YTT_JOB_TEST_STARTED") else {
        return;
    };
    let Some(survived) = std::env::var_os("YTT_JOB_TEST_SURVIVED") else {
        return;
    };
    fs::write(started, b"started").unwrap();
    let delay = std::env::var("YTT_JOB_TEST_SURVIVE_DELAY_MS")
        .ok()
        .and_then(|value| value.parse().ok())
        .unwrap_or(1_000);
    std::thread::sleep(Duration::from_millis(delay));
    fs::write(survived, b"survived").unwrap();
}

#[tokio::test(flavor = "current_thread")]
async fn admission_backlog_is_bounded_and_closed_is_explicit() {
    let (tx, rx) = tokio::sync::mpsc::channel(1);
    let handle = test_download_handle(tx);
    handle
        .set_dir(PathBuf::from("/tmp/one"))
        .expect("first set-dir should fill the small channel");
    for index in 0..DOWNLOAD_ADMISSION_BACKLOG {
        handle
            .start(Song::remote(
                format!("id-{index}"),
                "title",
                "artist",
                "1:00",
            ))
            .expect("bounded spill should accept up to its declared capacity");
    }
    assert!(
        handle
            .start(Song::remote("overflow", "title", "artist", "1:00"))
            .is_err(),
        "one request beyond the bounded spill must report Busy"
    );
    assert_eq!(
        handle.outstanding.snapshot().len(),
        DOWNLOAD_ADMISSION_BACKLOG,
        "rejected work must be rolled back from ownership"
    );
    drop(rx);
    let closed = handle
        .set_dir(PathBuf::from("/tmp/three"))
        .expect_err("closed queue should be reported");
    assert!(matches!(closed, DownloadSetDirError::Closed(_)));
    assert!(
        handle
            .start(Song::remote("closed", "title", "artist", "1:00"))
            .is_err()
    );
    assert_eq!(
        handle.outstanding.snapshot().len(),
        DOWNLOAD_ADMISSION_BACKLOG
    );
}

#[tokio::test(flavor = "current_thread")]
async fn set_dir_retries_and_merges_latest_without_download_overtaking() {
    let (tx, mut rx) = tokio::sync::mpsc::channel(1);
    let handle = test_download_handle(tx);
    handle.set_dir(PathBuf::from("/tmp/one")).unwrap();
    handle.set_dir(PathBuf::from("/tmp/two")).unwrap();
    handle
        .start(Song::remote("id", "title", "artist", "1:00"))
        .expect("a start behind deferred SetDir must be retained");
    handle.set_dir(PathBuf::from("/tmp/three")).unwrap();
    handle.set_dir(PathBuf::from("/tmp/four")).unwrap();

    assert!(matches!(
        rx.recv().await.map(|cmd| cmd.command),
        Some(DownloadCmd::SetDir(dir)) if dir == Path::new("/tmp/one")
    ));
    assert!(matches!(
        rx.recv().await.map(|cmd| cmd.command),
        Some(DownloadCmd::SetDir(dir)) if dir == Path::new("/tmp/two")
    ));
    assert!(matches!(
        rx.recv().await.map(|cmd| cmd.command),
        Some(DownloadCmd::Start(song)) if song.video_id == "id"
    ));
    assert!(matches!(
        rx.recv().await.map(|cmd| cmd.command),
        Some(DownloadCmd::SetDir(dir)) if dir == Path::new("/tmp/four")
    ));
    tokio::task::yield_now().await;
    assert!(
        rx.try_recv().is_err(),
        "the replaced directory must not leak"
    );
}

#[tokio::test(flavor = "current_thread")]
async fn saturated_actor_and_spill_commands_fail_terminally_when_receiver_closes() {
    let (tx, rx) = tokio::sync::mpsc::channel(1);
    let handle = test_download_handle(tx);
    for _ in 0..3 {
        handle
            .start(Song::remote("duplicate-id", "title", "artist", "1:00"))
            .expect("actor lane and bounded spill should accept the request");
    }
    assert_eq!(handle.outstanding.snapshot().len(), 3);

    let events = Arc::new(SyncMutex::new(Vec::new()));
    let captured = Arc::clone(&events);
    let sink: EventSink = Arc::new(move |event| {
        captured.lock().unwrap().push(event);
        Ok(DeliveryReceipt::Enqueued)
    });
    let actor = tokio::spawn(async move { drop(rx) });
    let (done, _done_rx) = watch::channel(false);
    supervise_test_download_actor(actor, &handle, sink, done).await;

    let events = events.lock().unwrap();
    assert_eq!(
        events
            .iter()
            .filter(|event| matches!(event, DownloadEvent::Error { video_id, .. } if video_id == "duplicate-id"))
            .count(),
        3,
        "every accepted request must keep its own terminal correlation"
    );
    assert!(handle.outstanding.snapshot().is_empty());
}

#[tokio::test(flavor = "current_thread")]
async fn panicked_actor_fails_each_duplicate_admission_exactly_once() {
    let (tx, rx) = tokio::sync::mpsc::channel(4);
    let handle = test_download_handle(tx);
    handle
        .start(Song::remote("already-completed", "title", "artist", "1:00"))
        .unwrap();
    let completed_admission = handle.outstanding.snapshot()[0].0;
    assert!(handle.outstanding.retire(completed_admission));
    for _ in 0..2 {
        handle
            .start(Song::remote("duplicate-id", "title", "artist", "1:00"))
            .unwrap();
    }

    let events = Arc::new(SyncMutex::new(Vec::new()));
    let captured = Arc::clone(&events);
    let sink: EventSink = Arc::new(move |event| {
        captured.lock().unwrap().push(event);
        Ok(DeliveryReceipt::Enqueued)
    });
    let actor = tokio::spawn(async move {
        let _rx = rx;
        panic!("deterministic actor panic")
    });
    let (done, _done_rx) = watch::channel(false);
    supervise_test_download_actor(actor, &handle, sink, done).await;

    let events = events.lock().unwrap();
    assert_eq!(events.len(), 2);
    assert!(events.iter().all(
        |event| matches!(event, DownloadEvent::Error { video_id, .. } if video_id == "duplicate-id")
    ));
    assert!(!events.iter().any(
        |event| matches!(event, DownloadEvent::Error { video_id, .. } if video_id == "already-completed")
    ));
    assert!(handle.outstanding.snapshot().is_empty());
}

#[tokio::test]
async fn terminal_retries_the_same_payload_after_saturated_delivery() {
    use std::sync::atomic::{AtomicUsize, Ordering};

    let attempts = Arc::new(AtomicUsize::new(0));
    let sink_attempts = Arc::clone(&attempts);
    let accepted = Arc::new(SyncMutex::new(None));
    let sink_accepted = Arc::clone(&accepted);
    let sink: EventSink = Arc::new(move |event| {
        if sink_attempts.fetch_add(1, Ordering::SeqCst) == 0 {
            Err(DeliveryError::Saturated)
        } else {
            *sink_accepted.lock().unwrap() = Some(event);
            Ok(DeliveryReceipt::Enqueued)
        }
    });
    let delivered = tokio::time::timeout(Duration::from_secs(1), async {
        emit_terminal(
            &sink,
            DownloadEvent::Done {
                video_id: "retried-terminal".to_owned(),
                path: "song.m4a".to_owned(),
            },
            &DownloadCancellation::never(),
        )
        .await
    })
    .await
    .expect("terminal retry timed out");

    assert!(delivered);
    assert_eq!(attempts.load(Ordering::SeqCst), 2);
    assert!(matches!(
        accepted.lock().unwrap().as_ref(),
        Some(DownloadEvent::Done { video_id, path })
            if video_id == "retried-terminal" && path == "song.m4a"
    ));
}

#[test]
fn outstanding_download_owner_state_has_a_hard_cap() {
    let outstanding = OutstandingDownloads::default();
    for index in 0..ownership::DOWNLOAD_OUTSTANDING_MAX {
        assert!(
            outstanding
                .register(DownloadOwner::Ordinary(format!("video-{index}")))
                .is_some(),
            "entry {index} should fit within the declared owner-state cap"
        );
    }
    assert!(
        outstanding
            .register(DownloadOwner::Ordinary("overflow".to_owned()))
            .is_none(),
        "owner state must reject work beyond its hard cap"
    );
    assert_eq!(
        outstanding.snapshot().len(),
        ownership::DOWNLOAD_OUTSTANDING_MAX
    );
}

#[cfg(unix)]
#[tokio::test]
async fn permanent_terminal_saturation_does_not_hold_download_permit() {
    let root = temp_dir("download-terminal-saturation-permit");
    let bin_dir = root.join("bin");
    let download_dir = root.join("music");
    fs::create_dir_all(&bin_dir).unwrap();
    let saved = download_dir.join("Track [shared].m4a");
    let starts = root.join("starts");
    let fake = write_executable(
        &bin_dir,
        "yt-dlp",
        &format!(
            "#!/bin/sh\nprintf x >> '{starts}'\nprintf audio > '{saved}'\nprintf '%s\\n' '{saved}'\n",
            starts = starts.display(),
            saved = saved.display()
        ),
    );
    let handle = spawn_with_program(
        |_| Err(DeliveryError::Saturated),
        download_dir,
        None,
        1,
        fake.to_string_lossy().into_owned(),
    );
    handle
        .start(Song::remote("first", "Track", "Artist", "1:00"))
        .unwrap();
    handle
        .start(Song::remote("second", "Track", "Artist", "1:00"))
        .unwrap();

    tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            if fs::read(&starts).is_ok_and(|bytes| bytes.len() >= 2) {
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("the second job must acquire the only permit while the first terminal is saturated");

    assert!(handle.shutdown().await);
    let _ = fs::remove_dir_all(root);
}

#[cfg(unix)]
#[tokio::test]
async fn run_download_uses_fake_ytdlp_progress_and_final_path() {
    let root = temp_dir("download");
    let bin_dir = root.join("bin");
    let download_dir = root.join("music");
    fs::create_dir_all(&bin_dir).unwrap();
    let saved = download_dir.join("Track [abc123def45].m4a");
    let fake = write_executable(
        &bin_dir,
        "yt-dlp",
        // Create a real (non-empty) file at the reported path, as yt-dlp would — the actor
        // now validates the file exists/non-empty/in-dir before reporting `Done`.
        &format!(
            "#!/bin/sh\nprintf 'download:  12.4%%\\n' >&2\nprintf 'download:  99.6%%\\n' >&2\nprintf 'audio' > '{saved}'\nprintf '%s\\n' '{saved}'\n",
            saved = saved.display()
        ),
    );

    let events = Arc::new(Mutex::new(Vec::new()));
    let captured = events.clone();
    let emit: EventSink = Arc::new(move |event| {
        captured.lock().unwrap().push(event);
        Ok(DeliveryReceipt::Enqueued)
    });

    run_download_with_program(
        fake.to_str().unwrap(),
        &Song::remote("abc123def45", "Track", "Artist", "3:12"),
        &download_dir,
        None,
        &emit,
    )
    .await
    .unwrap();

    let events = events.lock().unwrap();
    assert_eq!(events.len(), 3);
    match &events[0] {
        DownloadEvent::Progress { video_id, percent } => {
            assert_eq!(video_id, "abc123def45");
            assert_eq!(*percent, 12.0);
        }
        _ => panic!("expected first progress event"),
    }
    match &events[1] {
        DownloadEvent::Progress { video_id, percent } => {
            assert_eq!(video_id, "abc123def45");
            assert_eq!(*percent, 100.0);
        }
        _ => panic!("expected second progress event"),
    }
    match &events[2] {
        DownloadEvent::Done { video_id, path } => {
            assert_eq!(video_id, "abc123def45");
            assert_eq!(path.as_str(), saved.to_string_lossy().as_ref());
        }
        _ => panic!("expected done event"),
    }
    let sidecar = crate::downloads::read_sidecar(&saved)
        .unwrap()
        .expect("download should write metadata sidecar");
    assert_eq!(sidecar.title, "Track");
    assert_eq!(sidecar.artist, "Artist");
    assert_eq!(sidecar.linked_youtube_id(), Some("abc123def45"));

    let _ = fs::remove_dir_all(root);
}

#[cfg(unix)]
#[tokio::test]
async fn run_import_download_moves_from_incoming_to_complete() {
    let root = temp_dir("download-import");
    let bin_dir = root.join("bin");
    let download_dir = root.join("music");
    fs::create_dir_all(&bin_dir).unwrap();
    let fixture_audio = root.join("fixture.wav");
    write_test_wav(&fixture_audio);

    let incoming = download_dir
        .join(".yututui-inbox")
        .join("sp2yt-import")
        .join("incoming")
        .join("Track [abc123def45].wav");
    let complete = download_dir
        .join(".yututui-inbox")
        .join("sp2yt-import")
        .join("complete")
        .join("Track [abc123def45].wav");
    let fake = write_executable(
        &bin_dir,
        "yt-dlp",
        &format!(
            "#!/bin/sh\nprintf 'download: 100.0%%\\n' >&2\ncp '{fixture}' '{incoming}'\nprintf '%s\\n' '{incoming}'\n",
            fixture = fixture_audio.display(),
            incoming = incoming.display(),
        ),
    );
    let events = Arc::new(Mutex::new(Vec::new()));
    let captured = events.clone();
    let emit: EventSink = Arc::new(move |event| {
        captured.lock().unwrap().push(event);
        Ok(DeliveryReceipt::Enqueued)
    });
    let request = ImportDownloadRequest {
        session_id: "sp2yt-import".to_owned(),
        row_id: "row-00001".to_owned(),
        source_order: 1,
        claim: import_claim("sp2yt-import", "row-00001", 1, "abc123def45"),
        song: Song::remote("abc123def45", "Track", "Artist", "3:12")
            .with_import_session(Some("sp2yt-import".to_owned()), Some(1)),
    };
    save_import_download_session(&request);

    run_import_download_with_program(fake.to_str().unwrap(), &request, &download_dir, None, &emit)
        .await
        .unwrap();

    assert!(!incoming.exists());
    assert!(complete.exists());
    assert!(crate::downloads::sidecar_path(&complete).exists());
    assert!(!crate::downloads::sidecar_path(&incoming).exists());
    let events = events.lock().unwrap();
    assert!(events.iter().any(|event| {
        matches!(
            event,
            DownloadEvent::ImportDone { context, path }
                if context.video_id == "abc123def45"
                    && context.session_id == "sp2yt-import"
                    && context.row_id == "row-00001"
                    && path == complete.to_string_lossy().as_ref()
        )
    }));
    let session = crate::transfer::session::ImportSession::load("sp2yt-import").unwrap();
    assert!(session.rows[0].written);
    assert_eq!(
        session.rows[0].local_path.as_deref(),
        Some(complete.as_path())
    );

    let _ = crate::transfer::session::ImportSession::delete_record("sp2yt-import");
    let _ = fs::remove_dir_all(root);
}

#[cfg(unix)]
#[tokio::test]
async fn same_video_imports_complete_independently_for_two_sessions() {
    let root = temp_dir("download-import-same-video-two-sessions");
    let bin_dir = root.join("bin");
    let download_dir = root.join("music");
    fs::create_dir_all(&bin_dir).unwrap();
    let fixture_audio = root.join("fixture.wav");
    write_test_wav(&fixture_audio);
    let fake = write_executable(
        &bin_dir,
        "yt-dlp",
        &format!(
            "#!/bin/sh\ndir=''\nwhile [ \"$#\" -gt 0 ]; do\n  if [ \"$1\" = '-P' ]; then shift; dir=$1; fi\n  shift\ndone\nout=\"$dir/Track [abc123def45].wav\"\ncp '{fixture}' \"$out\"\nprintf '%s\\n' \"$out\"\n",
            fixture = fixture_audio.display(),
        ),
    );
    let request_a = ImportDownloadRequest {
        session_id: "sp2yt-same-a".to_owned(),
        row_id: "row-00001".to_owned(),
        source_order: 1,
        claim: import_claim("sp2yt-same-a", "row-00001", 1, "abc123def45"),
        song: Song::remote("abc123def45", "Track A", "Artist A", "3:12")
            .with_import_session(Some("sp2yt-same-a".to_owned()), Some(1)),
    };
    let request_b = ImportDownloadRequest {
        session_id: "sp2yt-same-b".to_owned(),
        row_id: "row-00007".to_owned(),
        source_order: 7,
        claim: import_claim("sp2yt-same-b", "row-00007", 7, "abc123def45"),
        song: Song::remote("abc123def45", "Track B", "Artist B", "3:12")
            .with_import_session(Some("sp2yt-same-b".to_owned()), Some(7)),
    };
    save_import_download_session(&request_a);
    save_import_download_session(&request_b);

    let events = Arc::new(Mutex::new(Vec::new()));
    let captured = Arc::clone(&events);
    let import_done = Arc::new(tokio::sync::Notify::new());
    let emitted_import_done = Arc::clone(&import_done);
    let handle = spawn_with_program(
        move |event| {
            let is_import_done = matches!(&event, DownloadEvent::ImportDone { .. });
            captured.lock().unwrap().push(event);
            if is_import_done {
                emitted_import_done.notify_one();
            }
            Ok(DeliveryReceipt::Enqueued)
        },
        download_dir.clone(),
        None,
        2,
        fake.to_string_lossy().into_owned(),
    );
    handle.start_for_import(request_a.clone()).unwrap();
    handle.start_for_import(request_b.clone()).unwrap();

    let completion = tokio::time::timeout(Duration::from_secs(15), async {
        loop {
            let notified = import_done.notified();
            let done = events
                .lock()
                .unwrap()
                .iter()
                .filter(|event| matches!(event, DownloadEvent::ImportDone { .. }))
                .count();
            if done >= 2 {
                break;
            }
            notified.await;
        }
    })
    .await;
    if completion.is_err() {
        panic!(
            "both stable import owners must receive a terminal completion; captured events: {:#?}",
            events.lock().unwrap()
        );
    }
    assert!(handle.shutdown().await);

    let mut contexts: Vec<_> = events
        .lock()
        .unwrap()
        .iter()
        .filter_map(|event| match event {
            DownloadEvent::ImportDone { context, .. } => Some((
                context.session_id.clone(),
                context.row_id.clone(),
                context.source_order,
            )),
            _ => None,
        })
        .collect();
    // The terminal outbox deliberately drains a simultaneous pending batch by admission id, so
    // delivery order is not a process-completion contract. Independence means that each stable
    // import owner receives exactly one completion with its own identity.
    contexts.sort_unstable();
    assert_eq!(
        contexts,
        vec![
            ("sp2yt-same-a".to_owned(), "row-00001".to_owned(), 1),
            ("sp2yt-same-b".to_owned(), "row-00007".to_owned(), 7),
        ]
    );

    let saved_a = crate::transfer::session::ImportSession::load("sp2yt-same-a").unwrap();
    let saved_b = crate::transfer::session::ImportSession::load("sp2yt-same-b").unwrap();
    let path_a = saved_a.rows[0].local_path.as_ref().expect("session A path");
    let path_b = saved_b.rows[0].local_path.as_ref().expect("session B path");
    assert_ne!(path_a, path_b);
    assert!(path_a.is_file());
    assert!(path_b.is_file());

    let _ = crate::transfer::session::ImportSession::delete_record("sp2yt-same-a");
    let _ = crate::transfer::session::ImportSession::delete_record("sp2yt-same-b");
    let _ = fs::remove_dir_all(root);
}

#[cfg(unix)]
#[tokio::test]
async fn import_commit_survives_closed_done_owner_and_retry_does_not_redownload() {
    let root = temp_dir("download-import-closed-done");
    let bin_dir = root.join("bin");
    let download_dir = root.join("music");
    fs::create_dir_all(&bin_dir).unwrap();
    let fixture_audio = root.join("fixture.wav");
    write_test_wav(&fixture_audio);
    let session_id = "sp2yt-import-closed-done";
    let incoming = download_dir
        .join(".yututui-inbox")
        .join(session_id)
        .join("incoming")
        .join("Track [abc123def45].wav");
    let complete = download_dir
        .join(".yututui-inbox")
        .join(session_id)
        .join("complete")
        .join("Track [abc123def45].wav");
    let fake = write_executable(
        &bin_dir,
        "yt-dlp-first",
        &format!(
            "#!/bin/sh\ncp '{fixture}' '{incoming}'\nprintf '%s\\n' '{incoming}'\n",
            fixture = fixture_audio.display(),
            incoming = incoming.display(),
        ),
    );
    let request = ImportDownloadRequest {
        session_id: session_id.to_owned(),
        row_id: "row-00001".to_owned(),
        source_order: 1,
        claim: import_claim(session_id, "row-00001", 1, "abc123def45"),
        song: Song::remote("abc123def45", "Track", "Artist", "3:12")
            .with_import_session(Some(session_id.to_owned()), Some(1)),
    };
    save_import_download_session(&request);
    let closed_on_done: EventSink = Arc::new(|event| match event {
        DownloadEvent::ImportDone { .. } => Err(DeliveryError::Closed),
        _ => Ok(DeliveryReceipt::Enqueued),
    });

    let error = run_import_download_with_program(
        fake.to_str().unwrap(),
        &request,
        &download_dir,
        None,
        &closed_on_done,
    )
    .await
    .expect_err("a closed terminal owner must still be reported");
    assert!(format!("{error:#}").contains("completion owner closed"));
    assert!(!incoming.exists());
    assert!(complete.is_file());
    assert!(crate::downloads::sidecar_path(&complete).is_file());
    let saved = crate::transfer::session::ImportSession::load(session_id).unwrap();
    assert!(saved.rows[0].written);
    assert_eq!(
        saved.rows[0].local_path.as_deref(),
        Some(complete.as_path())
    );

    let retry_marker = root.join("retry-program-ran");
    let retry_program = write_executable(
        &bin_dir,
        "yt-dlp-retry",
        &format!(
            "#!/bin/sh\nprintf 'unexpected retry' > '{}'\nexit 19\n",
            retry_marker.display()
        ),
    );
    let retry_events = Arc::new(Mutex::new(Vec::new()));
    let captured = Arc::clone(&retry_events);
    let accepted: EventSink = Arc::new(move |event| {
        captured.lock().unwrap().push(event);
        Ok(DeliveryReceipt::Enqueued)
    });
    run_import_download_with_program(
        retry_program.to_str().unwrap(),
        &request,
        &download_dir,
        None,
        &accepted,
    )
    .await
    .expect("retry should reconcile the durable row before spawning yt-dlp");
    assert!(!retry_marker.exists(), "retry unexpectedly spawned yt-dlp");
    assert_eq!(
        fs::read_dir(complete.parent().unwrap())
            .unwrap()
            .filter_map(Result::ok)
            .filter(|entry| entry.file_type().is_ok_and(|kind| kind.is_file()))
            .count(),
        2,
        "retry must retain exactly one audio/sidecar pair"
    );
    assert!(retry_events.lock().unwrap().iter().any(|event| {
        matches!(event, DownloadEvent::ImportDone { path, .. } if path == complete.to_string_lossy().as_ref())
    }));

    let _ = crate::transfer::session::ImportSession::delete_record(session_id);
    let _ = fs::remove_dir_all(root);
}

#[test]
fn import_inbox_dirs_reject_traversal_session_ids() {
    assert!(import_inbox_dirs(Path::new("/tmp"), "../escape").is_err());
    assert!(import_inbox_dirs(Path::new("/tmp"), "UPPER").is_err());
    let dirs = import_inbox_dirs(Path::new("/tmp"), "sp2yt-safe-1").unwrap();
    assert_eq!(
        dirs.complete,
        PathBuf::from("/tmp/.yututui-inbox/sp2yt-safe-1/complete")
    );
}

#[cfg(unix)]
#[tokio::test]
async fn import_inbox_rejects_an_intermediate_symlink_before_ytdlp_runs() {
    use std::os::unix::fs::symlink;

    let root = temp_dir("download-import-inbox-symlink");
    let bin_dir = root.join("bin");
    let download_dir = root.join("music");
    let outside = root.join("outside");
    fs::create_dir_all(&bin_dir).unwrap();
    fs::create_dir_all(&download_dir).unwrap();
    fs::create_dir_all(&outside).unwrap();
    symlink(&outside, download_dir.join(".yututui-inbox")).unwrap();
    let ran = root.join("yt-dlp-ran");
    let fake = write_executable(
        &bin_dir,
        "yt-dlp",
        &format!("#!/bin/sh\nprintf ran > '{}'\n", ran.display()),
    );
    let request = ImportDownloadRequest {
        session_id: "sp2yt-hostile-inbox".to_owned(),
        row_id: "row-00001".to_owned(),
        source_order: 1,
        claim: import_claim("sp2yt-hostile-inbox", "row-00001", 1, "abc123def45"),
        song: Song::remote("abc123def45", "Track", "Artist", "3:12")
            .with_import_session(Some("sp2yt-hostile-inbox".to_owned()), Some(1)),
    };
    let emit = accepted_event_sink();

    let error = run_import_download_with_program(
        fake.to_str().unwrap(),
        &request,
        &download_dir,
        None,
        &emit,
    )
    .await
    .expect_err("an intermediate inbox symlink must be rejected");

    assert!(format!("{error:#}").contains("symlink or non-directory"));
    assert!(!ran.exists(), "yt-dlp ran through a hostile inbox scope");
    assert!(!outside.join("sp2yt-hostile-inbox").exists());
    let _ = fs::remove_dir_all(root);
}

#[cfg(unix)]
#[tokio::test]
async fn required_sidecar_failure_never_commits_session_or_emits_done() {
    let root = temp_dir("download-import-sidecar-failure");
    let bin_dir = root.join("bin");
    let download_dir = root.join("music");
    fs::create_dir_all(&bin_dir).unwrap();
    let fixture_audio = root.join("fixture.wav");
    write_test_wav(&fixture_audio);
    let session_id = "sp2yt-sidecar-failure";
    let incoming = download_dir
        .join(".yututui-inbox")
        .join(session_id)
        .join("incoming")
        .join("Track [abc123def45].wav");
    fs::create_dir_all(incoming.parent().unwrap()).unwrap();
    let claim = import_claim(session_id, "row-00001", 1, "abc123def45");
    let owned_generation = incoming.with_file_name(format!(".ytt-owned-{}.wav", claim.claim_id));
    fs::create_dir(crate::downloads::sidecar_path(&owned_generation)).unwrap();
    let fake = write_executable(
        &bin_dir,
        "yt-dlp",
        &format!(
            "#!/bin/sh\ncp '{fixture}' '{incoming}'\nprintf '%s\\n' '{incoming}'\n",
            fixture = fixture_audio.display(),
            incoming = incoming.display(),
        ),
    );
    let events = Arc::new(Mutex::new(Vec::new()));
    let captured = Arc::clone(&events);
    let emit: EventSink = Arc::new(move |event| {
        captured.lock().unwrap().push(event);
        Ok(DeliveryReceipt::Enqueued)
    });
    let request = ImportDownloadRequest {
        session_id: session_id.to_owned(),
        row_id: "row-00001".to_owned(),
        source_order: 1,
        claim,
        song: Song::remote("abc123def45", "Track", "Artist", "3:12")
            .with_import_session(Some(session_id.to_owned()), Some(1)),
    };
    save_import_download_session(&request);

    let error = run_import_download_with_program(
        fake.to_str().unwrap(),
        &request,
        &download_dir,
        None,
        &emit,
    )
    .await
    .expect_err("required sidecar publication must be fatal");

    assert!(format!("{error:#}").contains("required download sidecar"));
    assert!(
        incoming.is_file(),
        "valid downloaded audio must remain retryable"
    );
    assert!(fs::metadata(&incoming).unwrap().len() >= 44);
    assert!(
        !events
            .lock()
            .unwrap()
            .iter()
            .any(|event| matches!(event, DownloadEvent::ImportDone { .. }))
    );
    let session = crate::transfer::session::ImportSession::load(session_id).unwrap();
    assert!(!session.rows[0].written);
    assert!(session.rows[0].local_path.is_none());

    assert!(
        owned_generation.is_file(),
        "rewritten generation is retained"
    );
    fs::remove_dir(crate::downloads::sidecar_path(&owned_generation)).unwrap();
    let retry_ran = root.join("retry-ytdlp-ran");
    let retry_program = write_executable(
        &bin_dir,
        "yt-dlp-retry",
        &format!(
            "#!/bin/sh\nprintf ran > '{}'\nexit 41\n",
            retry_ran.display()
        ),
    );
    run_import_download_with_program(
        retry_program.to_str().unwrap(),
        &request,
        &download_dir,
        None,
        &accepted_event_sink(),
    )
    .await
    .expect("retry should finalize the owned inbox file without another download");
    assert!(!retry_ran.exists(), "retry unexpectedly re-ran yt-dlp");
    let session = crate::transfer::session::ImportSession::load(session_id).unwrap();
    assert!(session.rows[0].written);
    assert!(
        session.rows[0]
            .local_path
            .as_ref()
            .is_some_and(|path| path.is_file())
    );

    let _ = crate::transfer::session::ImportSession::delete_record(session_id);
    let _ = fs::remove_dir_all(root);
}

#[cfg(unix)]
#[tokio::test]
async fn run_download_rejects_reported_path_outside_download_dir() {
    let root = temp_dir("download-escape");
    let bin_dir = root.join("bin");
    let download_dir = root.join("music");
    fs::create_dir_all(&bin_dir).unwrap();
    let outside = root.join("outside.m4a");
    let fake = write_executable(
        &bin_dir,
        "yt-dlp",
        &format!(
            "#!/bin/sh\nprintf 'audio' > '{outside}'\nprintf '%s\\n' '{outside}'\n",
            outside = outside.display()
        ),
    );
    let emit = accepted_event_sink();
    let result = run_download_with_program(
        fake.to_str().unwrap(),
        &Song::remote("abc123def45", "Track", "Artist", "3:12"),
        &download_dir,
        None,
        &emit,
    )
    .await
    .expect_err("outside path must be rejected");
    assert!(
        format!("{result:#}").contains("escaped the download directory"),
        "unexpected error: {result:#}"
    );
    let _ = fs::remove_dir_all(root);
}

#[cfg(unix)]
#[tokio::test]
async fn run_download_rejects_a_phantom_path_that_was_never_written() {
    let root = temp_dir("download-phantom");
    let bin_dir = root.join("bin");
    let download_dir = root.join("music");
    fs::create_dir_all(&bin_dir).unwrap();
    // yt-dlp exits 0 and prints a plausible path but never creates the file.
    let phantom = download_dir.join("Track [abc123def45].m4a");
    let fake = write_executable(
        &bin_dir,
        "yt-dlp",
        &format!("#!/bin/sh\nprintf '%s\\n' '{}'\n", phantom.display()),
    );
    let emit = accepted_event_sink();
    let result = run_download_with_program(
        fake.to_str().unwrap(),
        &Song::remote("abc123def45", "Track", "Artist", "3:12"),
        &download_dir,
        None,
        &emit,
    )
    .await;
    assert!(
        result.is_err(),
        "a phantom (never-written) path must be rejected, not reported as saved"
    );
    let _ = fs::remove_dir_all(root);
}

#[cfg(unix)]
#[tokio::test]
async fn run_download_reports_stderr_line_on_ytdlp_failure() {
    let root = temp_dir("download-stderr");
    let bin_dir = root.join("bin");
    let download_dir = root.join("music");
    fs::create_dir_all(&bin_dir).unwrap();
    // A long warning precedes the decisive ERROR line, as a stale yt-dlp does; the
    // reported detail must keep the LAST line even when earlier lines exceed the
    // 200-char excerpt cap on their own.
    let long_warning = format!("WARNING: {}", "w".repeat(250));
    let fake = write_executable(
        &bin_dir,
        "yt-dlp",
        &format!(
            "#!/bin/sh\nprintf '%s\\n' '{long_warning}' >&2\nprintf '%s\\n' 'ERROR: private video needs login' >&2\nexit 1\n"
        ),
    );
    let emit = accepted_event_sink();
    let result = run_download_with_program(
        fake.to_str().unwrap(),
        &Song::remote("abc123def45", "Track", "Artist", "3:12"),
        &download_dir,
        None,
        &emit,
    )
    .await
    .expect_err("yt-dlp failure should be reported");
    let error = format!("{result:#}");
    assert!(error.contains("ERROR: private video needs login"));
    let _ = fs::remove_dir_all(root);
}

#[cfg(unix)]
#[tokio::test]
async fn progress_reader_is_fenced_before_an_error_terminal() {
    let root = temp_dir("download-progress-fence");
    let bin_dir = root.join("bin");
    let download_dir = root.join("music");
    fs::create_dir_all(&bin_dir).unwrap();
    let fake = write_executable(
        &bin_dir,
        "yt-dlp",
        "#!/bin/sh\nprintf 'download: 1.0%%\\n' >&2\n( sleep 0.05; printf 'download: 90.0%%\\n' >&2 ) &\ndd if=/dev/zero bs=70000 count=1 2>/dev/null | tr '\\000' x\nwait\n",
    );
    let events = Arc::new(Mutex::new(Vec::new()));
    let captured = Arc::clone(&events);
    let emit: EventSink = Arc::new(move |event| {
        captured.lock().unwrap().push(event);
        Ok(DeliveryReceipt::Enqueued)
    });

    let result = run_download_with_program(
        fake.to_str().unwrap(),
        &Song::remote("abc123def45", "Track", "Artist", "3:12"),
        &download_dir,
        None,
        &emit,
    )
    .await;
    assert!(
        result.is_err(),
        "oversized stdout must terminate the download"
    );
    assert!(
        emit_terminal(
            &emit,
            DownloadEvent::Error {
                video_id: "abc123def45".to_owned(),
                error: "oversized output".to_owned(),
            },
            &DownloadCancellation::never(),
        )
        .await
    );
    tokio::time::sleep(Duration::from_millis(150)).await;
    let events = events.lock().unwrap();
    assert!(matches!(events.last(), Some(DownloadEvent::Error { .. })));

    let _ = fs::remove_dir_all(root);
}

#[cfg(unix)]
#[tokio::test]
async fn actor_shutdown_joins_task_and_kills_ytdlp_process_group() {
    let root = temp_dir("download-shutdown");
    let bin_dir = root.join("bin");
    let download_dir = root.join("music");
    let parent_pid = root.join("parent.pid");
    let helper_pid = root.join("helper.pid");
    fs::create_dir_all(&bin_dir).unwrap();
    let fake = write_executable(
        &bin_dir,
        "yt-dlp",
        &format!(
            "#!/bin/sh\nprintf '%s\\n' \"$$\" > '{parent_pid}'\nsleep 30 &\nprintf '%s\\n' \"$!\" > '{helper_pid}'\nwait\n",
            parent_pid = parent_pid.display(),
            helper_pid = helper_pid.display(),
        ),
    );
    let events = Arc::new(Mutex::new(Vec::new()));
    let captured = Arc::clone(&events);
    let handle = spawn_with_program(
        move |event| {
            captured.lock().unwrap().push(event);
            Ok(DeliveryReceipt::Enqueued)
        },
        download_dir,
        None,
        1,
        fake.to_string_lossy().into_owned(),
    );
    handle
        .start(Song::remote("abc123def45", "Track", "Artist", "3:12"))
        .unwrap();

    let parent = wait_for_pid_file(&parent_pid).await;
    let helper = wait_for_pid_file(&helper_pid).await;
    assert!(process_exists(parent));
    assert!(process_exists(helper));

    assert!(
        tokio::time::timeout(Duration::from_secs(5), handle.shutdown())
            .await
            .expect("download shutdown exceeded its contract")
    );
    wait_for_process_exit(parent).await;
    wait_for_process_exit(helper).await;
    assert!(
        !events
            .lock()
            .unwrap()
            .iter()
            .any(|event| matches!(event, DownloadEvent::Error { .. })),
        "shutdown cancellation must not surface as a user download failure"
    );

    let _ = fs::remove_dir_all(root);
}

#[cfg(unix)]
#[tokio::test]
async fn panicked_terminal_notifier_replays_the_owned_payload_once() {
    let root = temp_dir("download-task-panic-terminal");
    let bin_dir = root.join("bin");
    let download_dir = root.join("music");
    fs::create_dir_all(&bin_dir).unwrap();
    let saved = download_dir.join("Track [abc123def45].m4a");
    let fake = write_executable(
        &bin_dir,
        "yt-dlp",
        &format!(
            "#!/bin/sh\nprintf 'audio' > '{saved}'\nprintf '%s\\n' '{saved}'\n",
            saved = saved.display()
        ),
    );
    let panic_once = Arc::new(std::sync::atomic::AtomicBool::new(true));
    let sink_panic = Arc::clone(&panic_once);
    let events = Arc::new(Mutex::new(Vec::new()));
    let captured = Arc::clone(&events);
    let handle = spawn_with_program(
        move |event| {
            if sink_panic.swap(false, std::sync::atomic::Ordering::SeqCst) {
                panic!("deterministic download task panic");
            }
            captured.lock().unwrap().push(event);
            Ok(DeliveryReceipt::Enqueued)
        },
        download_dir,
        None,
        1,
        fake.to_string_lossy().into_owned(),
    );
    handle
        .start(Song::remote("abc123def45", "Track", "Artist", "3:12"))
        .unwrap();

    tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            if events
                .lock()
                .unwrap()
                .iter()
                .any(|event| matches!(event, DownloadEvent::Done { .. }))
            {
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("supervisor must restart the notifier and replay its owned terminal payload");
    assert!(handle.shutdown().await);

    let events = events.lock().unwrap();
    assert_eq!(
        events
            .iter()
            .filter(|event| matches!(event, DownloadEvent::Done { video_id, .. } if video_id == "abc123def45"))
            .count(),
        1
    );
    assert!(!events.iter().any(
        |event| matches!(event, DownloadEvent::Error { video_id, .. } if video_id == "abc123def45")
    ));

    let _ = fs::remove_dir_all(root);
}

#[cfg(unix)]
#[tokio::test]
async fn run_download_passes_cookie_file_to_ytdlp() {
    let root = temp_dir("download-cookies");
    let bin_dir = root.join("bin");
    let download_dir = root.join("music");
    fs::create_dir_all(&bin_dir).unwrap();
    let args_log = root.join("args.txt");
    let cookies = root.join("cookies.txt");
    fs::write(&cookies, "# Netscape HTTP Cookie File\n").unwrap();
    let saved = download_dir.join("Track [abc123def45].m4a");
    let fake = write_executable(
        &bin_dir,
        "yt-dlp",
        &format!(
            "#!/bin/sh\nfor arg do printf '%s\\n' \"$arg\"; done > '{args_log}'\nprintf 'audio' > '{saved}'\nprintf '%s\\n' '{saved}'\n",
            args_log = args_log.display(),
            saved = saved.display()
        ),
    );

    let emit = accepted_event_sink();

    run_download_with_program(
        fake.to_str().unwrap(),
        &Song::remote("abc123def45", "Track", "Artist", "3:12"),
        &download_dir,
        Some(cookies.as_path()),
        &emit,
    )
    .await
    .unwrap();

    let args: Vec<String> = fs::read_to_string(&args_log)
        .unwrap()
        .lines()
        .map(str::to_owned)
        .collect();
    let cookie_arg = cookies.to_string_lossy().into_owned();
    assert!(args.iter().any(|arg| arg == "--cookies"));
    assert!(args.iter().any(|arg| arg == &cookie_arg));

    let _ = fs::remove_dir_all(root);
}

#[cfg(unix)]
fn write_executable(dir: &Path, name: &str, contents: &str) -> PathBuf {
    let path = dir.join(name);
    fs::write(&path, contents).unwrap();
    fs::set_permissions(&path, fs::Permissions::from_mode(0o755)).unwrap();
    path
}

#[cfg(unix)]
fn write_test_wav(path: &Path) {
    const SAMPLE_RATE: u32 = 8_000;
    const DATA_LEN: u32 = 800;
    let mut bytes = Vec::with_capacity(44 + DATA_LEN as usize);
    bytes.extend_from_slice(b"RIFF");
    bytes.extend_from_slice(&(36 + DATA_LEN).to_le_bytes());
    bytes.extend_from_slice(b"WAVEfmt ");
    bytes.extend_from_slice(&16_u32.to_le_bytes());
    bytes.extend_from_slice(&1_u16.to_le_bytes());
    bytes.extend_from_slice(&1_u16.to_le_bytes());
    bytes.extend_from_slice(&SAMPLE_RATE.to_le_bytes());
    bytes.extend_from_slice(&SAMPLE_RATE.to_le_bytes());
    bytes.extend_from_slice(&1_u16.to_le_bytes());
    bytes.extend_from_slice(&8_u16.to_le_bytes());
    bytes.extend_from_slice(b"data");
    bytes.extend_from_slice(&DATA_LEN.to_le_bytes());
    bytes.resize(44 + DATA_LEN as usize, 128);
    fs::write(path, bytes).unwrap();
}

#[cfg(unix)]
async fn wait_for_pid_file(path: &Path) -> libc::pid_t {
    // Process-spawn tests run alongside CPU-heavy unit tests in CI. Keep the poll responsive but
    // allow scheduling contention without observing the shell's truncate-before-write window.
    tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            if let Some(pid) = fs::read_to_string(path)
                .ok()
                .and_then(|contents| contents.trim().parse().ok())
                .filter(|pid| *pid > 0)
            {
                return pid;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .unwrap_or_else(|_| panic!("timed out waiting for a PID in {}", path.display()))
}

#[cfg(unix)]
fn process_exists(pid: libc::pid_t) -> bool {
    crate::util::process::process_exists_for_test(pid)
}

#[cfg(unix)]
async fn wait_for_process_exit(pid: libc::pid_t) {
    tokio::time::timeout(Duration::from_secs(5), async {
        while process_exists(pid) {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .unwrap_or_else(|_| panic!("process {pid} survived download shutdown"));
}

#[cfg(unix)]
fn temp_dir(name: &str) -> PathBuf {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    std::env::temp_dir().join(format!(
        "yututui-{name}-test-{}-{nanos}",
        std::process::id()
    ))
}
