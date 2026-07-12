use std::collections::hash_map::DefaultHasher;
use std::hash::{Hash, Hasher};
use std::io::Read;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use yututui::remote::proto::{InstanceFile, InstanceMode, PROTOCOL_VERSION};

const CHILD_TIMEOUT: Duration = Duration::from_secs(10);
const DESCRIPTOR_TOKEN: &str = "0123456789abcdef0123456789abcdef";

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

    let user_tag = isolated_user_tag(root);
    let mut command = Command::new(env!("CARGO_BIN_EXE_ytt"));
    command
        .args(args)
        .env("HOME", root)
        .env("APPDATA", root.join("config"))
        .env("LOCALAPPDATA", root.join("local"))
        .env("YTM_CONFIG_DIR", root.join("config"))
        .env("YTM_DATA_DIR", root.join("data"))
        .env("YTM_CACHE_DIR", root.join("cache"))
        .env("XDG_CONFIG_HOME", root.join("config"))
        .env("XDG_DATA_HOME", root.join("data"))
        .env("XDG_CACHE_HOME", root.join("cache"))
        .env("XDG_RUNTIME_DIR", runtime)
        .env("YTM_TOOLS_DIR", root.join("tools"))
        .env("USER", &user_tag)
        .env("USERNAME", user_tag)
        .env("LANG", "C")
        .env("LC_ALL", "C")
        .env_remove("YTM_YTDLP")
        .env_remove("YTM_MPV");
    command
}

fn isolated_user_tag(root: &Path) -> String {
    let mut hasher = DefaultHasher::new();
    root.hash(&mut hasher);
    format!("smoke{:x}", hasher.finish())
        .chars()
        .take(16)
        .collect()
}

#[cfg(unix)]
fn remote_app_dir(root: &Path) -> PathBuf {
    use std::os::unix::fs::MetadataExt;

    let runtime = root.join("runtime");
    std::fs::create_dir_all(&runtime).expect("isolated runtime dir should be creatable");
    let uid = std::fs::metadata(&runtime)
        .expect("isolated runtime metadata should be readable")
        .uid();
    runtime.join(format!("yututui-{uid}"))
}

#[cfg(not(unix))]
fn remote_app_dir(root: &Path) -> PathBuf {
    std::env::temp_dir().join(format!("yututui-{}", isolated_user_tag(root)))
}

#[cfg(unix)]
fn canonical_remote_endpoint(root: &Path) -> String {
    remote_app_dir(root)
        .join(format!("yututui-remote-{}.sock", isolated_user_tag(root)))
        .to_string_lossy()
        .into_owned()
}

#[cfg(not(unix))]
fn canonical_remote_endpoint(root: &Path) -> String {
    format!(r"\\.\pipe\yututui-remote-{}", isolated_user_tag(root))
}

fn current_descriptor_path(root: &Path) -> PathBuf {
    remote_app_dir(root).join(format!("yututui-remote-{}.json", isolated_user_tag(root)))
}

fn write_current_descriptor(root: &Path, instance: &InstanceFile) {
    assert_eq!(instance.endpoint, canonical_remote_endpoint(root));
    let runtime = root.join("runtime");
    std::fs::create_dir_all(&runtime).expect("isolated runtime dir should be creatable");
    let app_dir = remote_app_dir(root);
    std::fs::create_dir_all(&app_dir).expect("private remote app dir should be creatable");
    let path = current_descriptor_path(root);
    std::fs::write(&path, serde_json::to_vec(instance).unwrap())
        .expect("current descriptor should be writable");
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&runtime, std::fs::Permissions::from_mode(0o700)).unwrap();
        std::fs::set_permissions(&app_dir, std::fs::Permissions::from_mode(0o700)).unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600)).unwrap();
    }
}

fn isolated_instance(root: &Path, protocol_version: u8, capabilities: Vec<String>) -> InstanceFile {
    InstanceFile {
        app_pid: 4_242,
        endpoint: canonical_remote_endpoint(root),
        token: DESCRIPTOR_TOKEN.to_owned(),
        created_unix: 123,
        mode: InstanceMode::Daemon,
        protocol_version,
        capabilities,
    }
}

fn clean_isolated_remote(root: &Path) {
    let _ = std::fs::remove_dir_all(remote_app_dir(root));
    let _ = std::fs::remove_dir_all(root);
}

fn run_isolated_with_timeout(root: &Path, args: &[&str]) -> std::process::Output {
    output_with_timeout(isolated_command(root, args), CHILD_TIMEOUT)
}

fn output_with_timeout(mut command: Command, timeout: Duration) -> std::process::Output {
    command.stdout(Stdio::piped()).stderr(Stdio::piped());
    let mut child = command.spawn().expect("ytt command should spawn");
    let deadline = Instant::now() + timeout;
    let status = loop {
        match child
            .try_wait()
            .expect("ytt child status should be readable")
        {
            Some(status) => break status,
            None if Instant::now() < deadline => std::thread::sleep(Duration::from_millis(10)),
            None => {
                let _ = child.kill();
                let _ = child.wait();
                panic!("ytt command exceeded {timeout:?}");
            }
        }
    };
    let mut stdout = Vec::new();
    let mut stderr = Vec::new();
    child
        .stdout
        .take()
        .expect("ytt stdout should be captured")
        .read_to_end(&mut stdout)
        .unwrap();
    child
        .stderr
        .take()
        .expect("ytt stderr should be captured")
        .read_to_end(&mut stderr)
        .unwrap();
    std::process::Output {
        status,
        stdout,
        stderr,
    }
}

fn stdout(output: &std::process::Output) -> String {
    String::from_utf8_lossy(&output.stdout).into_owned()
}

fn stderr(output: &std::process::Output) -> String {
    String::from_utf8_lossy(&output.stderr).into_owned()
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
fn one_shot_subcommands_handle_help_and_parse_errors_without_launching_tui() {
    for args in [
        &["doctor", "--help"][..],
        &["doctor", "privacy", "--help"][..],
        &["tools", "--help"][..],
        &["tools", "status", "--help"][..],
        &["transfer", "--help"][..],
        &["data", "--help"][..],
        &["data", "export", "--help"][..],
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
        &["data"][..],
        &["data", "unknown"][..],
        &["data", "export", "--to"][..],
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
fn personal_data_export_writes_a_private_sanitized_json_file_offline() {
    let root = isolated_root("personal-export");
    let _ = std::fs::remove_dir_all(&root);
    let config_dir = root.join("config");
    let data_dir = root.join("data");
    let export_dir = root.join("exports");
    for directory in [&config_dir, &data_dir, &export_dir] {
        std::fs::create_dir_all(directory).expect("isolated export directory");
    }

    const SECRET: &str = "cli-export-secret-sentinel";
    const PRIVATE_PATH: &str = "/Users/alice/private/music.flac";
    std::fs::write(
        config_dir.join("config.json"),
        format!(
            r#"{{"cookie":"{SECRET}","gemini_api_key":"{SECRET}","download_dir":"{PRIVATE_PATH}","volume":42}}"#
        ),
    )
    .expect("write isolated config");
    std::fs::write(
        data_dir.join("library.json"),
        r#"{"favorites":[{"video_id":"dQw4w9WgXcQ","title":"Portable song","artist":"Safe artist","duration":"3:32"}]}"#,
    )
    .expect("write isolated library");

    let output = isolated_command(
        &root,
        &["data", "export", "--to", export_dir.to_str().unwrap()],
    )
    .output()
    .expect("ytt data export should run");
    assert!(output.status.success(), "stderr: {}", stderr(&output));
    let out = stdout(&output);
    assert!(out.contains("Exported personal data to"), "{out}");
    assert!(
        out.contains("Private listening history is included"),
        "{out}"
    );

    let files: Vec<PathBuf> = std::fs::read_dir(&export_dir)
        .expect("list exports")
        .map(|entry| entry.expect("export entry").path())
        .collect();
    assert_eq!(files.len(), 1, "expected one final export: {files:?}");
    let bytes = std::fs::read(&files[0]).expect("read export");
    let text = String::from_utf8(bytes.clone()).expect("export is UTF-8");
    let json: serde_json::Value = serde_json::from_slice(&bytes).expect("parse export");
    assert_eq!(json["kind"], "yututui_personal_data_export");
    assert_eq!(json["schema_version"], 1);
    assert_eq!(json["settings"]["general"]["volume"], 42);
    assert_eq!(json["library"]["favorites"][0]["title"], "Portable song");
    for forbidden in [SECRET, PRIVATE_PATH, "gemini_api_key", "cookies_file"] {
        assert!(!text.contains(forbidden), "export leaked {forbidden:?}");
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        assert_eq!(
            std::fs::metadata(&files[0])
                .expect("export metadata")
                .permissions()
                .mode()
                & 0o777,
            0o600
        );
    }
    std::fs::remove_dir_all(root).expect("cleanup isolated export");
}

#[cfg(unix)]
#[test]
fn personal_data_export_recovers_from_a_stale_descriptor_without_deleting_it() {
    use std::os::unix::fs::PermissionsExt;

    const USER_TAG: &str = "staleexporttest";

    let root = isolated_root("stale-personal-export");
    let _ = std::fs::remove_dir_all(&root);
    let export_dir = root.join("exports");
    std::fs::create_dir_all(&export_dir).expect("isolated export directory");

    // A one-shot remote probe creates the same private runtime directory that the later export
    // will use, without starting the TUI or publishing an instance descriptor.
    let probe = isolated_command(&root, &["-r", "status"])
        .env("USER", USER_TAG)
        .output()
        .expect("isolated remote probe should run");
    assert_eq!(probe.status.code(), Some(1), "stderr: {}", stderr(&probe));

    let runtime = root.join("runtime");
    let app_dirs: Vec<PathBuf> = std::fs::read_dir(&runtime)
        .expect("list isolated runtime")
        .filter_map(|entry| {
            let path = entry.expect("runtime entry").path();
            path.file_name()
                .and_then(|name| name.to_str())
                .is_some_and(|name| name.starts_with("yututui-"))
                .then_some(path)
        })
        .collect();
    assert_eq!(app_dirs.len(), 1, "private runtime dir: {app_dirs:?}");
    let app_dir = &app_dirs[0];
    let endpoint = app_dir.join(format!("yututui-remote-{USER_TAG}.sock"));
    let descriptor = app_dir.join(format!("yututui-remote-{USER_TAG}.json"));
    let contents = serde_json::json!({
        "app_pid": u32::MAX,
        "endpoint": endpoint,
        "token": "00000000000000000000000000000000",
        "created_unix": 1,
        "mode": "standalone_tui",
        "protocol_version": 8,
        "capabilities": ["remote-control", "status", "personal-export-v1"]
    });
    std::fs::write(
        &descriptor,
        serde_json::to_vec(&contents).expect("serialize stale descriptor"),
    )
    .expect("write stale descriptor");
    std::fs::set_permissions(&descriptor, std::fs::Permissions::from_mode(0o600))
        .expect("make stale descriptor private");

    let output = isolated_command(
        &root,
        &["data", "export", "--to", export_dir.to_str().unwrap()],
    )
    .env("USER", USER_TAG)
    .output()
    .expect("ytt data export should recover from the stale descriptor");

    assert!(output.status.success(), "stderr: {}", stderr(&output));
    let out = stdout(&output);
    assert!(out.contains("Exported personal data to"), "{out}");
    assert!(out.contains("--new-instance"), "{out}");
    assert!(descriptor.exists(), "stale descriptor must not be deleted");
    assert_eq!(
        std::fs::read_dir(&export_dir)
            .expect("list recovered export")
            .count(),
        1
    );

    std::fs::remove_dir_all(root).expect("cleanup isolated stale export");
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

#[test]
fn new_remote_commands_fail_cleanly_without_an_owner() {
    let root = isolated_root("remote-no-instance");
    clean_isolated_remote(&root);

    for args in [
        &["-r", "info"][..],
        &["-r", "queue-list"][..],
        &["-r", "settings-show"][..],
        &["-r", "queue-play", "1"][..],
        &["-r", "watch"][..],
    ] {
        let output = run_isolated_with_timeout(&root, args);
        assert_eq!(
            output.status.code(),
            Some(1),
            "{args:?}: stdout={}, stderr={}",
            stdout(&output),
            stderr(&output)
        );
        assert!(stdout(&output).is_empty(), "{args:?}: unexpected stdout");
        assert!(
            stderr(&output).contains("ytt -r:"),
            "{args:?}: stderr={}",
            stderr(&output)
        );
    }

    clean_isolated_remote(&root);
}

#[test]
fn new_remote_parser_rejects_bad_arity_before_connecting() {
    let root = isolated_root("remote-bad-arity");
    clean_isolated_remote(&root);

    for args in [
        &["-r", "info", "extra"][..],
        &["-r", "queue-list", "extra"][..],
        &["-r", "settings-show", "extra"][..],
        &["-r", "queue-play"][..],
        &["-r", "queue-play", "0"][..],
        &["-r", "queue-play", "-1"][..],
        &["-r", "queue-play", "1", "extra"][..],
        &["-r", "queue-play", "184467440737095516160"][..],
        &["-r", "watch", "player", "queue"][..],
    ] {
        let output = run_isolated_with_timeout(&root, args);
        assert_eq!(
            output.status.code(),
            Some(2),
            "{args:?}: stdout={}, stderr={}",
            stdout(&output),
            stderr(&output)
        );
        assert!(stdout(&output).is_empty(), "{args:?}: unexpected stdout");
        assert!(
            stderr(&output).contains("ytt -r:"),
            "{args:?}: stderr={}",
            stderr(&output)
        );
    }

    clean_isolated_remote(&root);
}

#[test]
fn watch_rejects_unsupported_descriptor_protocol_and_capability() {
    let root = isolated_root("remote-watch-unsupported");
    clean_isolated_remote(&root);

    let protocol_instance =
        isolated_instance(&root, PROTOCOL_VERSION - 1, vec!["events-v8".to_owned()]);
    write_current_descriptor(&root, &protocol_instance);
    let protocol = run_isolated_with_timeout(&root, &["-r", "watch"]);
    assert_eq!(protocol.status.code(), Some(2), "{}", stderr(&protocol));
    assert!(stderr(&protocol).contains("requires protocol 8"));
    assert!(!stderr(&protocol).contains(DESCRIPTOR_TOKEN));
    assert!(!stderr(&protocol).contains(&protocol_instance.endpoint));

    let capability_instance = isolated_instance(
        &root,
        PROTOCOL_VERSION,
        vec!["remote-control".to_owned(), "status".to_owned()],
    );
    write_current_descriptor(&root, &capability_instance);
    let capability = run_isolated_with_timeout(&root, &["-r", "watch"]);
    assert_eq!(capability.status.code(), Some(2), "{}", stderr(&capability));
    assert!(stderr(&capability).contains("does not support watch events"));
    assert!(!stderr(&capability).contains(DESCRIPTOR_TOKEN));
    assert!(!stderr(&capability).contains(&capability_instance.endpoint));

    clean_isolated_remote(&root);
}

#[cfg(any(unix, windows))]
mod remote_owner {
    use super::*;
    use std::sync::mpsc;

    use interprocess::local_socket::tokio::prelude::*;
    use interprocess::local_socket::{GenericFilePath, ListenerOptions};
    use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
    use yututui::remote::proto::{
        ClientFrame, ClientOp, HelloAck, HelloRequest, PushEvent, QueueItemSnapshot, RemoteCommand,
        RemoteRequest, RemoteResponse, ServerFrame, SettingsSnapshot, StatusSnapshot, Topic,
    };
    use yututui::search_source::SearchSource;
    use yututui::streaming::StreamingMode;

    const OWNER_IO_TIMEOUT: Duration = Duration::from_secs(10);

    struct Exchange {
        command: RemoteCommand,
        response: RemoteResponse,
    }

    #[cfg(unix)]
    fn short_root(label: &str) -> PathBuf {
        PathBuf::from("/tmp").join(format!("ytt-r-{label}-{}", std::process::id()))
    }

    #[cfg(windows)]
    fn short_root(label: &str) -> PathBuf {
        isolated_root(label)
    }

    fn spawn_fake_owner(endpoint: String, exchanges: Vec<Exchange>) -> std::thread::JoinHandle<()> {
        let (ready_tx, ready_rx) = mpsc::sync_channel::<Result<(), String>>(1);
        let handle = std::thread::spawn(move || {
            let runtime = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .expect("fake owner runtime should build");
            runtime.block_on(async move {
                let _ = std::fs::remove_file(&endpoint);
                let name = endpoint
                    .as_str()
                    .to_fs_name::<GenericFilePath>()
                    .expect("canonical endpoint should be a filesystem socket name");
                let listener = match ListenerOptions::new()
                    .name(name)
                    .reclaim_name(false)
                    .create_tokio()
                {
                    Ok(listener) => listener,
                    Err(error) => {
                        let _ = ready_tx.send(Err(error.to_string()));
                        return;
                    }
                };
                ready_tx.send(Ok(())).unwrap();

                for exchange in exchanges {
                    let conn = tokio::time::timeout(OWNER_IO_TIMEOUT, listener.accept())
                        .await
                        .expect("timed out waiting for ytt remote connection")
                        .expect("fake owner should accept ytt connection");
                    let mut request_line = String::new();
                    {
                        let mut reader = BufReader::new(&conn);
                        tokio::time::timeout(OWNER_IO_TIMEOUT, reader.read_line(&mut request_line))
                            .await
                            .expect("timed out reading ytt request")
                            .expect("fake owner should read ytt request");
                    }
                    let request: RemoteRequest = serde_json::from_str(request_line.trim())
                        .expect("ytt should send a valid one-shot request");
                    assert_eq!(request.version, PROTOCOL_VERSION);
                    assert_eq!(request.token, DESCRIPTOR_TOKEN);
                    assert_eq!(request.command, exchange.command);

                    let mut response = serde_json::to_vec(&exchange.response).unwrap();
                    response.push(b'\n');
                    let mut writer = &conn;
                    tokio::time::timeout(OWNER_IO_TIMEOUT, writer.write_all(&response))
                        .await
                        .expect("timed out writing fake-owner response")
                        .expect("fake owner should write response");
                    tokio::time::timeout(OWNER_IO_TIMEOUT, writer.flush())
                        .await
                        .expect("timed out flushing fake-owner response")
                        .expect("fake owner should flush response");
                }
            });
        });

        match ready_rx.recv_timeout(CHILD_TIMEOUT) {
            Ok(Ok(())) => {}
            Ok(Err(error)) => panic!("fake owner could not bind: {error}"),
            Err(error) => panic!("fake owner did not become ready: {error}"),
        }
        handle
    }

    fn spawn_watch_owner(endpoint: String) -> std::thread::JoinHandle<()> {
        let (ready_tx, ready_rx) = mpsc::sync_channel::<Result<(), String>>(1);
        let handle = std::thread::spawn(move || {
            let runtime = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .expect("fake watch owner runtime should build");
            runtime.block_on(async move {
                let _ = std::fs::remove_file(&endpoint);
                let name = endpoint
                    .as_str()
                    .to_fs_name::<GenericFilePath>()
                    .expect("canonical endpoint should be a local socket name");
                let listener = match ListenerOptions::new()
                    .name(name)
                    .reclaim_name(false)
                    .create_tokio()
                {
                    Ok(listener) => listener,
                    Err(error) => {
                        let _ = ready_tx.send(Err(error.to_string()));
                        return;
                    }
                };
                ready_tx.send(Ok(())).unwrap();

                let conn = tokio::time::timeout(OWNER_IO_TIMEOUT, listener.accept())
                    .await
                    .expect("timed out waiting for ytt watch connection")
                    .expect("fake watch owner should accept ytt connection");
                let mut reader = BufReader::new(&conn);
                let mut writer = &conn;

                let mut line = String::new();
                tokio::time::timeout(OWNER_IO_TIMEOUT, reader.read_line(&mut line))
                    .await
                    .expect("timed out reading watch Hello")
                    .expect("fake watch owner should read Hello");
                let hello: HelloRequest =
                    serde_json::from_str(line.trim()).expect("ytt watch should send a valid Hello");
                assert_eq!(hello.version, PROTOCOL_VERSION);
                assert_eq!(hello.token, DESCRIPTOR_TOKEN);
                assert_eq!(hello.hello.client, "ytt-cli-watch");
                assert_eq!(hello.hello.min_version, PROTOCOL_VERSION);

                write_owner_line(
                    &mut writer,
                    &HelloAck {
                        ok: true,
                        version: PROTOCOL_VERSION,
                        session_id: 99,
                        capabilities: vec!["events-v8".to_owned()],
                        owner_mode: InstanceMode::Daemon,
                        reason: None,
                    },
                )
                .await;

                line.clear();
                tokio::time::timeout(OWNER_IO_TIMEOUT, reader.read_line(&mut line))
                    .await
                    .expect("timed out reading watch subscription")
                    .expect("fake watch owner should read subscription");
                let subscribe: ClientFrame = serde_json::from_str(line.trim())
                    .expect("ytt watch should send a valid subscription");
                assert_eq!(subscribe.id, 1);
                assert_eq!(
                    subscribe.op,
                    ClientOp::Subscribe {
                        topics: vec![Topic::System]
                    }
                );

                write_owner_line(
                    &mut writer,
                    &ServerFrame::Event {
                        seq: 1,
                        topic: Topic::System,
                        event: PushEvent::OwnerChanged {
                            mode: InstanceMode::Daemon,
                        },
                    },
                )
                .await;
                write_owner_line(
                    &mut writer,
                    &ServerFrame::Reply {
                        id: 1,
                        resp: RemoteResponse::ok("subscribed".to_owned()),
                    },
                )
                .await;
                write_owner_line(
                    &mut writer,
                    &ServerFrame::Goodbye {
                        reason: "shutting_down".to_owned(),
                    },
                )
                .await;
            });
        });

        match ready_rx.recv_timeout(CHILD_TIMEOUT) {
            Ok(Ok(())) => {}
            Ok(Err(error)) => panic!("fake watch owner could not bind: {error}"),
            Err(error) => panic!("fake watch owner did not become ready: {error}"),
        }
        handle
    }

    async fn write_owner_line<W, T>(writer: &mut W, value: &T)
    where
        W: tokio::io::AsyncWrite + Unpin,
        T: serde::Serialize,
    {
        let mut bytes = serde_json::to_vec(value).expect("fake owner frame should serialize");
        bytes.push(b'\n');
        tokio::time::timeout(OWNER_IO_TIMEOUT, writer.write_all(&bytes))
            .await
            .expect("timed out writing fake-owner frame")
            .expect("fake owner should write frame");
        tokio::time::timeout(OWNER_IO_TIMEOUT, writer.flush())
            .await
            .expect("timed out flushing fake-owner frame")
            .expect("fake owner should flush frame");
    }

    fn snapshot(queue: Vec<QueueItemSnapshot>, settings: SettingsSnapshot) -> StatusSnapshot {
        let position = queue
            .iter()
            .position(|item| item.current)
            .map_or(0, |index| index + 1);
        StatusSnapshot {
            title: None,
            artist: None,
            paused: true,
            volume: 50,
            position,
            total: queue.len(),
            streaming: settings.autoplay_streaming,
            owner_mode: InstanceMode::Daemon,
            settings,
            queue,
            shuffle: false,
            repeat: Default::default(),
            elapsed_ms: None,
            duration_ms: None,
            artwork: None,
        }
    }

    fn assert_credentials_hidden(output: &std::process::Output, endpoint: &str) {
        for text in [stdout(output), stderr(output)] {
            assert!(
                !text.contains(DESCRIPTOR_TOKEN),
                "credential leaked: {text}"
            );
            assert!(!text.contains(endpoint), "endpoint leaked: {text}");
        }
    }

    #[test]
    fn actual_ytt_remote_cli_projects_owner_status_and_converts_queue_index() {
        let root = short_root("ok");
        clean_isolated_remote(&root);
        let instance = isolated_instance(
            &root,
            PROTOCOL_VERSION,
            vec![
                "status".to_owned(),
                "events-v8".to_owned(),
                "remote-control".to_owned(),
            ],
        );
        #[cfg(unix)]
        assert!(
            instance.endpoint.len() < 100,
            "Unix socket path is too long for macOS sun_path: {}",
            instance.endpoint
        );
        write_current_descriptor(&root, &instance);
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            assert_eq!(
                std::fs::metadata(remote_app_dir(&root))
                    .unwrap()
                    .permissions()
                    .mode()
                    & 0o777,
                0o700
            );
            assert_eq!(
                std::fs::metadata(current_descriptor_path(&root))
                    .unwrap()
                    .permissions()
                    .mode()
                    & 0o777,
                0o600
            );
        }

        let info_response = RemoteResponse::status(snapshot(Vec::new(), Default::default()));
        let empty_response = RemoteResponse::status(snapshot(Vec::new(), Default::default()));
        let queue_response = RemoteResponse::status(snapshot(
            vec![
                QueueItemSnapshot {
                    title: "첫 곡\n\u{1b}[31m".to_owned(),
                    artist: "가수\r이름".to_owned(),
                    duration: "3:14\t".to_owned(),
                    current: true,
                },
                QueueItemSnapshot {
                    title: "第二曲\u{202e}".to_owned(),
                    artist: "Artist".to_owned(),
                    duration: "4:00".to_owned(),
                    current: false,
                },
            ],
            Default::default(),
        ));
        let settings_response = RemoteResponse::status(snapshot(
            Vec::new(),
            SettingsSnapshot {
                autoplay_streaming: true,
                streaming_mode: StreamingMode::Discovery,
                streaming_source: SearchSource::InternetArchive,
                speed_tenths: 15,
                seek_seconds: 12,
                normalize: true,
                gapless: false,
                ai_enabled: true,
                radio_mode: false,
            },
        ));
        let queue_json_expected = serde_json::to_value(&queue_response).unwrap();
        let settings_json_expected = serde_json::to_value(&settings_response).unwrap();
        let owner = spawn_fake_owner(
            instance.endpoint.clone(),
            vec![
                Exchange {
                    command: RemoteCommand::Status,
                    response: info_response.clone(),
                },
                Exchange {
                    command: RemoteCommand::Status,
                    response: info_response,
                },
                Exchange {
                    command: RemoteCommand::Status,
                    response: empty_response,
                },
                Exchange {
                    command: RemoteCommand::Status,
                    response: queue_response.clone(),
                },
                Exchange {
                    command: RemoteCommand::Status,
                    response: queue_response,
                },
                Exchange {
                    command: RemoteCommand::Status,
                    response: settings_response.clone(),
                },
                Exchange {
                    command: RemoteCommand::Status,
                    response: settings_response,
                },
                Exchange {
                    command: RemoteCommand::QueuePlay { position: 0 },
                    response: RemoteResponse::ok("played queue item 1".to_owned()),
                },
            ],
        );

        let info = run_isolated_with_timeout(&root, &["-r", "info"]);
        assert!(info.status.success(), "{}", stderr(&info));
        let info_text = stdout(&info);
        assert!(info_text.contains("pid 4242"), "{info_text}");
        assert!(info_text.contains("mode daemon"), "{info_text}");
        assert!(info_text.contains("protocol 8"), "{info_text}");
        assert!(
            info_text.contains("events-v8,remote-control,status"),
            "{info_text}"
        );
        assert_credentials_hidden(&info, &instance.endpoint);

        let info_json = run_isolated_with_timeout(&root, &["-r", "info", "--json"]);
        assert!(info_json.status.success(), "{}", stderr(&info_json));
        let info_value: serde_json::Value = serde_json::from_slice(&info_json.stdout).unwrap();
        let info_object = info_value.as_object().unwrap();
        assert_eq!(info_object.len(), 5);
        assert_eq!(info_object["app_pid"], 4_242);
        assert_eq!(info_object["created_unix"], 123);
        assert_eq!(info_object["mode"], "daemon");
        assert_eq!(info_object["protocol_version"], PROTOCOL_VERSION);
        assert_eq!(
            info_object["capabilities"],
            serde_json::json!(["events-v8", "remote-control", "status"])
        );
        assert!(!info_object.contains_key("endpoint"));
        assert!(!info_object.contains_key("token"));
        assert_credentials_hidden(&info_json, &instance.endpoint);

        let empty = run_isolated_with_timeout(&root, &["-r", "queue-list"]);
        assert!(empty.status.success(), "{}", stderr(&empty));
        assert_eq!(stdout(&empty), "queue empty\n");
        assert_credentials_hidden(&empty, &instance.endpoint);

        let queue = run_isolated_with_timeout(&root, &["-r", "queue-list"]);
        assert!(queue.status.success(), "{}", stderr(&queue));
        let queue_text = stdout(&queue);
        assert!(queue_text.starts_with("> 1. 첫 곡 [31m — 가수 이름  [3:14]"));
        assert!(queue_text.contains("  2. 第二曲 — Artist  [4:00]"));
        assert_eq!(queue_text.lines().count(), 2);
        assert!(!queue_text.contains('\u{1b}'));
        assert!(!queue_text.contains('\r'));
        assert!(!queue_text.contains('\u{202e}'));
        assert_credentials_hidden(&queue, &instance.endpoint);

        let queue_json = run_isolated_with_timeout(&root, &["-r", "queue-list", "--json"]);
        assert!(queue_json.status.success(), "{}", stderr(&queue_json));
        let queue_value: serde_json::Value = serde_json::from_slice(&queue_json.stdout).unwrap();
        assert_eq!(queue_value, queue_json_expected);
        assert!(queue_value.get("ok").is_some());
        assert!(queue_value.get("reason").is_some());
        assert!(queue_value.get("message").is_some());
        assert!(queue_value.get("status").is_some());
        assert_credentials_hidden(&queue_json, &instance.endpoint);

        let settings = run_isolated_with_timeout(&root, &["-r", "settings-show"]);
        assert!(settings.status.success(), "{}", stderr(&settings));
        assert_eq!(
            stdout(&settings).trim(),
            "autoplay=on  •  source=internet_archive  •  mode=discovery  •  speed=1.5x  •  seek=12s  •  normalize=on  •  gapless=off  •  ai=on  •  radio-mode=off"
        );
        assert_credentials_hidden(&settings, &instance.endpoint);

        let settings_json = run_isolated_with_timeout(&root, &["-r", "settings-show", "--json"]);
        assert!(settings_json.status.success(), "{}", stderr(&settings_json));
        let settings_value: serde_json::Value =
            serde_json::from_slice(&settings_json.stdout).unwrap();
        assert_eq!(settings_value, settings_json_expected);
        assert!(settings_value.get("ok").is_some());
        assert!(settings_value.get("reason").is_some());
        assert!(settings_value.get("message").is_some());
        assert!(settings_value.get("status").is_some());
        assert_credentials_hidden(&settings_json, &instance.endpoint);

        let queue_play = run_isolated_with_timeout(&root, &["-r", "queue-play", "1"]);
        assert!(queue_play.status.success(), "{}", stderr(&queue_play));
        assert_eq!(stdout(&queue_play), "played queue item 1\n");
        assert_credentials_hidden(&queue_play, &instance.endpoint);

        owner.join().expect("fake owner thread should complete");
        clean_isolated_remote(&root);
    }

    #[test]
    fn actual_ytt_watch_json_accepts_initial_event_before_reply_and_exits_on_goodbye() {
        let root = short_root("watch");
        clean_isolated_remote(&root);
        let instance = isolated_instance(
            &root,
            PROTOCOL_VERSION,
            vec!["events-v8".to_owned(), "status".to_owned()],
        );
        #[cfg(unix)]
        assert!(
            instance.endpoint.len() < 100,
            "Unix socket path is too long for macOS sun_path: {}",
            instance.endpoint
        );
        write_current_descriptor(&root, &instance);
        let owner = spawn_watch_owner(instance.endpoint.clone());

        let output = run_isolated_with_timeout(&root, &["-r", "watch", "system", "--json"]);
        assert!(output.status.success(), "stderr={}", stderr(&output));
        assert!(stderr(&output).is_empty(), "stderr={}", stderr(&output));
        assert_credentials_hidden(&output, &instance.endpoint);

        let output_text = stdout(&output);
        let lines: Vec<_> = output_text.lines().collect();
        assert_eq!(lines.len(), 2, "stdout={output_text}");
        assert!(
            lines
                .iter()
                .all(|line| !line.contains(r#"\"frame\":\"reply\""#)),
            "subscribe Reply must stay hidden: {output_text}"
        );
        let frames: Vec<ServerFrame> = lines
            .iter()
            .map(|line| serde_json::from_str(line).expect("watch stdout should be NDJSON frames"))
            .collect();
        assert_eq!(
            frames[0],
            ServerFrame::Event {
                seq: 1,
                topic: Topic::System,
                event: PushEvent::OwnerChanged {
                    mode: InstanceMode::Daemon
                }
            }
        );
        assert_eq!(
            frames[1],
            ServerFrame::Goodbye {
                reason: "shutting_down".to_owned()
            }
        );

        owner
            .join()
            .expect("fake watch owner thread should complete");
        clean_isolated_remote(&root);
    }
}
