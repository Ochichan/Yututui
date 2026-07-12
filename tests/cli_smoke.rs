use std::path::{Path, PathBuf};
use std::process::Command;

fn run(args: &[&str]) -> std::process::Output {
    Command::new(env!("CARGO_BIN_EXE_ytt"))
        .args(args)
        .output()
        .expect("ytt command should run")
}

fn isolated_root(name: &str) -> PathBuf {
    std::env::temp_dir().join(format!("ytt-cli-smoke-{name}-{}", std::process::id()))
}

fn isolated_command(root: &Path, args: &[&str]) -> Command {
    let runtime = root.join("runtime");
    std::fs::create_dir_all(&runtime).expect("isolated runtime dir should be creatable");
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&runtime, std::fs::Permissions::from_mode(0o700))
            .expect("isolated runtime dir should be private");
    }

    let mut command = Command::new(env!("CARGO_BIN_EXE_ytt"));
    command
        .args(args)
        .env("HOME", root)
        .env("APPDATA", root.join("config"))
        .env("LOCALAPPDATA", root.join("local"))
        .env("YTM_CONFIG_DIR", root.join("config"))
        .env("YTM_CACHE_DIR", root.join("cache"))
        .env("XDG_CONFIG_HOME", root.join("config"))
        .env("XDG_DATA_HOME", root.join("data"))
        .env("XDG_CACHE_HOME", root.join("cache"))
        .env("XDG_RUNTIME_DIR", runtime)
        .env("YTM_TOOLS_DIR", root.join("tools"))
        .env("LANG", "C")
        .env("LC_ALL", "C")
        .env_remove("YTM_YTDLP")
        .env_remove("YTM_MPV");
    command
}

fn stdout(output: &std::process::Output) -> String {
    String::from_utf8_lossy(&output.stdout).into_owned()
}

fn stderr(output: &std::process::Output) -> String {
    String::from_utf8_lossy(&output.stderr).into_owned()
}

fn snapshot_tree(root: &Path) -> Vec<(PathBuf, Vec<u8>)> {
    fn visit(root: &Path, current: &Path, out: &mut Vec<(PathBuf, Vec<u8>)>) {
        let Ok(entries) = std::fs::read_dir(current) else {
            return;
        };
        for entry in entries {
            let entry = entry.expect("snapshot directory entry");
            let path = entry.path();
            let file_type = entry.file_type().expect("snapshot file type");
            if file_type.is_dir() {
                visit(root, &path, out);
            } else if file_type.is_file() {
                out.push((
                    path.strip_prefix(root)
                        .expect("snapshot path below root")
                        .to_path_buf(),
                    std::fs::read(&path).expect("snapshot file bytes"),
                ));
            }
        }
    }

    let mut out = Vec::new();
    visit(root, root, &mut out);
    out.sort_by(|left, right| left.0.cmp(&right.0));
    out
}

#[test]
fn top_level_help_and_version_exit_before_tui_startup() {
    let help = run(&["--help"]);
    assert!(help.status.success(), "stderr: {}", stderr(&help));
    let help_out = stdout(&help);
    assert!(help_out.contains("Usage: ytt [OPTIONS]"));
    assert!(help_out.contains("ytt doctor terminal --json"));

    let version = run(&["--version"]);
    assert!(version.status.success(), "stderr: {}", stderr(&version));
    assert!(stdout(&version).starts_with("ytt "));
}

#[test]
fn observational_cli_does_not_create_persistence_roots_or_writer_locks() {
    let root = isolated_root("reader-no-lease");
    let _ = std::fs::remove_dir_all(&root);
    std::fs::create_dir_all(&root).expect("isolated root should be creatable");
    let config = root.join("config");
    let data = root.join("data");
    let cache = root.join("cache");

    let output = isolated_command(&root, &["tools", "status"])
        .env("YTM_DATA_DIR", &data)
        .output()
        .expect("ytt tools status should run read-only");
    assert!(
        matches!(output.status.code(), Some(0) | Some(1)),
        "stdout={}, stderr={}",
        stdout(&output),
        stderr(&output)
    );
    for persistence_root in [&config, &data, &cache] {
        assert!(
            !persistence_root.exists(),
            "reader created persistence root {}",
            persistence_root.display()
        );
        assert!(
            !persistence_root
                .join(".ytt-persistence-writer.lock")
                .exists(),
            "reader created a writer lease at {}",
            persistence_root.display()
        );
    }
}

#[test]
fn read_only_transfer_views_and_missing_tools_args_do_not_create_persistence_roots() {
    for (name, args, expected) in [
        (
            "download-view",
            &[
                "transfer",
                "download",
                "missing-session",
                "--accepted",
                "--dry-run",
            ][..],
            1,
        ),
        (
            "review-view",
            &["transfer", "review", "missing-session", "--all"][..],
            1,
        ),
        (
            "organize-view",
            &[
                "transfer",
                "organize",
                "missing-session",
                "--root",
                "/tmp/library",
                "--dry-run",
            ][..],
            1,
        ),
        ("tools-use-missing", &["tools", "use"][..], 2),
        ("tools-reset-missing", &["tools", "reset"][..], 2),
        ("spotify-help", &["auth", "spotify", "--help"][..], 0),
        (
            "spotify-client-id-missing",
            &["auth", "spotify", "--client-id"][..],
            2,
        ),
    ] {
        let root = isolated_root(name);
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(&root).expect("isolated root should be creatable");
        let config = root.join("config");
        let data = root.join("data");
        let cache = root.join("cache");

        let output = isolated_command(&root, args)
            .env("YTM_DATA_DIR", &data)
            .output()
            .expect("one-shot command should run");
        assert_eq!(
            output.status.code(),
            Some(expected),
            "{args:?}: stdout={}, stderr={}",
            stdout(&output),
            stderr(&output)
        );
        for persistence_root in [&config, &data, &cache] {
            assert!(
                !persistence_root.exists(),
                "{args:?} created persistence root {}",
                persistence_root.display()
            );
        }
    }
}

#[test]
fn organize_dry_run_with_existing_state_preserves_config_and_session_tree() {
    let root = isolated_root("organize-existing-read-only");
    let _ = std::fs::remove_dir_all(&root);
    let config = root.join("config");
    let data = root.join("data");
    let cache = root.join("cache");
    let sessions = data.join("transfers/sessions");
    std::fs::create_dir_all(&config).expect("config directory");
    std::fs::create_dir_all(&sessions).expect("session directory");

    let config_path = config.join("config.json");
    std::fs::write(
        &config_path,
        br#"{"local":{"import_path_template":"{artist}/{title}"}}"#,
    )
    .expect("write existing config");
    let session_id = "sp2yt-organize-read-only";
    let session_path = sessions.join(format!("{session_id}.json"));
    std::fs::write(
        &session_path,
        format!(
            r#"{{"schema_version":1,"session_id":"{session_id}","job_id":"{session_id}","stage":"writing","rows":[]}}"#
        ),
    )
    .expect("write existing import session");
    let library_root = root.join("library");
    let library_arg = library_root.to_string_lossy().into_owned();
    let config_before = snapshot_tree(&config);
    let data_before = snapshot_tree(&data);

    let output = isolated_command(
        &root,
        &[
            "transfer",
            "organize",
            session_id,
            "--root",
            &library_arg,
            "--dry-run",
        ],
    )
    .env("YTM_DATA_DIR", &data)
    .output()
    .expect("organize dry-run should run");

    assert!(
        output.status.success(),
        "stdout={}, stderr={}",
        stdout(&output),
        stderr(&output)
    );
    assert!(
        stdout(&output).contains("Organize preview: sp2yt-organize-read-only"),
        "stdout={}",
        stdout(&output)
    );
    assert_eq!(snapshot_tree(&config), config_before);
    assert_eq!(snapshot_tree(&data), data_before);
    assert!(!cache.exists(), "dry-run created cache state");
    assert!(!library_root.exists(), "dry-run created the target root");
}

#[test]
fn one_shot_subcommands_handle_help_and_parse_errors_without_launching_tui() {
    for args in [
        &["doctor", "--help"][..],
        &["doctor", "privacy", "--help"][..],
        &["tools", "--help"][..],
        &["tools", "status", "--help"][..],
        &["transfer", "--help"][..],
        &["auth", "--help"][..],
        &["daemon", "--help"][..],
        &["update", "--help"][..],
        &["-r", "--help"][..],
    ] {
        let output = run(args);
        assert!(
            output.status.success(),
            "{args:?} failed: {}",
            stderr(&output)
        );
        assert!(
            stdout(&output).contains("Usage:"),
            "{args:?} did not print usage"
        );
    }

    for args in [
        &["tools", "use"][..],
        &["tools", "reset"][..],
        &["transfer"][..],
        &["transfer", "unknown"][..],
        &["auth"][..],
        &["auth", "unknown"][..],
        &["daemon", "unknown"][..],
        &["-r", "not-a-command"][..],
    ] {
        let output = run(args);
        assert_eq!(
            output.status.code(),
            Some(2),
            "{args:?} should be a usage failure; stdout={}, stderr={}",
            stdout(&output),
            stderr(&output)
        );
    }
}

#[test]
fn doctor_privacy_reports_secret_files_without_tui_startup() {
    let root = isolated_root("doctor-privacy");
    let _ = std::fs::remove_dir_all(&root);
    std::fs::create_dir_all(&root).expect("isolated root should be creatable");

    let output = isolated_command(&root, &["doctor", "privacy"])
        .output()
        .expect("ytt doctor privacy should run");
    assert!(output.status.success(), "stderr: {}", stderr(&output));
    let out = stdout(&output);
    assert!(out.contains("Privacy-sensitive files"), "{out}");
    assert!(out.contains("config.json"), "{out}");
    assert!(out.contains("spotify_token.json"), "{out}");
    assert!(out.contains("recovery backups:"), "{out}");

    let cleanup = isolated_command(&root, &["doctor", "privacy", "--cleanup"])
        .output()
        .expect("ytt doctor privacy --cleanup should run");
    assert!(cleanup.status.success(), "stderr: {}", stderr(&cleanup));
    assert!(
        stdout(&cleanup).contains("cleanup: removed"),
        "{}",
        stdout(&cleanup)
    );
}

#[test]
fn doctor_terminal_json_reports_capabilities_without_config_or_runtime_startup() {
    let output = run(&["doctor", "terminal", "--json"]);
    assert!(output.status.success(), "stderr: {}", stderr(&output));

    let json: serde_json::Value =
        serde_json::from_slice(&output.stdout).expect("doctor terminal JSON");
    assert!(
        json.get("image_protocol")
            .and_then(|v| v.as_str())
            .is_some()
    );
    assert!(
        json.get("native_image_hint")
            .and_then(|v| v.as_bool())
            .is_some()
    );
    assert!(
        json.get("image_probe_timeout_ms")
            .and_then(|v| v.as_u64())
            .is_some()
    );
    assert!(
        json.get("image_protocol_override_suggestions")
            .and_then(|v| v.as_array())
            .is_some()
    );
    assert!(json.get("zoom_mode").and_then(|v| v.as_str()).is_some());
    assert_eq!(
        json.get("mouse_capture_configured")
            .and_then(|v| v.as_bool()),
        Some(true)
    );
}

#[test]
fn doctor_verbose_reports_full_environment_without_tui_startup() {
    let root = isolated_root("doctor-verbose");
    let _ = std::fs::remove_dir_all(&root);
    std::fs::create_dir_all(&root).expect("isolated root should be creatable");

    let output = isolated_command(&root, &["doctor", "--verbose"])
        .output()
        .expect("ytt doctor should run");
    assert!(
        matches!(output.status.code(), Some(0) | Some(1)),
        "stdout={}, stderr={}",
        stdout(&output),
        stderr(&output)
    );
    let out = stdout(&output);
    assert!(out.contains("ytt doctor"), "{out}");
    assert!(out.contains("installed via:"), "{out}");
    assert!(out.contains("External tools"), "{out}");
    assert!(out.contains("Managed yt-dlp"), "{out}");
    assert!(out.contains("yt-dlp details"), "{out}");
    assert!(out.contains("managed enabled:"), "{out}");
    assert!(out.contains("PATH candidates:"), "{out}");
    assert!(out.contains("JS runtime:"), "{out}");
    assert!(out.contains("Directories"), "{out}");
    assert!(
        out.contains("OK: all required tools") || out.contains("Problems found:"),
        "{out}"
    );
}

#[test]
fn daemon_status_and_stop_fail_cleanly_without_starting_daemon() {
    let root = isolated_root("daemon-status");
    let _ = std::fs::remove_dir_all(&root);
    std::fs::create_dir_all(&root).expect("isolated root should be creatable");

    let status = isolated_command(&root, &["daemon", "status"])
        .output()
        .expect("ytt daemon status should run");
    assert_eq!(status.status.code(), Some(1));
    assert!(
        stderr(&status).contains("ytt daemon:"),
        "stderr={}",
        stderr(&status)
    );

    let json = isolated_command(&root, &["daemon", "status", "--json"])
        .output()
        .expect("ytt daemon status --json should run");
    assert_eq!(json.status.code(), Some(1));
    assert!(
        stdout(&json).trim().is_empty(),
        "stdout should not contain partial JSON on transport failure: {}",
        stdout(&json)
    );
    assert!(
        stderr(&json).contains("ytt daemon:"),
        "stderr={}",
        stderr(&json)
    );

    let stop = isolated_command(&root, &["daemon", "stop"])
        .output()
        .expect("ytt daemon stop should run");
    assert_eq!(stop.status.code(), Some(1));
    assert!(
        stderr(&stop).contains("ytt daemon:"),
        "stderr={}",
        stderr(&stop)
    );
}

#[test]
fn tools_status_diagnose_unpin_and_reset_use_only_isolated_state() {
    let root = isolated_root("tools-state");
    let _ = std::fs::remove_dir_all(&root);
    std::fs::create_dir_all(&root).expect("isolated root should be creatable");

    let status = isolated_command(&root, &["tools", "status", "--why"])
        .output()
        .expect("ytt tools status should run");
    assert!(
        matches!(status.status.code(), Some(0) | Some(1)),
        "stdout={}, stderr={}",
        stdout(&status),
        stderr(&status)
    );
    let status_out = stdout(&status);
    assert!(status_out.contains("yt-dlp:"), "{status_out}");
    assert!(status_out.contains("managed:"), "{status_out}");
    assert!(status_out.contains("mpv:"), "{status_out}");
    assert!(status_out.contains("Selection reasons"), "{status_out}");
    assert!(status_out.contains("system candidates"), "{status_out}");
    assert!(status_out.contains("JS runtime:"), "{status_out}");

    let diagnose = isolated_command(&root, &["tools", "diagnose"])
        .output()
        .expect("ytt tools diagnose should run");
    assert!(
        diagnose.status.success(),
        "stdout={}, stderr={}",
        stdout(&diagnose),
        stderr(&diagnose)
    );
    let diagnose_out = stdout(&diagnose);
    let report_path = diagnose_out
        .strip_prefix("diagnostic file: ")
        .and_then(|line| line.lines().next())
        .map(PathBuf::from)
        .expect("diagnose should print report path");
    assert!(
        report_path.starts_with(&root),
        "report path escaped isolated root: {}",
        report_path.display()
    );
    let report = std::fs::read_to_string(&report_path).expect("diagnostic report should exist");
    assert!(report.contains("YuTuTui! "), "{report}");
    assert!(report.contains("target_os:"), "{report}");
    assert!(report.contains("YTM_YTDLP: <unset>"), "{report}");
    assert!(report.contains("JS runtimes:"), "{report}");

    let unpin = isolated_command(&root, &["tools", "unpin"])
        .output()
        .expect("ytt tools unpin should run");
    assert!(unpin.status.success(), "stderr={}", stderr(&unpin));
    assert!(
        stdout(&unpin).contains("yt-dlp unpinned"),
        "stdout={}",
        stdout(&unpin)
    );

    let reset = isolated_command(&root, &["tools", "reset", "--playback"])
        .output()
        .expect("ytt tools reset should run");
    assert!(
        matches!(reset.status.code(), Some(0) | Some(1)),
        "stdout={}, stderr={}",
        stdout(&reset),
        stderr(&reset)
    );
    let reset_out = stdout(&reset);
    assert!(reset_out.contains("session cache:"), "{reset_out}");
    assert!(
        reset_out.contains("yt-dlp probe cache: cleared"),
        "{reset_out}"
    );
    assert!(reset_out.contains("yt-dlp update lock:"), "{reset_out}");
}

#[test]
fn transfer_and_auth_one_shots_report_setup_failures_without_tui_startup() {
    let root = isolated_root("transfer-auth");
    let _ = std::fs::remove_dir_all(&root);
    std::fs::create_dir_all(&root).expect("isolated root should be creatable");

    let jobs = isolated_command(&root, &["transfer", "jobs"])
        .output()
        .expect("ytt transfer jobs should run");
    assert!(jobs.status.success(), "stderr={}", stderr(&jobs));
    assert_eq!(stdout(&jobs).trim(), "No transfer jobs.");

    let list_ytm = isolated_command(&root, &["transfer", "list", "ytm"])
        .output()
        .expect("ytt transfer list ytm should run");
    assert_eq!(list_ytm.status.code(), Some(1));
    assert!(
        stderr(&list_ytm).contains("YouTube Music cookie"),
        "stderr={}",
        stderr(&list_ytm)
    );

    let backup_dir = root.join("backup");
    let backup_dir_arg = backup_dir.to_string_lossy().into_owned();
    let backup = isolated_command(&root, &["transfer", "backup", "--dir", &backup_dir_arg])
        .output()
        .expect("ytt transfer backup should run");
    assert_eq!(backup.status.code(), Some(1));
    assert!(
        stderr(&backup).contains("YouTube Music cookie"),
        "stderr={}",
        stderr(&backup)
    );
    assert!(
        !backup_dir.exists(),
        "backup should fail before creating a destination without YTM auth"
    );

    let import = isolated_command(
        &root,
        &[
            "transfer",
            "import",
            "liked",
            "--dry-run",
            "--yes",
            "--min-score",
            "0.55",
            "--take-best",
        ],
    )
    .output()
    .expect("ytt transfer import should run");
    assert_eq!(import.status.code(), Some(1));
    assert!(
        stderr(&import).contains("Spotify"),
        "stderr={}",
        stderr(&import)
    );

    let resume = isolated_command(&root, &["transfer", "resume", "missing-job", "--yes"])
        .output()
        .expect("ytt transfer resume should run");
    assert_eq!(resume.status.code(), Some(1));
    assert!(
        stderr(&resume).contains("missing-job"),
        "stderr={}",
        stderr(&resume)
    );

    let listenbrainz = isolated_command(&root, &["auth", "listenbrainz"])
        .output()
        .expect("ytt auth listenbrainz should run");
    assert_eq!(listenbrainz.status.code(), Some(1));
    assert!(
        stderr(&listenbrainz).contains("missing token"),
        "stderr={}",
        stderr(&listenbrainz)
    );

    let spotify_status = isolated_command(&root, &["auth", "spotify", "--status"])
        .output()
        .expect("ytt auth spotify --status should run");
    assert_eq!(spotify_status.status.code(), Some(1));
    assert!(
        stderr(&spotify_status).contains("Spotify"),
        "stderr={}",
        stderr(&spotify_status)
    );

    let spotify_blank_client = isolated_command(&root, &["auth", "spotify", "--client-id", "   "])
        .output()
        .expect("ytt auth spotify blank client should run");
    assert_eq!(spotify_blank_client.status.code(), Some(1));
    assert!(
        stderr(&spotify_blank_client).contains("no Client ID configured"),
        "stderr={}",
        stderr(&spotify_blank_client)
    );

    let spotify_logout = isolated_command(&root, &["auth", "spotify", "--logout"])
        .output()
        .expect("ytt auth spotify logout should run");
    assert!(
        spotify_logout.status.success(),
        "stderr={}",
        stderr(&spotify_logout)
    );
    assert!(
        stdout(&spotify_logout).contains("Spotify disconnected"),
        "stdout={}",
        stdout(&spotify_logout)
    );
}
