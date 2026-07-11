use super::*;

#[test]
fn shift_l_toggles_lyrics_and_fetches_on_open() {
    let mut app = app_playing(3, 0); // playing id0
    let cmds = app.update(Msg::Key(key(KeyCode::Char('L'))));
    assert!(app.lyrics.visible);
    assert!(app.lyrics.loading);
    match cmds.as_slice() {
        [Cmd::FetchLyrics { video_id, .. }] => assert_eq!(video_id, "id0"),
        _ => panic!("expected a FetchLyrics cmd"),
    }
    // Toggling off issues no fetch.
    let cmds = app.update(Msg::Key(key(KeyCode::Char('L'))));
    assert!(!app.lyrics.visible);
    assert!(cmds.is_empty());
}

#[test]
fn lyrics_result_stored_only_for_current_track() {
    let mut app = app_playing(3, 0); // current id0
    let lines = lyric_lines();
    app.update(Msg::LyricsResult {
        video_id: "id0".to_owned(),
        lines: std::sync::Arc::clone(&lines),
    });
    assert!(
        app.lyrics
            .track
            .as_ref()
            .is_some_and(|l| l.lines.len() == 2)
    );
    assert!(std::sync::Arc::ptr_eq(
        &lines,
        &app.lyrics.track.as_ref().unwrap().lines
    ));
    // A late result for a different track is ignored.
    app.update(Msg::LyricsResult {
        video_id: "stale".to_owned(),
        lines: lyric_lines(),
    });
    assert_eq!(app.lyrics.track.as_ref().unwrap().video_id, "id0");
}

#[test]
fn advancing_track_clears_lyrics_and_refetches_when_open() {
    let mut app = app_playing(3, 0);
    app.lyrics.visible = true;
    app.update(Msg::LyricsResult {
        video_id: "id0".to_owned(),
        lines: lyric_lines(),
    });
    assert!(app.lyrics.track.is_some());
    let cmds = app.update(Msg::Key(key(KeyCode::Char('.')))); // -> id1
    assert!(app.lyrics.track.is_none());
    assert!(app.lyrics.loading);
    assert!(
        cmds.iter()
            .any(|c| matches!(c, Cmd::FetchLyrics { video_id, .. } if video_id == "id1"))
    );
}

// --- Album art ----------------------------------------------------------

#[test]
fn album_art_off_emits_no_fetch() {
    let mut app = app_playing(3, 0);
    // Opt-in: off by default → advancing a track issues no artwork fetch.
    let cmds = app.update(Msg::Key(key(KeyCode::Char('.'))));
    assert!(!cmds.iter().any(|c| matches!(c, Cmd::FetchArtwork { .. })));
    assert!(!app.art.loading);
}

#[test]
fn album_art_on_fetches_remote_then_builds_protocol() {
    let mut app = app_playing(3, 0);
    app.config.album_art = Some(true);
    app.art.picker = Some(Picker::halfblocks());
    let (resize_tx, _) = tokio::sync::mpsc::channel(8);
    app.set_art_resize_tx(resize_tx);
    // Advancing to id1 now fetches its thumbnail from the remote source.
    let cmds = app.update(Msg::Key(key(KeyCode::Char('.'))));
    assert!(app.art.loading);
    assert!(cmds.iter().any(|c| matches!(
        c,
        Cmd::FetchArtwork { video_id, source: ArtSource::Remote { video_id: vid } }
            if video_id == "id1" && vid == "id1"
    )));
    // The decoded image becomes a render-ready protocol for the current track.
    app.update(Msg::ArtworkResult {
        video_id: "id1".to_owned(),
        image: Some(image::DynamicImage::new_rgb8(120, 120)),
    });
    assert!(!app.art.loading);
    assert!(app.art_active());
    assert_eq!(app.art.dims, (120, 120));
    assert_eq!(
        std::sync::Arc::strong_count(app.art.source.as_ref().expect("held decoded art")),
        2,
        "App and the resize protocol should share one decoded pixel allocation"
    );
}

#[test]
fn owned_and_shared_album_art_protocols_render_identically() {
    use image::imageops::FilterType;
    use image::{DynamicImage, Rgba, RgbaImage};
    use ratatui::buffer::Buffer;
    use ratatui::layout::Rect;
    use ratatui_image::{Resize, ResizeEncodeRender};

    let mut pixels = RgbaImage::new(17, 11);
    for (x, y, pixel) in pixels.enumerate_pixels_mut() {
        *pixel = Rgba([
            (x * 31 + y * 7) as u8,
            (x * 11 + y * 37) as u8,
            (x * 19 + y * 13) as u8,
            if (x + y) % 3 == 0 { 96 } else { 255 },
        ]);
    }
    let image = DynamicImage::ImageRgba8(pixels);

    for background in [None, Some(Rgba([20, 30, 40, 255]))] {
        let mut picker = Picker::halfblocks();
        picker.set_background_color(background);
        for area in [Rect::new(0, 0, 5, 3), Rect::new(0, 0, 9, 4)] {
            let mut owned = picker.new_resize_protocol(image.clone());
            let mut shared = picker.new_resize_protocol_shared(std::sync::Arc::new(image.clone()));
            let mut owned_buffer = Buffer::empty(area);
            let mut shared_buffer = Buffer::empty(area);

            owned.resize_encode_render(
                &Resize::Scale(Some(FilterType::Triangle)),
                area,
                &mut owned_buffer,
            );
            shared.resize_encode_render(
                &Resize::Scale(Some(FilterType::Triangle)),
                area,
                &mut shared_buffer,
            );

            assert_eq!(owned_buffer, shared_buffer, "background={background:?}");
            assert!(owned.last_encoding_result().unwrap().is_ok());
            assert!(shared.last_encoding_result().unwrap().is_ok());
        }
    }
}

#[test]
fn artwork_result_for_stale_track_is_ignored() {
    let mut app = app_playing(3, 0); // current id0
    app.config.album_art = Some(true);
    app.art.picker = Some(Picker::halfblocks());
    app.update(Msg::ArtworkResult {
        video_id: "stale".to_owned(),
        image: Some(image::DynamicImage::new_rgb8(8, 8)),
    });
    assert!(!app.art_active());
}

#[test]
fn local_track_uses_local_art_source() {
    let mut app = App::new(100);
    app.config.album_art = Some(true);
    app.art.picker = Some(Picker::halfblocks());
    let song = Song::local_file(std::path::PathBuf::from("/music/song.m4a"));
    assert!(matches!(
        app.artwork_source(&song),
        Some(ArtSource::Local(_))
    ));
}

#[test]
fn art_fit_rect_centers_by_aspect() {
    let mut app = App::new(100);
    app.art.picker = Some(Picker::halfblocks()); // font cell 10x20 px
    app.art.dims = (100, 100); // square source
    let r = app.art_fit_rect(Rect {
        x: 0,
        y: 0,
        width: 40,
        height: 40,
    });
    // Cells are 1:2 (10×20px), so a square cover spans the full width but only half the
    // height, centered vertically in the box.
    assert_eq!((r.width, r.height), (40, 20));
    assert_eq!((r.x, r.y), (0, 10));
}

// --- M7: downloads ------------------------------------------------------

#[test]
fn d_starts_download_of_current_track() {
    let mut app = app_playing(3, 0); // playing id0
    let cmds = app.update(Msg::Key(key(KeyCode::Char('d'))));
    match cmds.as_slice() {
        [Cmd::Download(song)] => assert_eq!(song.video_id, "id0"),
        _ => panic!("expected a Download cmd"),
    }
    assert_eq!(
        app.downloads.active.get("id0"),
        Some(&DownloadState::Running(0))
    );
}

#[test]
fn d_ignores_local_tracks() {
    let _guard = crate::i18n::lock_for_test();
    let mut app = App::new(100);
    app.queue.set(
        vec![Song::local_file(PathBuf::from("/tmp/local-track.m4a"))],
        0,
    );
    let cmds = app.update(Msg::Key(key(KeyCode::Char('d'))));
    assert!(cmds.is_empty());
    assert!(app.status.text.contains("Already local"));
}

#[test]
fn download_progress_and_done_update_state() {
    let mut app = app_playing(1, 0);
    app.update(Msg::DownloadProgress {
        video_id: "id0".to_owned(),
        percent: 42.6,
    });
    assert_eq!(
        app.downloads.active.get("id0"),
        Some(&DownloadState::Running(43))
    );
    app.update(Msg::DownloadDone {
        video_id: "id0".to_owned(),
        path: "/tmp/x.m4a".to_owned(),
    });
    assert_eq!(app.downloads.active.get("id0"), Some(&DownloadState::Done));
    assert!(app.status.text.contains("/tmp/x.m4a"));
    assert_eq!(app.library_ui.downloaded.len(), 1);
    assert_eq!(app.library_ui.downloaded[0].playback_target(), "/tmp/x.m4a");
}

#[test]
fn download_error_marks_failed() {
    let mut app = app_playing(1, 0);
    app.update(Msg::DownloadError {
        video_id: "id0".to_owned(),
        error: "boom".to_owned(),
    });
    assert_eq!(
        app.downloads.active.get("id0"),
        Some(&DownloadState::Failed)
    );
    assert!(app.status.text.contains("boom"));
}

// --- M8: prefetch / instant skip ----------------------------------------

#[test]
fn loading_prefetches_the_next_track() {
    // Loading id0 with id1 next in the queue → should request a resolve for id1.
    let mut app = App::new(100);
    app.queue.set(songs(3), 0);
    let song = app.queue.current().cloned();
    let cmds = app.load_song(song);
    assert!(resolve_cmd(&cmds, "id1").is_some_and(|u| u.contains("id1")));
}

#[test]
fn skip_uses_prefetched_url_when_available() {
    let mut app = app_playing(3, 0); // playing id0, prefetch requested for id1
    app.update(StreamingMsg::Resolved {
        video_id: "id1".to_owned(),
        stream_url: "https://cdn.example/stream-id1".to_owned(),
    });
    // Skip: id1 should load via the prefetched direct URL, not its watch URL.
    let cmds = app.update(Msg::Key(key(KeyCode::Char('.'))));
    let url = load_url(&cmds).expect("a Load cmd");
    assert_eq!(url, "https://cdn.example/stream-id1");
    // And it should now prefetch id2.
    assert!(resolve_cmd(&cmds, "id2").is_some());
}

#[test]
fn skip_drops_stale_prefetched_url_and_falls_back() {
    let mut app = app_playing(3, 0);
    app.prefetch
        .resolved
        .insert_expired("id1".to_owned(), "https://cdn.example/stale-id1".to_owned());

    let cmds = app.update(Msg::Key(key(KeyCode::Char('.'))));

    assert_loads_video(&cmds, "id1");
    assert!(!app.prefetch.resolved.contains_fresh("id1"));
}

#[test]
fn prefetch_cache_lru_caps_without_clearing_everything() {
    let mut app = App::new(100);
    for i in 0..65 {
        app.update(StreamingMsg::Resolved {
            video_id: format!("id{i}"),
            stream_url: format!("https://cdn.example/stream-id{i}"),
        });
    }

    assert_eq!(app.prefetch.resolved.len(), 64);
    assert!(!app.prefetch.resolved.contains_fresh("id0"));
    assert!(app.prefetch.resolved.contains_fresh("id64"));
}

#[test]
fn skip_without_prefetch_falls_back_to_watch_url() {
    let mut app = app_playing(3, 0);
    let cmds = app.update(Msg::Key(key(KeyCode::Char('.')))); // no Resolved arrived
    assert_loads_video(&cmds, "id1");
}

// --- M9: mouse controls -------------------------------------------------
