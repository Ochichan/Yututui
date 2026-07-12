use super::*;

#[test]
fn dormant_cache_policy_preserves_unknown_raw_json_without_rewrite() {
    let dir = std::env::temp_dir().join(format!("ytm-cache-dormant-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let path = dir.join("config.json");
    let original = serde_json::to_vec_pretty(&serde_json::json!({
        "volume": 55,
        "unknown_top": {"future": [1, 2, 3]},
        "audio": {
            "unknown_audio": "keep",
            "mpv": {
                "cache_forward": MPV_CACHE_FORWARD_LEGACY_DEFAULT,
                "cache_back": MPV_CACHE_BACK_LEGACY_DEFAULT,
                "unknown_mpv": {"also": "keep"}
            }
        }
    }))
    .unwrap();
    std::fs::write(&path, &original).unwrap();

    let cfg = Config::load_from(&path);
    assert_eq!(cfg.volume, 55);
    assert_eq!(cfg.audio.mpv.cache_defaults_revision, 0);
    assert_eq!(cfg.audio.mpv.cache_forward, MPV_CACHE_FORWARD_DEFAULT);
    assert_eq!(cfg.audio.mpv.cache_back, MPV_CACHE_BACK_DEFAULT);
    assert_eq!(std::fs::read(&path).unwrap(), original);

    let disk: serde_json::Value = serde_json::from_slice(&original).unwrap();
    assert!(
        disk["audio"]["mpv"]
            .get("_cache_defaults_revision")
            .is_none()
    );
    assert_eq!(
        disk["unknown_top"],
        serde_json::json!({"future": [1, 2, 3]})
    );
    assert_eq!(disk["audio"]["unknown_audio"], "keep");
    assert_eq!(
        disk["audio"]["mpv"]["unknown_mpv"],
        serde_json::json!({"also": "keep"})
    );
    let _ = std::fs::remove_dir_all(dir);
}

#[test]
fn replayed_cache_snapshot_is_installed_without_premature_marker() {
    let dir = std::env::temp_dir().join(format!("ytm-cache-replay-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let path = dir.join("config.json");
    std::fs::write(
        &path,
        serde_json::to_vec_pretty(&Config::default()).unwrap(),
    )
    .unwrap();

    let mut replay = serde_json::to_value(Config::default()).unwrap();
    replay["volume"] = serde_json::json!(42);
    replay["unknown_journal"] = serde_json::json!({"survives": true});
    replay["audio"]["mpv"]["unknown_journal_mpv"] = serde_json::json!([1, 2, 3]);
    replay["audio"]["mpv"]
        .as_object_mut()
        .unwrap()
        .remove("_cache_defaults_revision");
    replay["audio"]["mpv"]["cache_forward"] = serde_json::json!("64MiB");
    replay["audio"]["mpv"]["cache_back"] = serde_json::json!("3MiB");
    crate::persist::write_test_journaled_snapshot(
        crate::persist::StoreKind::Config,
        path.clone(),
        serde_json::to_vec_pretty(&replay).unwrap(),
    )
    .unwrap();

    let cfg = Config::load_from(&path);
    assert_eq!(cfg.volume, 42);
    assert_eq!(cfg.audio.mpv.cache_forward, "64MiB");
    assert_eq!(cfg.audio.mpv.cache_back, "3MiB");
    assert_eq!(cfg.audio.mpv.cache_defaults_revision, 0);
    assert!(!crate::persist::test_journal_exists(&path));

    let installed_bytes = std::fs::read(&path).unwrap();
    let installed: Config = serde_json::from_slice(&installed_bytes).unwrap();
    assert_eq!(installed.volume, 42);
    assert_eq!(installed.audio.mpv.cache_defaults_revision, 0);
    let raw: serde_json::Value = serde_json::from_slice(&installed_bytes).unwrap();
    assert!(
        raw["audio"]["mpv"]
            .get("_cache_defaults_revision")
            .is_none()
    );
    assert_eq!(
        raw["unknown_journal"],
        serde_json::json!({"survives": true})
    );
    assert_eq!(
        raw["audio"]["mpv"]["unknown_journal_mpv"],
        serde_json::json!([1, 2, 3])
    );
    let _ = std::fs::remove_dir_all(dir);
}
