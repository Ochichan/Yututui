use std::sync::Arc;

use super::*;
use crate::open_subsonic::{AccountScopeId, BackendId, ItemId, OpenSubsonicItemRef, ServerSong};

#[tokio::test]
async fn rejects_music_server_tracks_before_spawning_ytdlp() {
    let song = Song::from_open_subsonic(ServerSong {
        item: OpenSubsonicItemRef::new(
            BackendId::new("test-backend").unwrap(),
            AccountScopeId::new("test-account").unwrap(),
            ItemId::new("test-song").unwrap(),
        ),
        title: "Server song".to_owned(),
        artist: "Server artist".to_owned(),
        artists: vec!["Server artist".to_owned()],
        album: None,
        album_id: None,
        album_artist: None,
        duration_secs: Some(180),
        track_number: None,
        disc_number: None,
        year: None,
        cover_art_id: None,
        content_type: Some("audio/flac".to_owned()),
        suffix: Some("flac".to_owned()),
        starred: false,
        user_rating: None,
    });
    let emit: EventSink = Arc::new(|_| Ok(DeliveryReceipt::Enqueued));
    let root =
        std::env::temp_dir().join(format!("ytt-reject-server-download-{}", std::process::id()));

    let error = run_download_with_program(
        root.join("must-not-exist").to_str().unwrap(),
        &song,
        &root,
        None,
        &emit,
    )
    .await
    .unwrap_err();

    assert!(
        error
            .to_string()
            .contains("music-server downloads are not supported")
    );
}
