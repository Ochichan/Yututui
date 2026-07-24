use std::collections::VecDeque;

use super::*;
use crate::api::PlayableRef;
use crate::playlists::{Playlist, Playlists};
use crate::station::{Explore, StationProfile};

const SECRET: &str = "sk-secret-export-sentinel-123456789";
const SPOTIFY_CLIENT_ID: &str = "spotify-personal-app-export-sentinel";
const JAMENDO_CLIENT_ID: &str = "jamendo-private-client-export-sentinel";
const AUDIUS_APP_NAME: &str = "audius-personal-app-export-sentinel";
const PRIVATE_PATH: &str = "/Users/alice/private/music/secret.flac";
const PLAYABLE_URL: &str = "https://user:password@example.invalid/private-stream";

fn test_directory(label: &str) -> PathBuf {
    let path = std::env::temp_dir().join(format!(
        "yututui-data-export-{label}-{}-{}",
        std::process::id(),
        random_suffix().expect("random suffix")
    ));
    fs::create_dir(&path).expect("create test directory");
    #[cfg(unix)]
    fs::set_permissions(&path, fs::Permissions::from_mode(0o700))
        .expect("make test directory private");
    path
}

fn remote_song() -> Song {
    let mut song = Song::remote("dQw4w9WgXcQ", "Safe title", "Safe artist", "3:32");
    song.origin_url = Some(PLAYABLE_URL.to_owned());
    song.origin_key = Some(SECRET.to_owned());
    song.import_session_id = Some("transfer-private-session".to_owned());
    song.import_source_order = Some(7);
    song.album_art_url = Some(PLAYABLE_URL.to_owned());
    song.playable = Some(PlayableRef::YoutubeVideo {
        id: "dQw4w9WgXcQ".to_owned(),
    });
    song
}

fn secret_config() -> Config {
    let mut config = Config {
        cookie: Some(SECRET.to_owned()),
        cookies_file: Some(PathBuf::from(PRIVATE_PATH)),
        download_dir: Some(PathBuf::from(PRIVATE_PATH)),
        gemini_api_key: Some(SECRET.to_owned()),
        ..Config::default()
    };
    config.local.roots.push(crate::config::LocalRootConfig {
        path: PathBuf::from(PRIVATE_PATH),
        enabled: Some(true),
        recursive: Some(true),
    });
    config.scrobble.lastfm.session_key = Some(SECRET.to_owned());
    config.scrobble.lastfm.api_key = Some(SECRET.to_owned());
    config.scrobble.lastfm.api_secret = Some(SECRET.to_owned());
    config.scrobble.lastfm.username = Some("private-user".to_owned());
    config.scrobble.listenbrainz.token = Some(SECRET.to_owned());
    config.scrobble.listenbrainz.api_url = Some(PLAYABLE_URL.to_owned());
    config.spotify.client_id = Some(SPOTIFY_CLIENT_ID.to_owned());
    config.search.jamendo_client_id = Some(JAMENDO_CLIENT_ID.to_owned());
    config.search.audius_app_name = Some(AUDIUS_APP_NAME.to_owned());
    config.tools.ytdlp_path = Some(PathBuf::from(PRIVATE_PATH));
    config.tools.mpv_path = Some(PathBuf::from(PRIVATE_PATH));
    config.audio.mpv.output = Some(SECRET.to_owned());
    config.audio.mpv.device = Some(PRIVATE_PATH.to_owned());
    config.audio.mpv.extra_args = vec![format!("--cookies={PRIVATE_PATH}")];
    config.audio.mpv.long_form_seek_optimization = crate::config::LongFormSeekOptimization::Auto;
    config.recording.track_directory = Some(PathBuf::from(PRIVATE_PATH));
    let mut local_theme = crate::theme::ThemeConfig::local_launch();
    local_theme.set_preset(crate::theme::ThemePreset::Custom);
    local_theme
        .set_override(crate::theme::ThemeRole::Accent, "#ABCDEF")
        .unwrap();
    config.local_theme = Some(local_theme);
    config
}

fn fixture_snapshot() -> ExportSnapshot {
    let remote = remote_song();
    let local = remote.with_local_path(PathBuf::from(PRIVATE_PATH));
    let library = Library {
        favorites: vec![remote.clone(), local.clone()],
        history: VecDeque::from([local.clone()]),
        radio_favorites: Vec::new(),
        radios: VecDeque::new(),
        ..Library::default()
    };
    let playlists = Playlists {
        playlists: vec![Playlist {
            id: "portable-list".to_owned(),
            name: "Portable list".to_owned(),
            songs: vec![remote, local.clone()],
        }],
        revision: 0,
    };
    let mut signals = Signals::default();
    signals.record_play(&local.video_id, "safe artist", 1.0, 100);
    signals.record_play(
        "local:/Users/alice/unknown.flac",
        "private artist",
        0.5,
        200,
    );
    let station = StationStore {
        active: Some(StationProfile {
            query: "late night drive".to_owned(),
            explore: Explore::Wide,
            avoid_artist_keys: vec!["safe artist".to_owned()],
        }),
    };
    ExportSnapshot::new_at(
        &secret_config(),
        &library,
        &playlists,
        &signals,
        &station,
        1234,
    )
}

#[test]
fn keybinding_export_preserves_slash_and_backslash_chords() {
    let bindings = BTreeMap::from([
        ("library.filter".to_owned(), "/".to_owned()),
        ("library.enqueue".to_owned(), "\\".to_owned()),
        ("library.invalid".to_owned(), "/private/path".to_owned()),
    ]);

    let safe = safe_keybindings(&bindings);

    assert_eq!(safe.get("library.filter").map(String::as_str), Some("/"));
    assert_eq!(safe.get("library.enqueue").map(String::as_str), Some("\\"));
    assert!(!safe.contains_key("library.invalid"));
}

#[test]
fn completed_export_file_name_shape_is_strict() {
    assert!(is_personal_export_file_name(
        "yututui-personal-data-v1-1783704534-0123456789abcdef.json"
    ));
    assert!(is_personal_export_file_name(
        "yututui-personal-data-v2-1783704534-0123456789abcdef.json"
    ));
    for invalid in [
        "yututui-personal-data-v1-1783704534-0123456789ABCDEf.json",
        "yututui-personal-data-v1-now-0123456789abcdef.json",
        "yututui-personal-data-v1-1783704534-short.json",
        "../yututui-personal-data-v1-1783704534-0123456789abcdef.json",
        "yututui-personal-data-v1-1783704534-0123456789abcdef.json.tmp",
    ] {
        assert!(!is_personal_export_file_name(invalid), "accepted {invalid}");
    }
}

#[test]
fn v1_export_decodes_once_as_a_v2_legacy_baseline() {
    let snapshot = fixture_snapshot();
    let bytes = serde_json::to_vec(&snapshot).unwrap();
    let decoded = decode_personal_state_export(&bytes).unwrap();

    assert_eq!(decoded.schema_version, 2);
    assert_eq!(
        crate::personal_state::project(&decoded)
            .unwrap()
            .legacy
            .favorites
            .len(),
        1,
        "local and downloaded copies with the same exact YouTube id collapse"
    );
    assert_eq!(
        decoded
            .operations
            .iter()
            .filter(|operation| matches!(
                operation.operation,
                crate::personal_state::Operation::LegacyBaseline { .. }
            ))
            .count(),
        1
    );
}

#[test]
fn v2_export_round_trips_and_rejects_private_metadata_claims() {
    let state = crate::personal_state::legacy_state(
        &Library::default(),
        &Playlists::default(),
        &Signals::default(),
        &StationStore::default(),
    )
    .unwrap();
    let bytes = serde_json::to_vec(&state).unwrap();
    assert_eq!(decode_personal_state_export(&bytes).unwrap(), state);

    let mut unsafe_state = state;
    unsafe_state.metadata.credentials_included = true;
    let bytes = serde_json::to_vec(&unsafe_state).unwrap();
    assert!(decode_personal_state_export(&bytes).is_err());
}

#[test]
fn projection_is_fail_closed_for_secrets_paths_urls_and_transfer_fields() {
    let snapshot = fixture_snapshot();
    let text = serde_json::to_string_pretty(&snapshot).expect("serialize snapshot");

    for forbidden in [
        SECRET,
        PRIVATE_PATH,
        PLAYABLE_URL,
        "transfer-private-session",
        "origin_url",
        "local_path",
        "import_session_id",
        "cookies_file",
        "gemini_api_key",
        "session_key",
        "api_secret",
        "private-user",
        SPOTIFY_CLIENT_ID,
        JAMENDO_CLIENT_ID,
        AUDIUS_APP_NAME,
    ] {
        assert!(!text.contains(forbidden), "export leaked {forbidden:?}");
    }
    assert!(text.contains("dQw4w9WgXcQ"));
    assert!(text.contains("Safe title"));
    assert_eq!(snapshot.schema_version, EXPORT_SCHEMA_VERSION);
    assert_eq!(snapshot.kind, EXPORT_KIND);
    assert!(!snapshot.privacy.credentials_included);
    assert!(!snapshot.privacy.filesystem_paths_included);
    assert!(snapshot.privacy.contains_listening_history);
    assert_eq!(snapshot.summary.omitted_signal_tracks, 1);
    assert_eq!(snapshot.summary.omitted_signal_events, 1);
    assert_eq!(snapshot.settings.playback["album_art_quality"], "high");
    assert_eq!(
        snapshot.settings.appearance["local_theme"]["preset"],
        "custom"
    );
    assert_eq!(
        snapshot.settings.appearance["local_theme"]["custom_overrides"]["accent"],
        "#ABCDEF"
    );
}

#[test]
fn export_includes_requested_long_form_mode_but_no_runtime_cache_state() {
    let value = serde_json::to_value(fixture_snapshot()).unwrap();
    let mpv = &value["settings"]["audio"]["mpv"];

    assert_eq!(mpv["cache_forward"], "32MiB");
    assert_eq!(mpv["cache_back"], "8MiB");
    assert_eq!(mpv["long_form_seek_optimization"], "auto");
    let text = serde_json::to_string(&value).unwrap();
    for forbidden in [
        "long_form_seek_effective",
        "long_form_seek_reason",
        "cache_root",
        "cache_path",
        "packet_payload",
    ] {
        assert!(!text.contains(forbidden), "export leaked {forbidden}");
    }
}

#[test]
fn writer_uses_unique_private_complete_files_and_cleans_temp_links() {
    let directory = test_directory("writer");
    let snapshot = fixture_snapshot();
    let first = export_snapshot(&directory, &snapshot).expect("first export");
    let second = export_snapshot(&directory, &snapshot).expect("second export");

    assert_ne!(first, second);
    for path in [&first, &second] {
        let bytes = fs::read(path).expect("read export");
        assert!(bytes.ends_with(b"\n"));
        let parsed: Value = serde_json::from_slice(&bytes).expect("valid complete JSON");
        assert_eq!(parsed["schema_version"], EXPORT_SCHEMA_VERSION);
        #[cfg(unix)]
        assert_eq!(
            fs::metadata(path).expect("metadata").permissions().mode() & 0o777,
            0o600
        );
    }
    let names: Vec<String> = fs::read_dir(&directory)
        .expect("list directory")
        .map(|entry| {
            entry
                .expect("directory entry")
                .file_name()
                .to_string_lossy()
                .into_owned()
        })
        .collect();
    assert_eq!(names.len(), 2);
    assert!(names.iter().all(|name| !name.starts_with('.')));
    fs::remove_dir_all(directory).expect("cleanup");
}

#[test]
fn limited_writer_rejects_bytes_past_the_cap_without_partial_chunk() {
    let mut writer = LimitedWriter::new(Vec::new(), 4);
    writer.write_all(b"1234").expect("within cap");
    let error = writer.write_all(b"5").expect_err("past cap");
    assert_eq!(error.kind(), io::ErrorKind::FileTooLarge);
    assert!(writer.exceeded);
    assert_eq!(writer.into_inner(), b"1234");
}

#[cfg(unix)]
#[test]
fn group_or_other_writable_destination_is_rejected() {
    let directory = test_directory("shared-destination");
    fs::set_permissions(&directory, fs::Permissions::from_mode(0o777))
        .expect("make shared fixture");

    let error = export_snapshot(&directory, &fixture_snapshot())
        .expect_err("shared destination must fail closed");

    assert!(matches!(error, ExportError::InvalidDestination(_)));
    assert!(error.to_string().contains("writable by another account"));
    assert!(
        fs::read_dir(&directory)
            .expect("list fixture")
            .next()
            .is_none()
    );
    fs::remove_dir(directory).expect("cleanup");
}

#[cfg(unix)]
#[test]
fn nonsticky_writable_ancestor_is_rejected_before_private_data_is_written() {
    let ancestor = test_directory("shared-ancestor");
    let destination = ancestor.join("private-child");
    fs::create_dir(&destination).expect("create child destination");
    fs::set_permissions(&destination, fs::Permissions::from_mode(0o700))
        .expect("make child private");
    fs::set_permissions(&ancestor, fs::Permissions::from_mode(0o777))
        .expect("make ancestor replaceable");

    let error = export_snapshot(&destination, &fixture_snapshot())
        .expect_err("replaceable ancestor must fail closed");

    assert!(matches!(error, ExportError::InvalidDestination(_)));
    assert!(error.to_string().contains("without sticky protection"));
    assert!(
        fs::read_dir(&destination)
            .expect("list child")
            .next()
            .is_none(),
        "temp file must be removed before any snapshot is published"
    );
    fs::set_permissions(&ancestor, fs::Permissions::from_mode(0o700))
        .expect("restore fixture permissions");
    fs::remove_dir_all(ancestor).expect("cleanup");
}

#[cfg(unix)]
#[test]
fn sticky_writable_ancestor_with_trusted_child_is_allowed() {
    let ancestor = test_directory("sticky-ancestor");
    let destination = ancestor.join("private-child");
    fs::create_dir(&destination).expect("create child destination");
    fs::set_permissions(&destination, fs::Permissions::from_mode(0o700))
        .expect("make child private");
    fs::set_permissions(&ancestor, fs::Permissions::from_mode(0o1777))
        .expect("make ancestor sticky shared");

    let exported = export_snapshot(&destination, &fixture_snapshot())
        .expect("sticky ancestor protects trusted child entry");

    assert!(exported.is_file());
    fs::set_permissions(&ancestor, fs::Permissions::from_mode(0o700))
        .expect("restore fixture permissions");
    fs::remove_dir_all(ancestor).expect("cleanup");
}

#[cfg(unix)]
#[test]
fn symlink_destination_is_rejected() {
    use std::os::unix::fs::symlink;

    let real = test_directory("real");
    let link = real.with_extension("link");
    symlink(&real, &link).expect("create symlink");
    let error = export_snapshot(&link, &fixture_snapshot()).expect_err("reject symlink");
    assert!(matches!(error, ExportError::InvalidDestination(_)));
    assert!(fs::read_dir(&real).expect("list real").next().is_none());
    fs::remove_file(link).expect("remove symlink");
    fs::remove_dir_all(real).expect("cleanup");
}

fn multi_device_personal_state() -> (
    crate::personal_state::PersonalStateV2,
    crate::personal_state::DeviceId,
) {
    use crate::personal_state::{
        CausalStamp, DeviceId, DeviceRecord, Dot, Operation, OperationEnvelope, OperationOrigin,
    };

    let mut state =
        crate::personal_state::PersonalStateV2::empty("multi-device-export".to_owned()).unwrap();
    let mut local_device = None;
    for raw_device_id in ["device-a", "device-b"] {
        let secrets = crate::sync::DeviceSecretMaterial::generate_for(raw_device_id).unwrap();
        let device_id = DeviceId::new(raw_device_id).unwrap();
        local_device.get_or_insert_with(|| device_id.clone());
        let dot = Dot {
            device_id: device_id.clone(),
            sequence: 1,
        };
        let observed = state.version_vector.clone();
        state.operations.push(OperationEnvelope {
            operation_id: format!("add-{raw_device_id}"),
            stamp: CausalStamp {
                dot: dot.clone(),
                observed,
                recorded_at_unix: 1,
            },
            origin: OperationOrigin::Local,
            operation: Operation::AddDevice {
                device: DeviceRecord {
                    device_id,
                    name: raw_device_id.to_owned(),
                    revoked: false,
                    public_identity: Some(secrets.public_identity()),
                },
            },
        });
        state.version_vector.observe(&dot);
    }
    crate::personal_state::refresh_device_registry(&mut state).unwrap();
    state.normalize().unwrap();
    (state, local_device.expect("fixture has a local device"))
}

#[test]
fn v2_export_uses_explicit_enrolled_device_for_multi_device_state() {
    let directory = test_directory("multi-device-v2");
    let (state, local_device) = multi_device_personal_state();
    let library = Library::default();
    let playlists = Playlists::default();
    let signals = Signals::default();
    let station = StationStore::default();

    assert!(
        reconcile_v2_sources(&state, None, &library, &playlists, &signals, &station,).is_err(),
        "multi-device state must never guess which device owns a new causal dot"
    );
    let exported = export_v2_from_sources(
        &directory,
        &state,
        Some(&local_device),
        &library,
        &playlists,
        &signals,
        &station,
    )
    .expect("explicit enrolled device permits v2 export");
    let decoded: crate::personal_state::PersonalStateV2 =
        serde_json::from_slice(&fs::read(exported).unwrap()).unwrap();
    assert_eq!(decoded.device_registry, state.device_registry);

    fs::remove_dir_all(directory).expect("cleanup");
}
