use std::path::PathBuf;

fn env_dir(name: &str) -> Option<PathBuf> {
    std::env::var(name)
        .ok()
        .map(|value| value.trim().to_owned())
        .filter(|value| !value.is_empty())
        .map(PathBuf::from)
}

pub fn config_dir() -> Option<PathBuf> {
    env_dir("YTM_DESKTOP_CONFIG_DIR").or_else(|| {
        directories::ProjectDirs::from("", "", "yututray")
            .map(|dirs| dirs.config_dir().to_path_buf())
    })
}

pub fn cache_dir() -> Option<PathBuf> {
    env_dir("YTM_DESKTOP_CACHE_DIR").or_else(|| {
        directories::ProjectDirs::from("", "", "yututray")
            .map(|dirs| dirs.cache_dir().to_path_buf())
    })
}

pub fn data_dir() -> Option<PathBuf> {
    env_dir("YTM_DESKTOP_DATA_DIR").or_else(|| {
        directories::ProjectDirs::from("", "", "yututray")
            .map(|dirs| dirs.data_local_dir().to_path_buf())
    })
}

#[cfg(any(windows, test))]
fn ensure_webview_data_dir_under(root: &std::path::Path) -> std::io::Result<PathBuf> {
    let directory = root.join("WebView2");
    crate::util::safe_fs::ensure_private_dir(&directory)?;
    Ok(directory)
}

#[cfg(target_os = "windows")]
pub(crate) fn webview_data_dir() -> std::io::Result<PathBuf> {
    let root = data_dir().ok_or_else(|| {
        std::io::Error::new(
            std::io::ErrorKind::NotFound,
            "could not resolve yututray data directory",
        )
    })?;
    ensure_webview_data_dir_under(&root)
}

pub fn initialize_writer() -> std::io::Result<crate::persist::PersistenceAccess> {
    crate::persist::initialize_persistence_writer_for_roots(
        [config_dir(), cache_dir(), data_dir()]
            .into_iter()
            .flatten(),
        false,
    )
}

pub(crate) fn legacy_config_dir() -> Option<PathBuf> {
    directories::ProjectDirs::from("", "", "yututui").map(|dirs| dirs.config_dir().to_path_buf())
}

#[cfg(test)]
mod tests {
    use std::path::{Path, PathBuf};

    use crate::desktop::window_state::{DesktopState, Point, WindowRect};

    fn test_root() -> PathBuf {
        let mut random = [0_u8; 8];
        getrandom::fill(&mut random).unwrap();
        let suffix = random
            .into_iter()
            .map(|byte| format!("{byte:02x}"))
            .collect::<String>();
        std::env::temp_dir().join(format!(
            "yututray-persistence-domain-{}-{suffix}",
            std::process::id()
        ))
    }

    fn flat_snapshot(directory: &Path) -> Vec<(String, Vec<u8>)> {
        let mut entries = std::fs::read_dir(directory)
            .unwrap()
            .map(|entry| {
                let entry = entry.unwrap();
                let name = entry.file_name().to_string_lossy().into_owned();
                let bytes = if name == ".ytt-persistence-writer.lock" {
                    Vec::new()
                } else {
                    std::fs::read(entry.path()).unwrap()
                };
                (name, bytes)
            })
            .collect::<Vec<_>>();
        entries.sort_by(|left, right| left.0.cmp(&right.0));
        entries
    }

    #[test]
    fn default_tray_roots_are_disjoint_from_playback_owner_roots() {
        let tray = directories::ProjectDirs::from("", "", "yututray").unwrap();
        let playback = directories::ProjectDirs::from("", "", "yututui").unwrap();

        assert_ne!(tray.config_dir(), playback.config_dir());
        assert_ne!(tray.cache_dir(), playback.cache_dir());
        assert_ne!(tray.data_local_dir(), playback.data_local_dir());
        assert!(!tray.config_dir().starts_with(playback.config_dir()));
        assert!(!tray.cache_dir().starts_with(playback.cache_dir()));
        assert!(!tray.data_local_dir().starts_with(playback.data_local_dir()));
    }

    #[test]
    fn tray_state_save_under_concurrent_owner_leases_leaves_core_trees_unchanged() {
        let root = test_root();
        let core_config = root.join("core-config");
        let core_cache = root.join("core-cache");
        let core_data = root.join("core-data");
        let tray_config = root.join("tray-config");
        let tray_cache = root.join("tray-cache");
        let tray_data = root.join("tray-data");
        std::fs::create_dir_all(&core_config).unwrap();
        std::fs::create_dir_all(&core_cache).unwrap();
        std::fs::create_dir_all(&core_data).unwrap();
        std::fs::write(core_config.join("config.json"), b"core-config-sentinel").unwrap();
        std::fs::write(core_cache.join("cache.bin"), b"core-cache-sentinel").unwrap();
        std::fs::write(core_data.join("WebView2.sentinel"), b"core-data-sentinel").unwrap();

        let state = DesktopState {
            main: Some(WindowRect {
                x: 10,
                y: 20,
                w: 800,
                h: 600,
                maximized: false,
            }),
            mini: Some(Point { x: 30, y: 40 }),
            mini_pinned: false,
            placement_v2: Default::default(),
            close_to_tray: false,
            keep_webview_alive: true,
            mini_theme: Some("glass".to_owned()),
        };

        crate::persist::with_test_writer_domain([&core_config, &core_cache, &core_data], || {
            let core_config_before = flat_snapshot(&core_config);
            let core_cache_before = flat_snapshot(&core_cache);
            let core_data_before = flat_snapshot(&core_data);
            crate::persist::with_test_writer_domain(
                [&tray_config, &tray_cache, &tray_data],
                || {
                    state.save_to(&tray_config.join("desktop.json"))?;
                    let webview_dir = super::ensure_webview_data_dir_under(&tray_data)?;
                    std::fs::write(webview_dir.join("tray.sentinel"), b"tray-data")?;
                    assert!(tray_config.join(".ytt-persistence-writer.lock").exists());
                    assert!(tray_cache.join(".ytt-persistence-writer.lock").exists());
                    assert!(tray_data.join(".ytt-persistence-writer.lock").exists());
                    Ok(())
                },
            )?;
            assert_eq!(flat_snapshot(&core_config), core_config_before);
            assert_eq!(flat_snapshot(&core_cache), core_cache_before);
            assert_eq!(flat_snapshot(&core_data), core_data_before);
            Ok(())
        })
        .unwrap();

        let restored: DesktopState = crate::util::safe_fs::load_json_or_default_limited(
            &tray_config.join("desktop.json"),
            1024 * 1024,
        );
        assert_eq!(restored, state);
        assert_eq!(
            std::fs::read(core_config.join("config.json")).unwrap(),
            b"core-config-sentinel"
        );
        assert_eq!(
            std::fs::read(core_cache.join("cache.bin")).unwrap(),
            b"core-cache-sentinel"
        );
        assert_eq!(
            std::fs::read(core_data.join("WebView2.sentinel")).unwrap(),
            b"core-data-sentinel"
        );
        let _ = std::fs::remove_dir_all(root);
    }
}
