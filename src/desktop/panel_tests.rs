use super::*;
use crate::desktop::control::ControlError;
use crate::desktop::menu_model::TrayState;
use crate::queue::Repeat;
use crate::remote::proto::{InstanceMode, QueueItemSnapshot, StatusSnapshot};

fn playing_update() -> PollUpdate {
    PollUpdate::connected(StatusSnapshot {
        title: Some("Song".to_string()),
        artist: Some("Artist".to_string()),
        paused: false,
        volume: 80,
        position: 1,
        total: 2,
        streaming: true,
        owner_mode: InstanceMode::StandaloneTui,
        settings: Default::default(),
        queue: vec![
            QueueItemSnapshot {
                title: "Song".to_string(),
                artist: "Artist".to_string(),
                duration: "3:00".to_string(),
                current: true,
            },
            QueueItemSnapshot {
                title: "Next".to_string(),
                artist: "Other".to_string(),
                duration: "4:00".to_string(),
                current: false,
            },
        ],
        shuffle: true,
        repeat: Repeat::All,
        elapsed_ms: Some(42_000),
        duration_ms: Some(180_000),
        is_live: false,
        queue_rev: Some(41),
        track_id: Some("song-id".to_string()),
        position_epoch: 7,
        artwork: None,
    })
}

fn english_payload(update: &PollUpdate) -> PanelPayload {
    payload_for_update_with_language(update, Language::English)
}

#[test]
fn playing_payload_enables_transport_and_volume() {
    let payload = english_payload(&playing_update());
    assert!(payload.connected);
    assert_eq!(payload.title, "Song");
    assert_eq!(payload.artist, "Artist");
    assert_eq!(payload.state_label, "Playing");
    assert_eq!(payload.owner_label, "Standalone TUI");
    assert_eq!(payload.queue_label, "1 / 2");
    assert_eq!(payload.volume_label, "80%");
    assert_eq!(payload.volume, 80);
    assert_eq!(payload.elapsed_ms, Some(42_000));
    assert_eq!(payload.duration_ms, Some(180_000));
    assert_eq!(payload.queue_rev, Some(41));
    assert!(payload.can_seek);
    assert_eq!(payload.queue.len(), 2);
    assert_eq!(payload.queue[0].title, "Song");
    assert!(payload.shuffle);
    assert_eq!(payload.repeat, "all");
    assert_eq!(payload.repeat_label, "All");
    assert!(payload.can_playback);
    assert!(payload.can_volume);
    assert!(payload.can_manage_queue);
    assert!(payload.can_toggle_streaming);
    assert!(payload.settings.can_radio_mode);
    assert_eq!(payload.settings.streaming_mode, "balanced");
    assert_eq!(payload.settings.streaming_source, "youtube");
    assert!(!payload.can_start_daemon);
    assert!(!payload.can_resume_daemon);
    assert!(!payload.can_stop_daemon);
}

#[test]
fn legacy_queue_without_revision_is_read_only() {
    let mut update = playing_update();
    let TrayState::Connected(status) = &mut update.state else {
        panic!("playing fixture must be connected");
    };
    status.queue_rev = None;

    let payload = english_payload(&update);
    assert!(!payload.queue.is_empty());
    assert_eq!(payload.queue_rev, None);
    assert!(!payload.can_manage_queue);
}

#[test]
fn live_badge_uses_explicit_signal_not_missing_duration() {
    let mut unknown_duration = playing_update();
    {
        let TrayState::Connected(status) = &mut unknown_duration.state else {
            panic!("playing fixture must be connected");
        };
        status.duration_ms = None;
        status.is_live = false;
    }
    let unknown_payload = english_payload(&unknown_duration);
    assert_eq!(unknown_payload.duration_ms, None);
    assert!(!unknown_payload.is_live);

    let TrayState::Connected(status) = &mut unknown_duration.state else {
        panic!("playing fixture must be connected");
    };
    status.is_live = true;
    let live_payload = english_payload(&unknown_duration);
    assert!(live_payload.is_live);
    assert!(update_script(&unknown_duration).contains(r#""isLive":true"#));
    assert!(PANEL_HTML.contains("payload.isLive === true"));
    assert!(!PANEL_HTML.contains("payload.durationMs == null && payload.elapsedMs != null"));
}

#[test]
fn explicit_track_identity_distinguishes_ids_and_position_epochs() {
    let mut update = playing_update();
    let original = payload::track_identity(&update.state);

    let TrayState::Connected(status) = &mut update.state else {
        panic!("playing fixture must be connected");
    };
    status.title = Some("Changed ICY metadata".to_string());
    status.artist = Some("Same station".to_string());
    assert_eq!(payload::track_identity(&update.state), original);

    let TrayState::Connected(status) = &mut update.state else {
        panic!("playing fixture must be connected");
    };
    status.track_id = Some("different-id".to_string());
    let different_track = payload::track_identity(&update.state);
    assert_ne!(different_track, original);

    let TrayState::Connected(status) = &mut update.state else {
        panic!("playing fixture must be connected");
    };
    status.track_id = Some("song-id".to_string());
    status.position_epoch += 1;
    assert_ne!(payload::track_identity(&update.state), original);
}

#[test]
fn disconnected_payload_only_enables_resume_when_a_session_is_available() {
    let update = PollUpdate::disconnected(ControlError::NotRunning);
    let payload = english_payload(&update);
    assert!(!payload.connected);
    assert_eq!(payload.title, "YuTuTui! is not running");
    assert_eq!(payload.owner_label, "Offline");
    assert_eq!(payload.queue_label, "Queue unavailable");
    assert!(!payload.can_playback);
    assert!(!payload.can_volume);
    assert!(!payload.can_seek);
    assert_eq!(payload.volume, 0);
    assert_eq!(payload.elapsed_ms, None);
    assert!(payload.can_start_daemon);
    assert!(!payload.can_resume_daemon);
    assert!(!payload.can_stop_daemon);
    assert_eq!(payload.error, Some("YuTuTui! is not running".to_string()));

    let resumable = PollUpdate::disconnected_with_resume(ControlError::NotRunning, true);
    let resumable_payload = english_payload(&resumable);
    assert!(resumable_payload.can_start_daemon);
    assert!(resumable_payload.can_resume_daemon);
}

#[test]
fn idle_daemon_payload_enables_resume_and_stop() {
    let update = PollUpdate::connected(StatusSnapshot {
        title: None,
        artist: None,
        paused: true,
        volume: 70,
        position: 0,
        total: 0,
        streaming: false,
        owner_mode: InstanceMode::Daemon,
        settings: Default::default(),
        queue: Vec::new(),
        shuffle: false,
        repeat: Default::default(),
        elapsed_ms: None,
        duration_ms: None,
        is_live: false,
        queue_rev: Some(0),
        track_id: None,
        position_epoch: 0,
        artwork: None,
    });
    let payload = english_payload(&update);
    assert_eq!(payload.title, "Nothing playing");
    assert_eq!(payload.state_label, "Idle");
    assert_eq!(payload.owner_label, "Daemon");
    assert!(!payload.can_playback);
    assert!(payload.can_volume);
    assert!(!payload.can_start_daemon);
    assert!(payload.can_resume_daemon);
    assert!(payload.can_stop_daemon);
    assert!(!payload.can_manage_queue);
    assert!(!payload.settings.can_radio_mode);
}

#[test]
fn ipc_message_parses_panel_command() {
    assert_eq!(
        parse_ipc_message(r#"{"action":"play_pause"}"#).unwrap(),
        PanelCommand::PlayPause
    );
    assert_eq!(
        PanelCommand::PlayPause.menu_action(),
        Some(MenuAction::PlayPause)
    );
    assert_eq!(PanelCommand::Hide.menu_action(), None);
}

#[test]
fn panel_ipc_v1_correlates_and_validates_requests() {
    assert_eq!(
        parse_ipc_request(r#"{"v":1,"id":42,"command":{"action":"set_volume","value":68}}"#)
            .unwrap(),
        PanelRequest {
            id: Some(42),
            command: PanelCommand::SetVolume(68),
        }
    );
    assert_eq!(
            parse_ipc_request(
                r#"{"v":1,"id":43,"command":{"action":"queue_play","value":{"position":2,"expectedRev":41}}}"#
            )
            .unwrap(),
            PanelRequest {
                id: Some(43),
                command: PanelCommand::QueuePlay {
                    position: 2,
                    expected_rev: Some(41),
                },
            }
        );
    for invalid in [
        r#"{"v":2,"id":42,"command":{"action":"play_pause"}}"#,
        r#"{"v":1,"id":0,"command":{"action":"play_pause"}}"#,
        r#"{"v":1,"id":42,"command":{"action":"set_volume","value":101}}"#,
        r#"{"v":1,"id":42,"command":{"action":"play_pause"},"extra":true}"#,
        r#"{"v":1,"id":42,"command":{"action":"queue_play","value":{"position":2}}}"#,
        r#"{"v":1,"id":42,"command":{"action":"queue_remove","value":{"position":2}}}"#,
    ] {
        assert!(parse_ipc_request(invalid).is_err(), "accepted {invalid}");
    }
}

#[test]
fn command_result_script_is_correlated_and_structured() {
    let error = DesktopCommandError::new("stale_rev", "Queue changed", true);
    let script = command_result_script(7, Some(&error));
    assert!(script.contains("ytmTuiCommandResult"));
    assert!(script.contains(r#""id":7"#));
    assert!(script.contains(r#""code":"stale_rev""#));
    assert!(script.contains(r#""displayMessage":"Queue changed""#));
    assert!(script.contains(r#""retryable":true"#));
}

#[test]
fn panel_setting_commands_map_to_remote_commands() {
    assert_eq!(
        PanelCommand::SetStreamingMode(StreamingMode::Focused).remote_command(),
        Some(RemoteCommand::SetSetting {
            change: RemoteSettingChange::StreamingMode {
                value: StreamingMode::Focused
            }
        })
    );
    assert_eq!(
        PanelCommand::SetRadioMode(true).remote_command(),
        Some(RemoteCommand::SetSetting {
            change: RemoteSettingChange::RadioMode {
                state: ToggleState::On
            }
        })
    );
    assert_eq!(
        PanelCommand::QueuePlay {
            position: 2,
            expected_rev: Some(41),
        }
        .remote_command(),
        Some(RemoteCommand::QueuePlayIfRevision {
            position: 2,
            expected_rev: 41,
        })
    );
    assert_eq!(
        PanelCommand::QueueRemove {
            position: 1,
            expected_rev: Some(42),
        }
        .remote_command(),
        Some(RemoteCommand::QueueRemoveIfRevision {
            position: 1,
            expected_rev: 42,
        })
    );
}

#[test]
fn panel_queue_commands_fall_back_only_without_a_revision() {
    assert_eq!(
        PanelCommand::QueuePlay {
            position: 2,
            expected_rev: None,
        }
        .remote_command(),
        Some(RemoteCommand::QueuePlay { position: 2 })
    );
    assert_eq!(
        PanelCommand::QueueRemove {
            position: 1,
            expected_rev: None,
        }
        .remote_command(),
        Some(RemoteCommand::QueueRemove { position: 1 })
    );
}

#[test]
fn ipc_message_parses_setting_commands() {
    assert_eq!(
        parse_ipc_message(r#"{"action":"set_streaming","value":true}"#).unwrap(),
        PanelCommand::SetStreaming(true)
    );
    assert_eq!(
        parse_ipc_message(r#"{"action":"set_streaming_mode","value":"discovery"}"#).unwrap(),
        PanelCommand::SetStreamingMode(StreamingMode::Discovery)
    );
    assert_eq!(
        parse_ipc_message(r#"{"action":"set_streaming_source","value":"jamendo"}"#).unwrap(),
        PanelCommand::SetStreamingSource(SearchSource::Jamendo)
    );
    assert_eq!(
        parse_ipc_message(r#"{"action":"set_speed","value":12}"#).unwrap(),
        PanelCommand::SetSpeed(12)
    );
    assert_eq!(
        parse_ipc_message(r#"{"action":"toggle_shuffle"}"#).unwrap(),
        PanelCommand::ToggleShuffle
    );
    assert_eq!(
        parse_ipc_message(r#"{"action":"cycle_repeat"}"#).unwrap(),
        PanelCommand::CycleRepeat
    );
    assert_eq!(
        parse_ipc_message(r#"{"action":"queue_play","value":{"position":2,"expectedRev":41}}"#)
            .unwrap(),
        PanelCommand::QueuePlay {
            position: 2,
            expected_rev: Some(41),
        }
    );
    assert_eq!(
        parse_ipc_message(r#"{"action":"queue_remove","value":{"position":1}}"#).unwrap(),
        PanelCommand::QueueRemove {
            position: 1,
            expected_rev: None,
        }
    );
}

#[test]
fn ipc_message_parses_volume_seek_and_drag() {
    assert_eq!(
        parse_ipc_message(r#"{"action":"set_volume","value":63}"#).unwrap(),
        PanelCommand::SetVolume(63)
    );
    // The WebView boundary validates rather than silently changing intent.
    assert_eq!(
        parse_ipc_message(r#"{"action":"set_volume","value":100}"#).unwrap(),
        PanelCommand::SetVolume(100)
    );
    assert!(parse_ipc_message(r#"{"action":"set_volume","value":-4}"#).is_err());
    assert!(parse_ipc_message(r#"{"action":"set_volume","value":101}"#).is_err());
    assert_eq!(
        parse_ipc_message(r#"{"action":"seek_to","value":91500}"#).unwrap(),
        PanelCommand::SeekTo(91_500)
    );
    assert_eq!(
        parse_ipc_message(r#"{"action":"drag"}"#).unwrap(),
        PanelCommand::Drag
    );

    assert_eq!(
        PanelCommand::SetVolume(63).remote_command(),
        Some(RemoteCommand::SetVolume { percent: 63 })
    );
    assert_eq!(
        PanelCommand::SeekTo(91_500).remote_command(),
        Some(RemoteCommand::SeekTo { ms: 91_500 })
    );
    assert_eq!(PanelCommand::Drag.remote_command(), None);
    assert_eq!(PanelCommand::Drag.menu_action(), None);
}

#[test]
fn scripts_escape_html_script_endings() {
    let mut update = playing_update();
    if let TrayState::Connected(status) = &mut update.state {
        status.title = Some("</script><script>alert(1)</script><!--".to_string());
    }
    let html = html(&update, PanelTheme::Default, None);
    assert!(html.contains(r"\u003c/script>\u003cscript>alert(1)"));
    assert!(html.contains(r"\u003c!--"));
    assert!(!html.contains("</script><script>alert"));

    let script = update_script(&update);
    assert!(script.contains(r"\u003c/script>\u003cscript>alert(1)"));
    assert!(script.contains(r"\u003c!--"));
    assert!(!script.contains("</script><script>alert"));
}

#[test]
fn panel_html_has_exactly_one_payload_slot() {
    assert_eq!(PANEL_HTML.matches("__INITIAL_PAYLOAD__").count(), 1);
}

#[test]
fn idle_standalone_payload_cannot_resume() {
    let mut update = playing_update();
    if let TrayState::Connected(status) = &mut update.state {
        status.title = None;
        status.artist = None;
        status.total = 0;
        status.queue.clear();
    }
    let payload = english_payload(&update);
    assert_eq!(payload.state_label, "Idle");
    assert!(
        !payload.can_resume_daemon,
        "resume against a standalone TUI always dead-ends"
    );
    assert!(!payload.can_start_daemon);
}

#[test]
fn panel_html_exposes_mode_switch_and_bars() {
    // The Music/Radio switch and both interactive bars must survive redesigns.
    assert!(PANEL_HTML.contains("data-mode=\"music\""));
    assert!(PANEL_HTML.contains("data-mode=\"radio\""));
    assert!(PANEL_HTML.contains("data-action=\"set_radio_mode\""));
    assert!(PANEL_HTML.contains("id=\"volumeBar\""));
    assert!(PANEL_HTML.contains("id=\"progressBar\""));
    assert!(PANEL_HTML.contains("addEventListener(\"wheel\""));
    assert!(PANEL_HTML.contains("\"set_volume\""));
    assert!(PANEL_HTML.contains("\"seek_to\""));
    assert!(PANEL_HTML.contains("send(\"drag\")"));
}

#[test]
fn panel_html_exposes_queue_and_play_modes() {
    assert!(PANEL_HTML.contains("data-tab=\"queue\""));
    assert!(PANEL_HTML.contains("data-action=\"toggle_shuffle\""));
    assert!(PANEL_HTML.contains("data-action=\"cycle_repeat\""));
    assert!(PANEL_HTML.contains("data-action=\"queue_play\""));
    assert!(PANEL_HTML.contains("data-action=\"queue_remove\""));
    assert!(PANEL_HTML.contains("queueCommandValue"));
    assert!(PANEL_HTML.contains("expectedRev"));
}

#[test]
fn cushion_uses_the_three_level_information_architecture() {
    for tab in ["now", "queue", "more"] {
        assert!(
            PANEL_HTML.contains(&format!(r#"data-tab="{tab}""#)),
            "missing {tab} tab"
        );
        assert!(
            PANEL_HTML.contains(&format!(r#"data-panel="{tab}""#)),
            "missing {tab} panel"
        );
    }
    assert_eq!(PANEL_HTML.matches(r#"role="tab""#).count(), 3);
    assert!(!PANEL_HTML.contains(r#"data-tab="streaming""#));
    assert!(!PANEL_HTML.contains(r#"data-tab="playback""#));
}

#[test]
fn fixed_svg_icons_and_cushion_artwork_replace_font_glyph_controls() {
    assert!(PANEL_HTML.contains("Lucide Contributors"));
    for icon in [
        "icon-play",
        "icon-pause",
        "icon-skip-back",
        "icon-skip-forward",
        "icon-shuffle",
        "icon-repeat",
        "icon-volume",
        "icon-pin",
        "icon-x",
    ] {
        assert!(PANEL_HTML.contains(icon), "missing {icon}");
    }
    assert!(PANEL_HTML.contains(r#"id="artImg""#));
    for legacy_entity in ["&#9654;", "&#9664;", "&#10005;", "&#128251;"] {
        assert!(
            !PANEL_HTML.contains(legacy_entity),
            "font-dependent control glyph survived: {legacy_entity}"
        );
    }
}

#[test]
fn theme_ids_round_trip() {
    for theme in PanelTheme::ALL {
        assert_eq!(PanelTheme::from_id(theme.id()), Some(theme));
    }
    assert_eq!(PanelTheme::from_id("bogus"), None);
}

#[test]
fn theme_ids_are_substitution_safe() {
    // Ids are spliced verbatim into the page's data-theme attribute.
    for theme in PanelTheme::ALL {
        assert!(
            theme
                .id()
                .chars()
                .all(|c| c.is_ascii_lowercase() || c == '_'),
            "{} is not attribute-safe",
            theme.id()
        );
    }
}

#[test]
fn window_size_expands_only_minimal() {
    for theme in PanelTheme::ALL {
        let collapsed = theme.window_size(false);
        let expanded = theme.window_size(true);
        if theme == PanelTheme::Minimal {
            assert!(expanded.1 > collapsed.1);
            assert_eq!(expanded.0, collapsed.0, "expansion only grows downward");
        } else {
            assert_eq!(expanded, collapsed);
        }
    }
}

#[test]
fn ipc_message_parses_theme_commands() {
    assert_eq!(
        parse_ipc_message(r#"{"action":"set_theme","value":"minimal"}"#).unwrap(),
        PanelCommand::SetTheme(PanelTheme::Minimal)
    );
    assert_eq!(
        parse_ipc_message(r#"{"action":"set_theme","value":"tamagotchi"}"#).unwrap(),
        PanelCommand::SetTheme(PanelTheme::Tamagotchi)
    );
    assert!(parse_ipc_message(r#"{"action":"set_theme","value":"bogus"}"#).is_err());
    assert!(parse_ipc_message(r#"{"action":"set_theme"}"#).is_err());
    assert_eq!(
        parse_ipc_message(r#"{"action":"set_expanded","value":true}"#).unwrap(),
        PanelCommand::SetExpanded(true)
    );
    assert!(parse_ipc_message(r#"{"action":"set_expanded","value":"yes"}"#).is_err());
    assert_eq!(
        parse_ipc_message(r#"{"action":"set_shared_sheet","value":true}"#).unwrap(),
        PanelCommand::SetSharedSheet(Some(PanelSheet::Queue))
    );
    assert_eq!(
        parse_ipc_message(r#"{"action":"set_shared_sheet","value":"more"}"#).unwrap(),
        PanelCommand::SetSharedSheet(Some(PanelSheet::More))
    );
    assert_eq!(
        parse_ipc_message(r#"{"action":"set_shared_sheet","value":false}"#).unwrap(),
        PanelCommand::SetSharedSheet(None)
    );
    assert!(parse_ipc_message(r#"{"action":"set_shared_sheet","value":"yes"}"#).is_err());
    assert_eq!(
        parse_ipc_message(r#"{"action":"set_pinned","value":true}"#).unwrap(),
        PanelCommand::SetPinned(true)
    );
    assert!(parse_ipc_message(r#"{"action":"set_pinned","value":1}"#).is_err());
    assert_eq!(
        parse_ipc_message(
            r#"{"action":"persist_ui","value":{"queueScrollY":421,"activeControl":"queue-remove-2"}}"#
        )
        .unwrap(),
        PanelCommand::PersistUi(PanelUiSnapshot {
            queue_scroll_y: 421,
            active_control: Some("queue-remove-2".to_string()),
        })
    );
    assert!(
        parse_ipc_message(
            r#"{"action":"persist_ui","value":{"queueScrollY":10000001,"activeControl":null}}"#
        )
        .is_err()
    );
    let long_id = "x".repeat(129);
    assert!(
        parse_ipc_message(&format!(
            r#"{{"action":"persist_ui","value":{{"queueScrollY":0,"activeControl":"{long_id}"}}}}"#
        ))
        .is_err()
    );
}

#[test]
fn ipc_numeric_values_are_bounded() {
    assert_eq!(
        parse_ipc_message(r#"{"action":"queue_play","value":{"position":998}}"#).unwrap(),
        PanelCommand::QueuePlay {
            position: 998,
            expected_rev: None,
        }
    );
    for message in [
        r#"{"action":"set_speed","value":4}"#,
        r#"{"action":"set_speed","value":21}"#,
        r#"{"action":"set_seek_seconds","value":0}"#,
        r#"{"action":"set_seek_seconds","value":61}"#,
        r#"{"action":"seek_to","value":604800001}"#,
        r#"{"action":"queue_play","value":{"position":999}}"#,
        r#"{"action":"queue_remove","value":{"position":999}}"#,
        r#"{"action":"queue_play","value":2}"#,
        r#"{"action":"queue_play","value":{"position":2,"expectedRev":-1}}"#,
    ] {
        assert!(parse_ipc_message(message).is_err(), "accepted {message}");
    }
    let oversized = parse_ipc_message(&"x".repeat(4097)).unwrap_err();
    assert!(oversized.to_string().contains("too large"));
}

#[test]
fn theme_commands_stay_tray_local() {
    // Skin changes must never produce socket traffic or menu actions.
    for command in [
        PanelCommand::SetTheme(PanelTheme::Minimal),
        PanelCommand::SetExpanded(true),
        PanelCommand::SetSharedSheet(Some(PanelSheet::Queue)),
        PanelCommand::PersistUi(PanelUiSnapshot::default()),
        PanelCommand::SetPinned(true),
    ] {
        assert_eq!(command.remote_command(), None);
        assert_eq!(command.menu_action(), None);
    }
}

#[test]
fn panel_html_exposes_theme_switching() {
    // The CSS switch rules and the shared picker must survive redesigns.
    assert!(PANEL_HTML.contains(r#"html[data-theme="minimal"]"#));
    assert!(PANEL_HTML.contains(r#"html[data-theme="tamagotchi"]"#));
    assert!(PANEL_HTML.contains(r#"data-action="set_theme""#));
    for theme in PanelTheme::ALL {
        assert!(
            PANEL_HTML.contains(&format!(r#"data-value="{}""#, theme.id())),
            "picker misses {}",
            theme.id()
        );
    }
}

#[test]
fn minimal_theme_discloses_the_common_controls() {
    assert!(PANEL_HTML.contains(r#"id="playerRoot""#));
    assert!(PANEL_HTML.contains(r#"id="compactMenu""#));
    assert!(PANEL_HTML.contains(".player-shell.expanded .volume-row"));
    assert!(PANEL_HTML.contains(".player-shell.expanded .progress-row"));
    assert!(PANEL_HTML.contains(r#""set_expanded""#));
}

#[test]
fn compact_skins_reuse_the_shared_queue_and_more_tree() {
    assert!(PANEL_HTML.contains(r#"id="tabQueue""#));
    assert!(PANEL_HTML.contains(r#"id="tabMore""#));
    assert!(PANEL_HTML.contains(r#"id="sharedSheetBar""#));
    assert!(PANEL_HTML.contains(r#"send("set_shared_sheet", sheet)"#));
    assert!(PANEL_HTML.contains(r#"send("set_shared_sheet", false)"#));
    assert!(!PANEL_HTML.contains("data-sheet="));
    assert_eq!(
        PANEL_HTML
            .matches(r#"<section class="tab-panel" data-panel="queue""#)
            .count(),
        1,
        "compact skins must not clone queue controls"
    );
    assert_eq!(
        PANEL_HTML
            .matches(r#"<section class="tab-panel" data-panel="more""#)
            .count(),
        1,
        "compact skins must not clone settings controls"
    );
}

#[test]
fn every_skin_styles_one_semantic_transport_tree() {
    assert!(PANEL_HTML.contains(r#"id="sharedTransport""#));
    assert!(PANEL_HTML.contains(r#"id="transportSlot""#));
    assert!(!PANEL_HTML.contains("projectTransportForTheme"));
    assert_eq!(PANEL_HTML.matches(r#"id="transportSlot""#).count(), 1);
    for action in ["previous", "play_pause", "next"] {
        assert_eq!(
            PANEL_HTML
                .matches(&format!(r#"data-action="{action}""#))
                .count(),
            1,
            "transport action {action} must have one semantic control"
        );
    }
    for legacy_id in ["mnPrev", "mnPlay", "mnNext", "tmPrev", "tmPlay", "tmNext"] {
        assert!(
            !PANEL_HTML.contains(&format!(r#"id="{legacy_id}""#)),
            "legacy duplicate transport control {legacy_id} remains"
        );
    }
}

#[test]
fn every_skin_uses_one_semantic_control_tree() {
    for id in [
        "playerRoot",
        "stateLabel",
        "title",
        "artist",
        "artImg",
        "recovery",
        "progressBar",
        "volumeBar",
        "shuffle",
        "repeat",
        "pin",
        "hide",
        "error",
        "tabQueue",
        "tabMore",
        "panelQueue",
        "panelMore",
    ] {
        assert_eq!(
            PANEL_HTML.matches(&format!(r#"id="{id}""#)).count(),
            1,
            "semantic control #{id} must be mounted exactly once"
        );
    }
    assert_eq!(
        PANEL_HTML
            .matches(r#"class="segmented theme-pick""#)
            .count(),
        1,
        "theme selection must remain in the common semantic tree"
    );
    for duplicate_root in ["minimalRoot", "tamaRoot", "mnCapsule", "tmMenu"] {
        assert!(
            !PANEL_HTML.contains(&format!(r#"id="{duplicate_root}""#)),
            "duplicate skin-owned semantic root #{duplicate_root} remains"
        );
    }
}

#[test]
fn panel_html_keeps_dynamic_locale_live_and_correlated_command_hooks() {
    assert!(PANEL_HTML.contains("copyByLocale"));
    assert!(PANEL_HTML.contains("function setLocale(nextLocale)"));
    assert!(PANEL_HTML.contains("document.documentElement.lang = locale"));
    assert!(PANEL_HTML.contains(".bar.live .fill"));
    assert!(PANEL_HTML.contains(".bar.live .knob"));
    assert!(PANEL_HTML.contains("pendingRequests"));
    assert!(PANEL_HTML.contains("window.ytmTuiCommandResult"));
    assert!(PANEL_HTML.contains("window.ytmTuiFocusPrimary"));
}

#[test]
fn panel_html_has_exactly_one_theme_slot() {
    assert_eq!(PANEL_HTML.matches("__PANEL_THEME__").count(), 1);
    assert!(PANEL_HTML.contains(r#"data-theme="__PANEL_THEME__""#));
    assert_eq!(PANEL_HTML.matches("__INITIAL_PINNED__").count(), 1);
    assert_eq!(PANEL_HTML.matches("__INITIAL_EXPANDED__").count(), 1);
    assert_eq!(PANEL_HTML.matches("__INITIAL_SHARED_SHEET__").count(), 1);
    assert_eq!(PANEL_HTML.matches("__INITIAL_QUEUE_SCROLL_Y__").count(), 1);
    assert_eq!(PANEL_HTML.matches("__INITIAL_ACTIVE_CONTROL__").count(), 1);
    assert_eq!(PANEL_HTML.matches("__PANEL_LANG__").count(), 1);
    assert_eq!(PANEL_HTML.matches("__PANEL_LOCALE__").count(), 1);
}

#[test]
fn html_bakes_the_selected_theme() {
    let page = html(&playing_update(), PanelTheme::Minimal, None);
    assert!(page.contains(r#"data-theme="minimal""#));
    assert!(!page.contains("__PANEL_THEME__"));
    assert!(!page.contains("__CSP_NONCE__"));
    assert!(page.contains("Content-Security-Policy"));
    assert!(page.contains("window.__YTM_TUI_INITIAL_PINNED__ = false;"));

    let pinned = html_with_pinned(&playing_update(), PanelTheme::Minimal, None, true);
    assert!(pinned.contains("window.__YTM_TUI_INITIAL_PINNED__ = true;"));

    let restored = html_with_state(
        &playing_update(),
        PanelTheme::Minimal,
        None,
        true,
        true,
        Some(PanelSheet::More),
    );
    assert!(restored.contains("window.__YTM_TUI_INITIAL_EXPANDED__ = true;"));
    assert!(restored.contains("window.__YTM_TUI_INITIAL_SHARED_SHEET__ = \"more\";"));

    let restored_ui = html_with_panel_ui_state(
        &playing_update(),
        PanelTheme::Minimal,
        None,
        true,
        true,
        Some(PanelSheet::Queue),
        &PanelUiSnapshot {
            queue_scroll_y: 421,
            active_control: Some("queue-remove-2".to_string()),
        },
    );
    assert!(restored_ui.contains("window.__YTM_TUI_INITIAL_QUEUE_SCROLL_Y__ = 421;"));
    assert!(
        restored_ui.contains("window.__YTM_TUI_INITIAL_ACTIVE_CONTROL__ = \"queue-remove-2\";")
    );
}

#[test]
fn panel_html_renders_english_and_korean_locales() {
    let english = html_with_language(
        &playing_update(),
        PanelTheme::Default,
        None,
        false,
        false,
        None,
        Language::English,
    );
    assert!(english.contains(r#"<html lang="en""#));
    assert!(english.contains(r#""stateLabel":"Playing""#));
    assert!(english.contains(r#""repeatLabel":"All""#));

    let korean = html_with_language(
        &playing_update(),
        PanelTheme::Default,
        None,
        false,
        false,
        None,
        Language::Korean,
    );
    assert!(korean.contains(r#"<html lang="ko""#));
    assert!(korean.contains(r#""stateLabel":"재생 중""#));
    assert!(korean.contains(r#""repeatLabel":"전체""#));
    assert!(korean.contains(r#"window.__YTM_TUI_LOCALE__ = "ko";"#));
}

#[test]
fn panel_html_exposes_tamagotchi_pet_and_screen() {
    assert!(PANEL_HTML.contains(r#"id="tamaVisual" aria-hidden="true""#));
    assert!(PANEL_HTML.contains(r#"id="tmScreen""#));
    assert!(PANEL_HTML.contains(r#"id="tmMarquee""#));
    assert_eq!(PANEL_HTML.matches(r#"id="volumeBar""#).count(), 1);
    assert_eq!(PANEL_HTML.matches(r#"id="progressBar""#).count(), 1);
    // The pet state machine and its LCD look must survive redesigns.
    assert!(PANEL_HTML.contains(r#"[data-pet="dance"]"#));
    assert!(PANEL_HTML.contains(r#"[data-pet="sleep"]"#));
    assert!(PANEL_HTML.contains(r#"[data-pet="off"]"#));
    assert!(PANEL_HTML.contains(r#"shape-rendering="crispEdges""#));
    assert!(PANEL_HTML.contains("image-rendering: pixelated"));
}

#[test]
fn panel_html_has_exactly_one_art_slot() {
    assert_eq!(PANEL_HTML.matches("__INITIAL_ART__").count(), 1);
    assert!(PANEL_HTML.contains("ytmTuiApplyArt"));
}

#[test]
fn panel_html_exposes_keyboard_and_screen_reader_contracts() {
    for contract in [
        r#"role="slider""#,
        r#"role="tab""#,
        r#"role="tabpanel""#,
        r#"aria-live="assertive""#,
        "aria-current",
        "prefers-reduced-motion",
        "forced-colors: active",
        "event.key !== \"Escape\"",
        "requestQueueRemove",
    ] {
        assert!(PANEL_HTML.contains(contract), "missing {contract}");
    }
}

#[test]
fn html_bakes_initial_art() {
    let uri = "data:image/png;base64,iVBORw0KGgo=";
    let page = html(&playing_update(), PanelTheme::Minimal, Some(uri));
    assert!(page.contains(&format!("window.__YTM_TUI_INITIAL_ART__ = \"{uri}\";")));

    let artless = html(&playing_update(), PanelTheme::Minimal, None);
    assert!(artless.contains("window.__YTM_TUI_INITIAL_ART__ = null;"));
    assert!(!artless.contains("__INITIAL_ART__"));
}

#[test]
fn art_data_uri_sniffs_common_formats() {
    assert!(art_data_uri(&[0xFF, 0xD8, 0xFF, 0x00]).starts_with("data:image/jpeg;base64,"));
    assert!(art_data_uri(b"\x89PNG\r\n\x1a\n").starts_with("data:image/png;base64,"));
    assert!(art_data_uri(b"RIFF\x00\x00\x00\x00WEBPVP8 ").starts_with("data:image/webp;base64,"));
    assert!(art_data_uri(b"not an image").starts_with("data:application/octet-stream;base64,"));
}

#[test]
fn art_script_splices_or_clears() {
    assert_eq!(
        art_script(None),
        "window.ytmTuiApplyArt && window.ytmTuiApplyArt(null);"
    );
    let script = art_script(Some("data:image/png;base64,AA=="));
    assert_eq!(
        script,
        "window.ytmTuiApplyArt && window.ytmTuiApplyArt(\"data:image/png;base64,AA==\");"
    );
}

#[test]
fn load_art_data_uri_handles_missing_and_oversized() {
    let dir = std::env::temp_dir();
    assert_eq!(
        load_art_data_uri(&dir.join("ytt-panel-art-missing.bin")),
        None
    );

    let ok_path = dir.join(format!("ytt-panel-art-ok-{}.bin", std::process::id()));
    std::fs::write(&ok_path, [0xFF, 0xD8, 0xFF, 0x00]).unwrap();
    let uri = load_art_data_uri(&ok_path);
    std::fs::remove_file(&ok_path).ok();
    assert!(uri.unwrap().starts_with("data:image/jpeg;base64,"));

    let corrupt_path = dir.join(format!("ytt-panel-art-corrupt-{}.bin", std::process::id()));
    std::fs::write(&corrupt_path, b"not an image").unwrap();
    let corrupt = load_art_data_uri(&corrupt_path);
    std::fs::remove_file(&corrupt_path).ok();
    assert_eq!(corrupt, None);

    let big_path = dir.join(format!("ytt-panel-art-big-{}.bin", std::process::id()));
    std::fs::write(&big_path, vec![0u8; (MAX_PANEL_ART_BYTES + 1) as usize]).unwrap();
    let rejected = load_art_data_uri(&big_path);
    std::fs::remove_file(&big_path).ok();
    assert_eq!(rejected, None);
}
