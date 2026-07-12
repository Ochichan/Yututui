use super::*;
use crate::player::video::VideoEvent;

#[test]
fn video_open_waits_for_absolute_pause_admission() {
    for error in [
        crate::util::delivery::DeliveryError::Busy,
        crate::util::delivery::DeliveryError::Closed,
    ] {
        let mut app = app_playing(2, 0);
        let generation = app.video.generation;

        let cmds = app.toggle_video_overlay_with_fake_spawn(true);

        assert!(
            app.video.proc.is_none(),
            "spawned before {error:?} admission"
        );
        assert!(!app.playback.paused);
        assert!(!app.video.paused_audio);
        assert_eq!(app.video.generation, generation);
        assert!(cmds.iter().flat_map(Cmd::player_commands).any(|command| {
            matches!(command, PlayerCmd::SetProperty { name, value }
                if name == "pause" && value == &serde_json::Value::Bool(true))
        }));

        reject_player_transition(&mut app, cmds, error);

        assert!(app.video.proc.is_none());
        assert!(!app.playback.paused);
        assert!(!app.video.paused_audio);
        assert_eq!(app.video.generation, generation);
    }
}

#[test]
fn accepted_video_open_spawns_and_claims_the_audio_pause() {
    let mut app = app_playing(2, 0);

    let mut cmds = app.toggle_video_overlay_with_fake_spawn(true);
    assert!(app.video.proc.is_none());

    admit_player_transition(&mut app, &mut cmds);

    assert!(app.video.proc.is_some());
    assert!(app.playback.paused);
    assert!(app.video.paused_audio);
    assert_eq!(app.status.kind, StatusKind::Info);
    assert!(matches!(
        app.status.text.as_str(),
        "Opening video in mpv…" | "mpv에서 영상을 여는 중…"
    ));
    assert!(
        cmds.iter()
            .any(|cmd| matches!(cmd, Cmd::VideoConnect { .. }))
    );
    app.close_video();
}

#[test]
fn already_paused_audio_opens_without_overlay_pause_ownership() {
    let mut app = app_playing(2, 0);
    app.playback.paused = true;

    let cmds = app.toggle_video_overlay_with_fake_spawn(true);

    assert!(app.video.proc.is_some());
    assert!(app.playback.paused);
    assert!(!app.video.paused_audio);
    assert!(!cmds.iter().any(|cmd| matches!(cmd, Cmd::PlayerControl(_))));
    app.close_video();
}

#[test]
fn video_spawn_failure_returns_typed_absolute_unpause_compensation() {
    let mut app = app_playing(2, 0);
    let opening = app.toggle_video_overlay_with_fake_spawn(false);

    let mut compensation = app.admit_player_intents_with_followups_for_test(&opening);

    assert!(app.video.proc.is_none());
    assert!(app.playback.paused);
    assert!(app.video.paused_audio);
    assert!(compensation.iter().flat_map(Cmd::player_commands).any(
        |command| matches!(command, PlayerCmd::SetProperty { name, value }
            if name == "pause" && value == &serde_json::Value::Bool(false))
    ));

    admit_player_transition(&mut app, &mut compensation);

    assert!(!app.playback.paused);
    assert!(!app.video.paused_audio);
    assert_eq!(app.status.kind, StatusKind::Error);
    assert!(matches!(
        app.status.text.as_str(),
        "Failed to launch mpv" | "mpv 실행에 실패했습니다"
    ));
}

#[test]
fn video_finish_rejection_keeps_process_and_pause_ownership_retryable() {
    for error in [
        crate::util::delivery::DeliveryError::Busy,
        crate::util::delivery::DeliveryError::Closed,
    ] {
        let mut app = app_playing(2, 0);
        app.video.proc = Some(fake_overlay_proc());
        app.playback.paused = true;
        app.video.paused_audio = true;

        let cmds = app.toggle_video_overlay();
        assert!(app.video.proc.is_some());

        reject_player_transition(&mut app, cmds, error);

        assert!(app.video.proc.is_some());
        assert!(app.playback.paused);
        assert!(app.video.paused_audio);
        app.close_video();
    }
}

#[test]
fn video_ipc_failure_releases_overlay_owned_audio_pause_after_admission() {
    let mut app = app_playing(2, 0);
    app.video.proc = Some(fake_overlay_proc());
    app.playback.paused = true;
    app.video.paused_audio = true;
    let generation = app.video.generation;

    let mut cmds = app.update(PlayerMsg::VideoOverlay {
        generation,
        event: VideoEvent::Failed("IPC command write failed".to_owned()),
    });

    assert!(app.video.proc.is_some());
    assert!(app.playback.paused);
    assert!(app.video.paused_audio);
    assert!(cmds.iter().flat_map(Cmd::player_commands).any(|command| {
        matches!(command, PlayerCmd::SetProperty { name, value }
            if name == "pause" && value == &serde_json::Value::Bool(false))
    }));

    admit_player_transition(&mut app, &mut cmds);

    assert!(app.video.proc.is_none());
    assert!(!app.playback.paused);
    assert!(!app.video.paused_audio);
    assert_eq!(app.status.kind, StatusKind::Error);
    assert!(app.status.text.contains("IPC command write failed"));
}

#[test]
fn failed_layout_respawn_reuses_retryable_finish_admission() {
    let mut app = app_playing(2, 0);
    app.video.proc = Some(fake_overlay_proc());
    app.playback.paused = true;
    app.video.paused_audio = true;

    let cmds = app.toggle_video_layout_with_fake_spawn(false);

    assert!(app.video.proc.is_none());
    assert!(app.playback.paused);
    assert!(app.video.paused_audio);
    reject_player_transition(&mut app, cmds, crate::util::delivery::DeliveryError::Busy);
    assert!(app.playback.paused && app.video.paused_audio);

    let mut retry = app.toggle_video_overlay();
    admit_player_transition(&mut app, &mut retry);
    assert!(!app.playback.paused);
    assert!(!app.video.paused_audio);
}

#[test]
fn video_continue_advances_queue_paused_and_loads_next_video() {
    let mut app = app_playing(3, 0);
    // The overlay paused the audio when it opened.
    app.playback.paused = true;
    app.video.paused_audio = true;

    let mut cmds = app.video_continue_next();
    admit_player_transition(&mut app, &mut cmds);

    assert_eq!(current(&app), "id1");
    // The next track loads into the audio engine (position tracking)…
    assert_loads_video(&cmds, "id1");
    // …but both sides stay pinned paused: video owns playback until the overlay closes.
    assert!(app.playback.paused);
    assert!(app.video.paused_audio);
    assert!(cmds.iter().flat_map(Cmd::player_commands).any(|c| matches!(
        c,
        PlayerCmd::SetProperty { name, value }
            if name == "pause" && value == &serde_json::Value::Bool(true)
    )));
    // The same overlay window is asked to show the next track's video.
    assert!(cmds.iter().any(|c| matches!(
        c,
        Cmd::VideoLoad(url) if url == "https://www.youtube.com/watch?v=id1"
    )));
}

#[test]
fn video_continue_at_queue_end_stops_like_audio() {
    let mut app = app_playing(2, 1);
    app.playback.paused = true;
    app.video.paused_audio = true;

    let mut cmds = app.video_continue_next();
    admit_player_transition(&mut app, &mut cmds);

    // Mirrors the audio queue-end: nothing left loaded, mpv told to drop the file.
    assert!(has_stop(&cmds));
    assert!(!cmds.iter().any(|c| matches!(c, Cmd::VideoLoad(_))));
    assert!(app.prefetch.loaded_video_id.is_none());
    // Nothing to resume when the (already closed) overlay state is cleaned up.
    assert!(!app.video.paused_audio);
    assert!(app.playback.paused);
}

#[test]
fn video_continue_repeat_one_reloads_the_same_video() {
    let mut app = app_playing(2, 0);
    app.queue.repeat = crate::queue::Repeat::One;
    app.playback.paused = true;
    app.video.paused_audio = true;

    let mut cmds = app.video_continue_next();
    admit_player_transition(&mut app, &mut cmds);

    assert_eq!(current(&app), "id0");
    assert!(cmds.iter().any(|c| matches!(
        c,
        Cmd::VideoLoad(url) if url == "https://www.youtube.com/watch?v=id0"
    )));
}

#[test]
fn video_event_after_close_is_ignored() {
    let mut app = app_playing(2, 0);
    app.config.auto_continue_videos = Some(true);
    // The overlay was already closed (`v`): a late Eof from its IPC client is stale.
    let generation = app.video.generation;
    let mut cmds = app.update(PlayerMsg::VideoOverlay {
        generation,
        event: VideoEvent::Eof,
    });
    admit_player_transition(&mut app, &mut cmds);
    assert!(cmds.is_empty());
    assert_eq!(current(&app), "id0");
}

#[test]
fn video_eof_with_toggle_off_closes_and_resumes_audio() {
    let mut app = app_playing(2, 0);
    app.video.proc = Some(fake_overlay_proc());
    app.playback.paused = true;
    app.video.paused_audio = true;

    let generation = app.video.generation;
    let mut cmds = app.update(PlayerMsg::VideoOverlay {
        generation,
        event: VideoEvent::Eof,
    });
    admit_player_transition(&mut app, &mut cmds);

    // Toggle off: an ended video reads as a close — window reaped, audio resumed.
    assert!(app.video.proc.is_none());
    assert!(!app.playback.paused);
    assert!(!app.video.paused_audio);
    assert!(cmds.iter().flat_map(Cmd::player_commands).any(|c| matches!(
        c,
        PlayerCmd::SetProperty { name, value }
            if name == "pause" && value == &serde_json::Value::Bool(false)
    )));
    assert_eq!(current(&app), "id0", "no advance with the toggle off");
}

#[test]
fn video_eof_with_toggle_on_keeps_the_window_and_advances() {
    let mut app = app_playing(3, 0);
    app.config.auto_continue_videos = Some(true);
    app.video.proc = Some(fake_overlay_proc());
    app.playback.paused = true;
    app.video.paused_audio = true;

    let generation = app.video.generation;
    let mut cmds = app.update(PlayerMsg::VideoOverlay {
        generation,
        event: VideoEvent::Eof,
    });
    admit_player_transition(&mut app, &mut cmds);

    assert!(
        app.video.proc.is_some(),
        "the window stays open for the next video"
    );
    assert_eq!(current(&app), "id1");
    assert!(cmds.iter().any(|c| matches!(
        c,
        Cmd::VideoLoad(url) if url == "https://www.youtube.com/watch?v=id1"
    )));
    app.close_video();
}

#[test]
fn video_event_from_an_older_generation_is_ignored() {
    let mut app = app_playing(2, 0);
    app.config.auto_continue_videos = Some(true);
    app.video.proc = Some(fake_overlay_proc());
    app.video.generation = 3;

    // A Quit from the window that Shift+V already replaced must not close the new one.
    let cmds = app.update(PlayerMsg::VideoOverlay {
        generation: 2,
        event: VideoEvent::Quit,
    });

    assert!(cmds.is_empty());
    assert!(app.video.proc.is_some());
    app.close_video();
}

#[test]
fn video_next_key_skips_and_shows_the_next_video() {
    let mut app = app_playing(3, 0);
    app.video.proc = Some(fake_overlay_proc());
    app.playback.paused = true;
    app.video.paused_audio = true;

    let generation = app.video.generation;
    let mut cmds = app.update(PlayerMsg::VideoOverlay {
        generation,
        event: VideoEvent::Next,
    });
    admit_player_transition(&mut app, &mut cmds);

    assert_eq!(current(&app), "id1");
    // Audio stays pinned paused under the video; the window shows the landed track.
    assert!(app.playback.paused && app.video.paused_audio);
    assert!(cmds.iter().any(|c| matches!(
        c,
        Cmd::VideoLoad(url) if url == "https://www.youtube.com/watch?v=id1"
    )));
    app.close_video();
}

#[test]
fn video_prev_key_goes_back_a_video() {
    let mut app = app_playing(3, 1);
    app.video.proc = Some(fake_overlay_proc());
    app.playback.paused = true;
    app.video.paused_audio = true;

    let generation = app.video.generation;
    let mut cmds = app.update(PlayerMsg::VideoOverlay {
        generation,
        event: VideoEvent::Prev,
    });
    admit_player_transition(&mut app, &mut cmds);

    assert_eq!(current(&app), "id0");
    assert!(app.playback.paused && app.video.paused_audio);
    assert!(cmds.iter().any(|c| matches!(
        c,
        Cmd::VideoLoad(url) if url == "https://www.youtube.com/watch?v=id0"
    )));
    app.close_video();
}

#[test]
fn video_toggle_pause_event_emits_overlay_pause_command() {
    let mut app = app_playing(2, 0);
    app.video.proc = Some(fake_overlay_proc());
    app.playback.paused = true;
    app.video.paused_audio = true;

    let generation = app.video.generation;
    let cmds = app.update(PlayerMsg::VideoOverlay {
        generation,
        event: VideoEvent::TogglePause,
    });

    assert!(matches!(cmds.as_slice(), [Cmd::VideoTogglePause]));
    assert!(app.playback.paused && app.video.paused_audio);
    app.close_video();
}

#[test]
fn video_pause_property_updates_status_without_resuming_audio() {
    let _guard = crate::i18n::lock_for_test();
    let mut app = app_playing(2, 0);
    app.video.proc = Some(fake_overlay_proc());
    app.playback.paused = true;
    app.video.paused_audio = true;

    let generation = app.video.generation;
    let cmds = app.update(PlayerMsg::VideoOverlay {
        generation,
        event: VideoEvent::Paused(false),
    });

    assert!(cmds.is_empty());
    assert_eq!(app.status.text, "Video playing");
    assert!(app.playback.paused, "audio engine stays pinned paused");
    assert!(app.video.paused_audio);
    app.close_video();
}

#[test]
fn video_close_key_uses_normal_finish_path() {
    let mut app = app_playing(2, 0);
    app.video.proc = Some(fake_overlay_proc());
    app.playback.paused = true;
    app.video.paused_audio = true;

    let generation = app.video.generation;
    let mut cmds = app.update(PlayerMsg::VideoOverlay {
        generation,
        event: VideoEvent::Close,
    });
    admit_player_transition(&mut app, &mut cmds);

    assert!(app.video.proc.is_none());
    assert!(!app.playback.paused);
    assert!(!app.video.paused_audio);
    assert!(cmds.iter().flat_map(Cmd::player_commands).any(|c| matches!(
        c,
        PlayerCmd::SetProperty { name, value }
            if name == "pause" && value == &serde_json::Value::Bool(false)
    )));
}

#[test]
fn video_fullscreen_and_mute_events_emit_overlay_commands() {
    let mut app = app_playing(2, 0);
    app.video.proc = Some(fake_overlay_proc());

    let generation = app.video.generation;
    let fullscreen = app.update(PlayerMsg::VideoOverlay {
        generation,
        event: VideoEvent::ToggleFullscreen,
    });
    assert!(matches!(
        fullscreen.as_slice(),
        [Cmd::VideoToggleFullscreen]
    ));

    let mute = app.update(PlayerMsg::VideoOverlay {
        generation,
        event: VideoEvent::ToggleMute,
    });
    assert!(matches!(mute.as_slice(), [Cmd::VideoToggleMute]));
    app.close_video();
}

// --- Playlist search & import (`Ctrl+P` kind, ytpl: rows) -----------------
