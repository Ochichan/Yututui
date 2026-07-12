use super::*;

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

#[tokio::test(flavor = "current_thread")]
async fn import_full_and_closed_errors_preserve_the_exact_tracking_owner() {
    let request = |session_id: &str| ImportDownloadRequest {
        session_id: session_id.to_owned(),
        row_id: "row-00009".to_owned(),
        source_order: 9,
        claim: crate::transfer::artifact_identity::test_import_claim(
            session_id,
            "row-00009",
            9,
            "shared-video",
        ),
        song: Song::remote("shared-video", "title", "artist", "1:00")
            .with_import_session(Some(session_id.to_owned()), Some(9)),
    };

    let (full_tx, _full_rx) = tokio::sync::mpsc::channel(1);
    let full_handle = test_download_handle(full_tx);
    for index in 0..ownership::DOWNLOAD_OUTSTANDING_MAX {
        assert!(
            full_handle
                .outstanding
                .register(DownloadOwner::Ordinary(format!("owner-{index}")))
                .is_some()
        );
    }
    let full = full_handle
        .start_for_import(request("sp2yt-full-owner"))
        .expect_err("the owner-state cap must reject one more import");
    assert_eq!(
        full.tracking_key,
        import_download_tracking_key("sp2yt-full-owner", 9)
    );
    let full_context = full.import_context.expect("full import context");
    assert_eq!(full_context.row_id, "row-00009");
    assert_eq!(full_context.video_id, "shared-video");

    let (closed_tx, closed_rx) = tokio::sync::mpsc::channel(1);
    let closed_handle = test_download_handle(closed_tx);
    drop(closed_rx);
    let closed = closed_handle
        .start_for_import(request("sp2yt-closed-owner"))
        .expect_err("a closed actor must reject the import");
    assert_eq!(
        closed.tracking_key,
        import_download_tracking_key("sp2yt-closed-owner", 9)
    );
    let closed_context = closed.import_context.expect("closed import context");
    assert_eq!(closed_context.session_id, "sp2yt-closed-owner");
    assert_eq!(closed_context.source_order, 9);
}

#[tokio::test(flavor = "current_thread")]
async fn duplicate_live_import_claim_is_coalesced_before_a_second_worker_command() {
    let (tx, mut rx) = tokio::sync::mpsc::channel(4);
    let handle = test_download_handle(tx);
    let claim = crate::transfer::artifact_identity::test_import_claim(
        "sp2yt-duplicate-claim",
        "row-00001",
        1,
        "shared-video",
    );
    let request = ImportDownloadRequest {
        session_id: claim.session_id.clone(),
        row_id: claim.row_id.clone(),
        source_order: claim.source_order,
        claim,
        song: Song::remote("shared-video", "Title", "Artist", "1:00")
            .with_import_session(Some("sp2yt-duplicate-claim".to_owned()), Some(1)),
    };

    handle.start_for_import(request.clone()).unwrap();
    handle.start_for_import(request).unwrap();

    assert!(matches!(
        rx.recv().await.unwrap().command,
        DownloadCmd::StartForImport(_)
    ));
    assert!(matches!(
        rx.try_recv(),
        Err(tokio::sync::mpsc::error::TryRecvError::Empty)
    ));
    assert_eq!(handle.outstanding.snapshot().len(), 1);
}

#[tokio::test(flavor = "current_thread")]
async fn same_video_rows_in_one_session_are_distinct_admissions() {
    let (tx, mut rx) = tokio::sync::mpsc::channel(4);
    let handle = test_download_handle(tx);
    let request = |row_id: &str, source_order: u32| {
        let claim = crate::transfer::artifact_identity::test_import_claim(
            "sp2yt-same-session",
            row_id,
            source_order,
            "shared-video",
        );
        ImportDownloadRequest {
            session_id: claim.session_id.clone(),
            row_id: claim.row_id.clone(),
            source_order: claim.source_order,
            claim,
            song: Song::remote("shared-video", "Title", "Artist", "1:00")
                .with_import_session(Some("sp2yt-same-session".to_owned()), Some(source_order)),
        }
    };

    handle.start_for_import(request("row-00001", 1)).unwrap();
    handle.start_for_import(request("row-00007", 7)).unwrap();

    let mut admitted = Vec::new();
    for _ in 0..2 {
        let command = rx.recv().await.expect("both row owners must be queued");
        let admission_id = command
            .admission_id
            .expect("an import start must have an admission id");
        let DownloadCmd::StartForImport(request) = command.command else {
            panic!("expected an import start command");
        };
        admitted.push((
            admission_id,
            request.session_id,
            request.row_id,
            request.source_order,
            request.claim.claim_id,
            request.song.video_id,
        ));
    }
    admitted.sort_unstable_by_key(|entry| entry.3);
    assert_eq!(admitted[0].1, "sp2yt-same-session");
    assert_eq!(admitted[0].2, "row-00001");
    assert_eq!(admitted[0].3, 1);
    assert_eq!(admitted[0].4, "test-claim-1-shared-video");
    assert_eq!(admitted[0].5, "shared-video");
    assert_eq!(admitted[1].1, "sp2yt-same-session");
    assert_eq!(admitted[1].2, "row-00007");
    assert_eq!(admitted[1].3, 7);
    assert_eq!(admitted[1].4, "test-claim-7-shared-video");
    assert_eq!(admitted[1].5, "shared-video");
    assert_ne!(admitted[0].0, admitted[1].0);

    let mut owners: Vec<_> = handle
        .outstanding
        .running_snapshot()
        .into_iter()
        .map(|(admission_id, owner)| match owner {
            DownloadOwner::Import(context) => (
                admission_id,
                context.session_id,
                context.row_id,
                context.source_order,
                context.claim.claim_id,
                context.video_id,
            ),
            DownloadOwner::Ordinary(_) => panic!("expected a stable import owner"),
        })
        .collect();
    owners.sort_unstable_by_key(|entry| entry.3);
    assert_eq!(owners, admitted);

    // Persisted, distinct completion paths are exercised by the two-session integration test.
    // This unit test isolates the collision-prone admission boundary: sharing both a session and
    // a video id must not coalesce different row/source-order/claim owners.
    assert!(matches!(
        rx.try_recv(),
        Err(tokio::sync::mpsc::error::TryRecvError::Empty)
    ));
}

#[test]
fn synthetic_import_interruption_is_visible_and_the_claim_remains_resumable() {
    let session_id = "sp2yt-interrupted-owner";
    let claim = crate::transfer::artifact_identity::test_import_claim(
        session_id,
        "row-00001",
        1,
        "shared-video",
    );
    crate::transfer::session::ImportSession {
        session_id: session_id.to_owned(),
        session_instance_id: claim.session_instance_id.clone(),
        rows: vec![crate::transfer::session::ImportSessionRow {
            row_id: claim.row_id.clone(),
            source_order: 1,
            status: crate::transfer::session::ImportSessionRowStatus::Matched,
            selected_key: Some(claim.expected_key.clone()),
            download_claim: Some(claim.clone()),
            ..crate::transfer::session::ImportSessionRow::default()
        }],
        ..crate::transfer::session::ImportSession::default()
    }
    .save()
    .unwrap();
    let context = ImportDownloadContext {
        session_id: session_id.to_owned(),
        row_id: claim.row_id.clone(),
        source_order: 1,
        video_id: "shared-video".to_owned(),
        claim: claim.clone(),
    };

    let event = DownloadOwner::Import(context).unexpected_error("worker panicked".to_owned());

    assert!(matches!(event, DownloadEvent::ImportError { .. }));
    let interrupted = crate::transfer::session::ImportSession::load(session_id).unwrap();
    assert_eq!(interrupted.rows[0].errors, vec!["worker panicked"]);
    assert_eq!(interrupted.rows[0].download_claim.as_ref(), Some(&claim));
    assert_eq!(
        crate::transfer::session::claim_import_download(session_id, 1, "shared-video").unwrap(),
        claim
    );
    crate::transfer::session::record_import_download_error(&claim, "stopped").unwrap();
    crate::transfer::session::ImportSession::delete_record(session_id).unwrap();
}

#[tokio::test(flavor = "current_thread")]
async fn sparse_over_cap_import_is_rejected_before_metadata_or_sidecar_rewrite() {
    let root = std::env::temp_dir().join(format!(
        "yututui-import-pre-metadata-cap-{}",
        std::process::id()
    ));
    std::fs::create_dir_all(&root).unwrap();
    let audio = root.join("Large [abc123def45].m4a");
    let file = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&audio)
        .unwrap();
    file.set_len(crate::transfer::artifact_identity::ARTIFACT_AUDIO_MAX_BYTES + 1)
        .unwrap();
    drop(file);
    let song = Song::remote("abc123def45", "Large", "Artist", "1:00");

    let error = finalize_download_metadata(&song, &audio, &root, Some(&root), None, true)
        .await
        .expect_err("sparse over-cap input must fail before metadata rewrite");

    assert!(format!("{error:#}").contains("before metadata rewrite"));
    assert_eq!(
        std::fs::metadata(&audio).unwrap().len(),
        crate::transfer::artifact_identity::ARTIFACT_AUDIO_MAX_BYTES + 1
    );
    assert!(!crate::downloads::sidecar_path(&audio).exists());
    std::fs::remove_dir_all(root).unwrap();
}
