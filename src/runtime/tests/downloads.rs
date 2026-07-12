use super::*;

#[test]
fn rejected_download_admission_reduces_terminal_failure_without_owner_requeue() {
    let mut app = App::new(100);
    app.downloads
        .active
        .insert("video".to_owned(), crate::app::DownloadState::Running(0));

    let follow_ups = recover_download_admission(
        &mut app,
        crate::download::DownloadStartError {
            video_id: "video".to_owned(),
            tracking_key: "video".to_owned(),
            import_context: None,
        },
    );

    assert!(follow_ups.is_empty());
    assert_eq!(
        app.downloads.active.get("video"),
        Some(&crate::app::DownloadState::Failed)
    );
    assert_eq!(app.status.kind, crate::app::StatusKind::Error);
    assert!(app.status.text.contains("queue is full"));
}

#[test]
fn rejected_import_admission_settles_and_records_the_exact_session_row() {
    let session_id = "sp2yt-runtime-rejected-import";
    let claim = crate::transfer::artifact_identity::test_import_claim(
        session_id,
        "row-00004",
        4,
        "shared-video",
    );
    crate::transfer::session::ImportSession {
        session_id: session_id.to_owned(),
        session_instance_id: claim.session_instance_id.clone(),
        rows: vec![crate::transfer::session::ImportSessionRow {
            row_id: "row-00004".to_owned(),
            source_order: 4,
            status: crate::transfer::session::ImportSessionRowStatus::Matched,
            selected_key: Some("shared-video".to_owned()),
            download_claim: Some(claim.clone()),
            ..crate::transfer::session::ImportSessionRow::default()
        }],
        ..crate::transfer::session::ImportSession::default()
    }
    .save()
    .unwrap();
    let mut app = App::new(100);
    let context = crate::download::ImportDownloadContext {
        session_id: session_id.to_owned(),
        row_id: "row-00004".to_owned(),
        source_order: 4,
        video_id: "shared-video".to_owned(),
        claim,
    };
    let tracking_key = context.tracking_key();
    app.downloads
        .active
        .insert(tracking_key.clone(), crate::app::DownloadState::Running(0));

    let follow_ups = recover_download_admission(
        &mut app,
        crate::download::DownloadStartError {
            video_id: context.video_id.clone(),
            tracking_key: tracking_key.clone(),
            import_context: Some(Box::new(context)),
        },
    );

    assert!(follow_ups.is_empty());
    assert_eq!(
        app.downloads.active.get(&tracking_key),
        Some(&crate::app::DownloadState::Failed)
    );
    assert!(!app.downloads.active.contains_key("shared-video"));
    let session = crate::transfer::session::ImportSession::load(session_id).unwrap();
    assert_eq!(session.rows[0].errors.len(), 1);
    assert!(session.rows[0].errors[0].contains("queue is full"));
    assert!(session.rows[0].download_claim.is_none());
    let _ = crate::transfer::session::ImportSession::delete_record(session_id);
}

#[test]
fn read_only_import_rejection_settles_the_import_tracking_key() {
    let mut app = App::new(100);
    let song = crate::api::Song::remote("shared-video", "Title", "Artist", "1:00")
        .with_import_session(Some("sp2yt-read-only-import".to_owned()), Some(2));
    let tracking_key = crate::download::import_download_tracking_key("sp2yt-read-only-import", 2);
    app.downloads
        .active
        .insert(tracking_key.clone(), crate::app::DownloadState::Running(0));
    let command = Cmd::Download(crate::app::DownloadCmd::Start(Box::new(song)));

    let follow_ups = super::super::read_only::reject_mutation(
        &mut app,
        &command,
        "downloads",
        "test writer lease",
    );

    assert!(follow_ups.is_empty());
    assert_eq!(
        app.downloads.active.get(&tracking_key),
        Some(&crate::app::DownloadState::Failed)
    );
    assert!(!app.downloads.active.contains_key("shared-video"));
}

#[test]
fn read_only_confirmed_delete_is_rejected_before_the_file_or_manifest_changes() {
    let root =
        std::env::temp_dir().join(format!("yututui-read-only-delete-{}", std::process::id()));
    std::fs::create_dir_all(&root).unwrap();
    let file = root.join("saved.m4a");
    std::fs::write(&file, b"audio").unwrap();
    let mut app = App::new(100);
    app.config.download_dir = file.parent().map(std::path::Path::to_path_buf);
    let song =
        crate::api::Song::remote("saved", "Saved", "Artist", "1:00").with_local_path(file.clone());
    app.library_ui.downloaded = vec![song.clone()];
    app.download_store.record(&song);
    let command = Cmd::Download(crate::app::DownloadCmd::Delete {
        paths: vec![file.clone()],
        root: app.config.effective_download_dir(),
    });

    let follow_ups = super::super::read_only::reject_mutation(
        &mut app,
        &command,
        "downloads",
        "test writer lease",
    );

    assert!(follow_ups.is_empty());
    assert!(file.exists());
    assert_eq!(app.library_ui.downloaded.len(), 1);
    assert!(app.download_store.contains_youtube_id("saved"));
    assert_eq!(app.status.kind, crate::app::StatusKind::Error);
    let _ = std::fs::remove_dir_all(root);
}
