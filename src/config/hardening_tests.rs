use std::path::PathBuf;

use super::*;

#[test]
fn parses_netscape_cookies_youtube_only() {
    let txt = "# Netscape HTTP Cookie File\n\
               .youtube.com\tTRUE\t/\tTRUE\t1999999999\tSAPISID\tsecret1\n\
               #HttpOnly_.youtube.com\tTRUE\t/\tTRUE\t1999999999\tSID\tsecret2\n\
               .example.com\tTRUE\t/\tFALSE\t1999999999\tIGNORED\tnope\n";
    let header = parse_netscape_cookies(txt);
    assert!(header.contains("SAPISID=secret1"));
    assert!(header.contains("SID=secret2"));
    assert!(!header.contains("IGNORED"));
}

#[test]
fn netscape_cookies_reject_lookalike_domains_and_header_breakers() {
    let txt = "# Netscape HTTP Cookie File\n\
               evil-youtube.com\tTRUE\t/\tTRUE\t1999999999\tSAPISID\tbad1\n\
               notyoutube.com\tTRUE\t/\tTRUE\t1999999999\tSID\tbad2\n\
               youtube.com.evil.com\tTRUE\t/\tTRUE\t1999999999\tHSID\tbad3\n\
               .youtube.com\tTRUE\t/\tTRUE\t1999999999\tGOOD\tok;INJECTED=x\n\
               .youtube.com\tTRUE\t/\tTRUE\t1999999999\tSAPISID\tclean\n";
    let header = parse_netscape_cookies(txt);
    assert!(!header.contains("bad1"));
    assert!(!header.contains("bad2"));
    assert!(!header.contains("bad3"));
    assert!(!header.contains("INJECTED"));
    assert!(header.contains("SAPISID=clean"));
}

#[test]
fn default_cookies_file_lives_under_audio_dir() {
    assert_eq!(
        ytm_dir_under_audio_dir(PathBuf::from("/Users/alice/Music")).join("cookies.txt"),
        PathBuf::from("/Users/alice/Music/yututui/cookies.txt")
    );
}

#[test]
fn default_download_dir_lives_under_audio_dir() {
    assert_eq!(
        ytm_dir_under_audio_dir(PathBuf::from("/Users/alice/Music")),
        PathBuf::from("/Users/alice/Music/yututui")
    );
}

#[test]
fn configured_cookies_file_overrides_default() {
    let cfg = Config {
        cookies_file: Some(PathBuf::from("/custom/cookies.txt")),
        ..Config::default()
    };
    assert_eq!(
        cfg.effective_cookies_file(),
        Some(PathBuf::from("/custom/cookies.txt"))
    );
}

#[test]
fn existing_cookies_file_requires_a_present_file() {
    let missing = std::env::temp_dir().join(format!(
        "yututui-missing-cookies-{}-{:?}.txt",
        std::process::id(),
        std::thread::current().id()
    ));
    let _ = std::fs::remove_file(&missing);
    let cfg = Config {
        cookies_file: Some(missing),
        ..Config::default()
    };
    assert_eq!(cfg.existing_cookies_file(), None);
    let (handoff_path, warning) = cfg.cookies_file_for_external_tools_with_warning(None);
    assert_eq!(handoff_path, None);
    assert!(
        warning
            .expect("explicit missing cookies file should warn")
            .contains("Cookies file not used for mpv/yt-dlp")
    );
}

#[test]
fn existing_cookies_file_keeps_a_present_file() {
    let path = std::env::temp_dir().join(format!(
        "yututui-present-cookies-{}-{:?}.txt",
        std::process::id(),
        std::thread::current().id()
    ));
    std::fs::write(&path, "# Netscape HTTP Cookie File\n").unwrap();
    let cfg = Config {
        cookies_file: Some(path.clone()),
        ..Config::default()
    };
    assert_eq!(cfg.existing_cookies_file(), Some(path.clone()));
    assert_eq!(
        cfg.cookies_file_for_external_tools_with_warning(None),
        (Some(path.clone()), None)
    );
    let _ = std::fs::remove_file(path);
}

#[cfg(unix)]
#[test]
fn existing_cookies_file_rejects_symlink() {
    let root = std::env::temp_dir().join(format!(
        "yututui-symlink-cookies-{}-{:?}",
        std::process::id(),
        std::thread::current().id()
    ));
    let _ = std::fs::remove_dir_all(&root);
    std::fs::create_dir_all(&root).unwrap();
    let real = root.join("real.txt");
    let link = root.join("link.txt");
    std::fs::write(&real, "# Netscape HTTP Cookie File\n").unwrap();
    std::os::unix::fs::symlink(&real, &link).unwrap();
    let cfg = Config {
        cookies_file: Some(link),
        ..Config::default()
    };

    assert_eq!(cfg.existing_cookies_file(), None);
    assert_eq!(cfg.cookies_file_for_external_tools(Some(&root)), None);
    let (path, warning) = cfg.cookies_file_for_external_tools_with_warning(Some(&root));
    assert_eq!(path, None);
    assert!(
        warning
            .expect("symlink cookies file should warn")
            .contains("non-symlink")
    );
    let _ = std::fs::remove_dir_all(root);
}

#[test]
fn existing_cookies_file_rejects_oversized_file() {
    let path = std::env::temp_dir().join(format!(
        "yututui-oversized-cookies-{}-{:?}.txt",
        std::process::id(),
        std::thread::current().id()
    ));
    let file = std::fs::File::create(&path).unwrap();
    file.set_len(MAX_COOKIE_BYTES + 1).unwrap();
    let cfg = Config {
        cookies_file: Some(path.clone()),
        ..Config::default()
    };

    assert_eq!(cfg.existing_cookies_file(), None);
    assert_eq!(cfg.cookies_file_for_external_tools(None), None);
    let (handoff_path, warning) = cfg.cookies_file_for_external_tools_with_warning(None);
    assert_eq!(handoff_path, None);
    assert!(
        warning
            .expect("oversized cookies file should warn")
            .contains("under 4 MiB")
    );
    let _ = std::fs::remove_file(path);
}

#[test]
fn external_tool_cookies_are_imported_to_private_data_dir() {
    let root = std::env::temp_dir().join(format!(
        "yututui-import-cookies-{}-{:?}",
        std::process::id(),
        std::thread::current().id()
    ));
    let _ = std::fs::remove_dir_all(&root);
    std::fs::create_dir_all(&root).unwrap();
    let source = root.join("source-cookies.txt");
    let data_dir = root.join("data");
    let body = "# Netscape HTTP Cookie File\n.youtube.com\tTRUE\t/\tTRUE\t1\tSID\tsecret\n";
    std::fs::write(&source, body).unwrap();
    let cfg = Config {
        cookies_file: Some(source.clone()),
        ..Config::default()
    };

    let imported = cfg
        .cookies_file_for_external_tools(Some(&data_dir))
        .expect("valid cookies should import");
    assert_eq!(
        cfg.cookies_file_for_external_tools_with_warning(Some(&data_dir)),
        (Some(imported.clone()), None)
    );

    assert_eq!(imported, data_dir.join(EXTERNAL_COOKIES_COPY));
    assert_eq!(std::fs::read_to_string(&imported).unwrap(), body);
    assert_eq!(
        cfg.player_runtime(Some(imported.clone())).cookies_file,
        Some(imported.clone())
    );
    assert_eq!(
        cfg.download_runtime(Some(imported.clone())).cookies_file,
        Some(imported.clone())
    );
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mode = std::fs::metadata(&imported).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o600);
    }
    let _ = std::fs::remove_dir_all(root);
}

#[test]
fn external_tool_cookies_fall_back_to_strict_source_without_data_dir() {
    let path = std::env::temp_dir().join(format!(
        "yututui-source-cookies-{}-{:?}.txt",
        std::process::id(),
        std::thread::current().id()
    ));
    std::fs::write(&path, "# Netscape HTTP Cookie File\n").unwrap();
    let cfg = Config {
        cookies_file: Some(path.clone()),
        ..Config::default()
    };

    assert_eq!(
        cfg.cookies_file_for_external_tools(None),
        Some(path.clone())
    );
    let _ = std::fs::remove_file(path);
}

#[test]
fn load_from_rotates_secret_recovery_backups() {
    let dir = std::env::temp_dir().join(format!("ytm-cfg-rotate-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let path = dir.join("config.json");

    for idx in 0..(crate::util::safe_fs::SECRET_BACKUP_RETENTION + 2) {
        std::fs::write(&path, format!("not-json-with-secret-{idx}")).unwrap();
        let _ = Config::load_from(&path);
    }

    let backups = crate::util::safe_fs::recovery_backups(&path).unwrap();
    assert_eq!(backups.len(), crate::util::safe_fs::SECRET_BACKUP_RETENTION);
    assert!(path.exists(), "fresh default config should remain in place");

    let _ = std::fs::remove_dir_all(&dir);
}
