//! Golden corpus for the frozen v7 one-shot wire (docs/gui/02 §2, §20).
//!
//! These literals are the byte shapes the shipped v1.5.9 binaries emit and parse
//! (`src/remote/proto.rs` was unchanged between the v1.5.9 tag and the v8 split, so
//! serializing with the split types reproduces the shipped wire exactly). Two guarantees:
//!
//! 1. **Parse forever**: every literal a v7 client can send still deserializes.
//! 2. **Byte-stable replies**: what we serialize for v7-visible shapes is byte-identical
//!    to what v1.5.9 produced — additive fields must be `skip_serializing_if` so they
//!    never appear here.
//!
//! If one of these tests fails, the change breaks shipped `ytt -r` / tray binaries.
//! Fix the change, never the literal.

use super::*;

fn cmd_of(line: &str) -> RemoteCommand {
    let req: RemoteRequest =
        serde_json::from_str(line).unwrap_or_else(|e| panic!("golden line failed: {e}\n{line}"));
    assert_eq!(req.version, 7, "corpus is v7: {line}");
    assert_eq!(req.token, "tok");
    req.command
}

#[test]
fn golden_v7_command_lines_parse_forever() {
    let cases: &[(&str, RemoteCommand)] = &[
        (
            r#"{"version":7,"token":"tok","command":{"cmd":"next"}}"#,
            RemoteCommand::Next,
        ),
        (
            r#"{"version":7,"token":"tok","command":{"cmd":"prev"}}"#,
            RemoteCommand::Prev,
        ),
        (
            r#"{"version":7,"token":"tok","command":{"cmd":"toggle_pause"}}"#,
            RemoteCommand::TogglePause,
        ),
        (
            r#"{"version":7,"token":"tok","command":{"cmd":"play","query":"lofi"}}"#,
            RemoteCommand::Play {
                query: "lofi".to_string(),
            },
        ),
        (
            r#"{"version":7,"token":"tok","command":{"cmd":"enqueue","query":"jazz"}}"#,
            RemoteCommand::Enqueue {
                query: "jazz".to_string(),
            },
        ),
        (
            r#"{"version":7,"token":"tok","command":{"cmd":"volume_up"}}"#,
            RemoteCommand::VolumeUp,
        ),
        (
            r#"{"version":7,"token":"tok","command":{"cmd":"volume_down"}}"#,
            RemoteCommand::VolumeDown,
        ),
        (
            r#"{"version":7,"token":"tok","command":{"cmd":"set_volume","percent":42}}"#,
            RemoteCommand::SetVolume { percent: 42 },
        ),
        (
            r#"{"version":7,"token":"tok","command":{"cmd":"seek_back"}}"#,
            RemoteCommand::SeekBack,
        ),
        (
            r#"{"version":7,"token":"tok","command":{"cmd":"seek_forward"}}"#,
            RemoteCommand::SeekForward,
        ),
        (
            r#"{"version":7,"token":"tok","command":{"cmd":"seek_to","ms":91500}}"#,
            RemoteCommand::SeekTo { ms: 91_500 },
        ),
        (
            r#"{"version":7,"token":"tok","command":{"cmd":"toggle_shuffle"}}"#,
            RemoteCommand::ToggleShuffle,
        ),
        (
            r#"{"version":7,"token":"tok","command":{"cmd":"cycle_repeat"}}"#,
            RemoteCommand::CycleRepeat,
        ),
        (
            r#"{"version":7,"token":"tok","command":{"cmd":"queue_play","position":3}}"#,
            RemoteCommand::QueuePlay { position: 3 },
        ),
        (
            r#"{"version":7,"token":"tok","command":{"cmd":"queue_remove","position":0}}"#,
            RemoteCommand::QueueRemove { position: 0 },
        ),
        (
            r#"{"version":7,"token":"tok","command":{"cmd":"streaming","state":"toggle"}}"#,
            RemoteCommand::Streaming {
                state: ToggleState::Toggle,
            },
        ),
        // The pre-rename alias shipped in old tray panels must parse forever.
        (
            r#"{"version":7,"token":"tok","command":{"cmd":"radio","state":"on"}}"#,
            RemoteCommand::Streaming {
                state: ToggleState::On,
            },
        ),
        (
            r#"{"version":7,"token":"tok","command":{"cmd":"resume_session"}}"#,
            RemoteCommand::ResumeSession,
        ),
        (
            r#"{"version":7,"token":"tok","command":{"cmd":"status"}}"#,
            RemoteCommand::Status,
        ),
        (
            r#"{"version":7,"token":"tok","command":{"cmd":"quit"}}"#,
            RemoteCommand::Quit,
        ),
    ];
    for (line, expect) in cases {
        assert_eq!(&cmd_of(line), expect, "line: {line}");
    }
}

#[test]
fn golden_v7_setting_change_lines_parse_forever() {
    let cases: &[(&str, RemoteSettingChange)] = &[
        (
            r#"{"setting":"autoplay_streaming","value":true}"#,
            RemoteSettingChange::AutoplayStreaming { value: true },
        ),
        (
            r#"{"setting":"streaming_mode","value":"Focused"}"#,
            RemoteSettingChange::StreamingMode {
                value: StreamingMode::Focused,
            },
        ),
        (
            r#"{"setting":"streaming_source","value":"radio_browser"}"#,
            RemoteSettingChange::StreamingSource {
                value: SearchSource::RadioBrowser,
            },
        ),
        (
            r#"{"setting":"speed","tenths":15}"#,
            RemoteSettingChange::Speed { tenths: 15 },
        ),
        (
            r#"{"setting":"seek_seconds","seconds":5}"#,
            RemoteSettingChange::SeekSeconds { seconds: 5 },
        ),
        (
            r#"{"setting":"normalize","value":false}"#,
            RemoteSettingChange::Normalize { value: false },
        ),
        (
            r#"{"setting":"gapless","value":true}"#,
            RemoteSettingChange::Gapless { value: true },
        ),
        (
            r#"{"setting":"ai_enabled","value":false}"#,
            RemoteSettingChange::AiEnabled { value: false },
        ),
        (
            r#"{"setting":"radio_mode","state":"off"}"#,
            RemoteSettingChange::RadioMode {
                state: ToggleState::Off,
            },
        ),
    ];
    for (line, expect) in cases {
        let change: RemoteSettingChange = serde_json::from_str(line).unwrap();
        assert_eq!(&change, expect, "line: {line}");
        let wrapped = format!(
            r#"{{"version":7,"token":"tok","command":{{"cmd":"set_setting","change":{line}}}}}"#
        );
        assert_eq!(
            cmd_of(&wrapped),
            RemoteCommand::SetSetting { change: *expect },
            "wrapped: {wrapped}"
        );
    }
}

#[test]
fn golden_v7_request_serialization_is_byte_stable() {
    let req = RemoteRequest {
        version: 7,
        token: "tok".to_string(),
        command: RemoteCommand::TogglePause,
    };
    assert_eq!(
        serde_json::to_string(&req).unwrap(),
        r#"{"version":7,"token":"tok","command":{"cmd":"toggle_pause"}}"#
    );
}

#[test]
fn golden_v7_response_serialization_is_byte_stable() {
    assert_eq!(
        serde_json::to_string(&RemoteResponse::err("bad_version")).unwrap(),
        r#"{"ok":false,"reason":"bad_version"}"#
    );
    assert_eq!(
        serde_json::to_string(&RemoteResponse::ok("pong".to_string())).unwrap(),
        r#"{"ok":true,"reason":"ok","message":"pong"}"#
    );
}

#[test]
fn golden_v7_status_response_is_byte_stable() {
    let snap = StatusSnapshot {
        title: Some("Song".to_string()),
        artist: Some("Artist".to_string()),
        paused: false,
        volume: 55,
        position: 2,
        total: 3,
        streaming: true,
        owner_mode: InstanceMode::Daemon,
        settings: SettingsSnapshot {
            autoplay_streaming: true,
            streaming_mode: StreamingMode::Balanced,
            streaming_source: SearchSource::Youtube,
            speed_tenths: 10,
            seek_seconds: 10,
            normalize: false,
            gapless: true,
            ai_enabled: false,
            radio_mode: false,
        },
        queue: vec![QueueItemSnapshot {
            title: "Song".to_string(),
            artist: "Artist".to_string(),
            duration: "3:14".to_string(),
            current: true,
        }],
        shuffle: true,
        repeat: Repeat::All,
        elapsed_ms: Some(61_500),
        duration_ms: Some(194_000),
        is_live: false,
        queue_rev: None,
        track_id: None,
        position_epoch: 0,
        artwork: None,
    };
    let line = serde_json::to_string(&RemoteResponse::status(snap)).unwrap();
    assert_eq!(
        line,
        "{\"ok\":true,\"reason\":\"ok\",\"message\":\"[playing] Song — Artist  •  vol 55%  •  2/3  •  streaming on\",\
\"status\":{\"title\":\"Song\",\"artist\":\"Artist\",\"paused\":false,\"volume\":55,\"position\":2,\"total\":3,\
\"streaming\":true,\"owner_mode\":\"daemon\",\"settings\":{\"autoplay_streaming\":true,\"streaming_mode\":\"Balanced\",\
\"streaming_source\":\"youtube\",\"speed_tenths\":10,\"seek_seconds\":10,\"normalize\":false,\"gapless\":true,\
\"ai_enabled\":false,\"radio_mode\":false},\"queue\":[{\"title\":\"Song\",\"artist\":\"Artist\",\"duration\":\"3:14\",\
\"current\":true}],\"shuffle\":true,\"repeat\":\"all\",\"elapsed_ms\":61500,\"duration_ms\":194000}}"
    );
}

#[test]
fn golden_v8_status_artwork_is_additive() {
    let artless = StatusSnapshot {
        title: None,
        artist: None,
        paused: true,
        volume: 30,
        position: 0,
        total: 0,
        streaming: false,
        owner_mode: InstanceMode::StandaloneTui,
        settings: SettingsSnapshot::default(),
        queue: Vec::new(),
        shuffle: false,
        repeat: Repeat::Off,
        elapsed_ms: None,
        duration_ms: None,
        is_live: false,
        queue_rev: None,
        track_id: None,
        position_epoch: 0,
        artwork: None,
    };
    // Absent artwork never appears on the wire (v7 byte stability).
    let artless_line = serde_json::to_string(&artless).unwrap();
    assert!(!artless_line.contains("artwork"));
    assert!(!artless_line.contains("is_live"));
    assert!(!artless_line.contains("queue_rev"));
    assert!(!artless_line.contains("track_id"));
    assert!(!artless_line.contains("position_epoch"));

    // Present artwork serializes as a nested ref with `mime` omitted when unknown,
    // and round-trips.
    let with_art = StatusSnapshot {
        artwork: Some(ArtworkRef {
            key: "vid".to_string(),
            path: Some("/tmp/vid.jpg".to_string()),
            mime: None,
        }),
        ..artless
    };
    let line = serde_json::to_string(&with_art).unwrap();
    assert!(line.contains(r#""artwork":{"key":"vid","path":"/tmp/vid.jpg"}"#));
    let back: StatusSnapshot = serde_json::from_str(&line).unwrap();
    assert_eq!(back, with_art);
}

#[test]
fn status_live_signal_is_additive_and_explicit() {
    let legacy = r#"{"title":"Loading","artist":"Artist","paused":false,"volume":50,"position":1,"total":1,"streaming":false,"duration_ms":null}"#;
    let parsed: StatusSnapshot = serde_json::from_str(legacy).unwrap();
    assert_eq!(parsed.duration_ms, None);
    assert!(!parsed.is_live);
    assert_eq!(parsed.queue_rev, None);
    assert_eq!(parsed.track_id, None);
    assert_eq!(parsed.position_epoch, 0);

    let mut live = parsed;
    live.is_live = true;
    live.queue_rev = Some(41);
    live.track_id = Some("station-id".to_string());
    live.position_epoch = 7;
    let line = serde_json::to_string(&live).unwrap();
    assert!(line.contains(r#""is_live":true"#));
    assert!(line.contains(r#""queue_rev":41"#));
    assert!(line.contains(r#""track_id":"station-id""#));
    assert!(line.contains(r#""position_epoch":7"#));
}

#[test]
fn golden_v7_status_lines_parse_forever() {
    // As emitted by a fully-populated v1.5.9 server (byte shape above).
    let line = "{\"title\":\"Song\",\"artist\":\"Artist\",\"paused\":false,\"volume\":55,\"position\":2,\"total\":3,\
\"streaming\":true,\"owner_mode\":\"daemon\",\"settings\":{\"autoplay_streaming\":true,\"streaming_mode\":\"Balanced\",\
\"streaming_source\":\"youtube\",\"speed_tenths\":10,\"seek_seconds\":10,\"normalize\":false,\"gapless\":true,\
\"ai_enabled\":false,\"radio_mode\":false},\"queue\":[{\"title\":\"Song\",\"artist\":\"Artist\",\"duration\":\"3:14\",\
\"current\":true}],\"shuffle\":true,\"repeat\":\"all\",\"elapsed_ms\":61500,\"duration_ms\":194000}";
    let snap: StatusSnapshot = serde_json::from_str(line).unwrap();
    assert_eq!(snap.owner_mode, InstanceMode::Daemon);
    assert_eq!(snap.queue.len(), 1);
    assert_eq!(snap.repeat, Repeat::All);
    assert_eq!(snap.elapsed_ms, Some(61_500));

    // The very old pre-`streaming` rename shape ("radio") must also keep parsing.
    let old = r#"{"title":null,"artist":null,"paused":true,"volume":30,"position":0,"total":0,"radio":false}"#;
    let snap: StatusSnapshot = serde_json::from_str(old).unwrap();
    assert!(!snap.streaming);
    assert_eq!(snap.owner_mode, InstanceMode::StandaloneTui);
}

#[test]
fn golden_v7_instance_file_is_byte_stable_and_parses() {
    let file = InstanceFile {
        app_pid: 7,
        endpoint: "sock".to_string(),
        token: "tok".to_string(),
        created_unix: 1,
        mode: InstanceMode::StandaloneTui,
        protocol_version: 7,
        capabilities: vec!["remote-control".to_string(), "status".to_string()],
    };
    let line = serde_json::to_string(&file).unwrap();
    assert_eq!(
        line,
        r#"{"app_pid":7,"endpoint":"sock","token":"tok","created_unix":1,"mode":"standalone_tui","protocol_version":7,"capabilities":["remote-control","status"]}"#
    );
    let back: InstanceFile = serde_json::from_str(&line).unwrap();
    assert_eq!(back.protocol_version, 7);
    assert_eq!(back.capabilities.len(), 2);
}

#[test]
fn v7_lines_with_unknown_future_fields_still_parse() {
    // Additive-evolution tolerance: serde ignores unknown fields on every v7 shape, so a
    // newer peer adding fields can never break an old-shaped consumer built from this code.
    let req = r#"{"version":7,"token":"tok","command":{"cmd":"status"},"future_field":true}"#;
    assert!(serde_json::from_str::<RemoteRequest>(req).is_ok());

    let resp = r#"{"ok":true,"reason":"ok","message":"m","future":{"deep":1}}"#;
    assert!(serde_json::from_str::<RemoteResponse>(resp).is_ok());

    let snap = r#"{"title":null,"artist":null,"paused":false,"volume":1,"position":0,"total":0,"streaming":false,"brand_new":"x"}"#;
    assert!(serde_json::from_str::<StatusSnapshot>(snap).is_ok());

    let inst = r#"{"app_pid":7,"endpoint":"s","token":"t","created_unix":1,"sessions":9}"#;
    assert!(serde_json::from_str::<InstanceFile>(inst).is_ok());
}
