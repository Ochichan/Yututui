use std::fs;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};

use super::{PublishCommand, parse, resolve_music_folder};

fn args(values: &[&str]) -> Vec<String> {
    values.iter().map(|value| (*value).to_owned()).collect()
}

fn temp_root(label: &str) -> PathBuf {
    static NEXT: AtomicU64 = AtomicU64::new(0);
    let sequence = NEXT.fetch_add(1, Ordering::Relaxed);
    let root = std::env::temp_dir().join(format!(
        "ytt-publish-cli-{label}-{}-{sequence}",
        std::process::id()
    ));
    let _ = fs::remove_dir_all(&root);
    fs::create_dir_all(&root).unwrap();
    root
}

#[test]
fn every_accepted_form_parses() {
    assert_eq!(parse(&args(&["setup"])).unwrap(), PublishCommand::Setup);
    assert_eq!(
        parse(&args(&["status"])).unwrap(),
        PublishCommand::Status { json: false }
    );
    assert_eq!(
        parse(&args(&["status", "--json"])).unwrap(),
        PublishCommand::Status { json: true }
    );
    assert_eq!(
        parse(&args(&["list"])).unwrap(),
        PublishCommand::List { json: false }
    );
    assert_eq!(
        parse(&args(&["list", "--json"])).unwrap(),
        PublishCommand::List { json: true }
    );
    assert_eq!(
        parse(&args(&["track", "dQw4w9WgXcQ"])).unwrap(),
        PublishCommand::Track {
            video_id: "dQw4w9WgXcQ".to_owned()
        }
    );
}

#[test]
fn an_empty_invocation_says_what_is_available() {
    assert_eq!(
        parse(&args(&[])).unwrap_err(),
        "publish requires `setup`, `status`, `list`, or `track <VIDEO_ID>`"
    );
}

#[test]
fn unknown_and_malformed_forms_are_refused_with_the_accepted_shapes() {
    let expected =
        "publish accepts `setup`, `status [--json]`, `list [--json]`, or `track <VIDEO_ID>`";
    for invocation in [
        vec!["unpublish"],
        vec!["setup", "--json"],
        vec!["status", "--verbose"],
        vec!["list", "--json", "extra"],
        vec!["track"],
        vec!["track", "one", "two"],
    ] {
        assert_eq!(
            parse(&args(&invocation)).unwrap_err(),
            expected,
            "{invocation:?}"
        );
    }
}

#[test]
fn a_blank_video_id_is_refused() {
    assert_eq!(
        parse(&args(&["track", "   "])).unwrap_err(),
        "track requires a VIDEO_ID"
    );
}

#[test]
fn a_real_directory_resolves_to_its_canonical_path() {
    let root = temp_root("resolve-ok");

    let resolved = resolve_music_folder(&root.to_string_lossy()).unwrap();

    assert_eq!(resolved, fs::canonicalize(&root).unwrap());
    let _ = fs::remove_dir_all(root);
}

#[test]
fn a_regular_file_is_not_a_music_folder() {
    let root = temp_root("resolve-file");
    let file = root.join("not-a-folder");
    fs::write(&file, b"x").unwrap();

    let error = resolve_music_folder(&file.to_string_lossy()).unwrap_err();

    assert!(error.contains("is not a directory"), "{error}");
    let _ = fs::remove_dir_all(root);
}

#[test]
#[cfg(unix)]
fn a_symlinked_music_folder_is_refused_so_the_real_target_is_explicit() {
    let root = temp_root("resolve-symlink");
    let real = root.join("real");
    let link = root.join("link");
    fs::create_dir_all(&real).unwrap();
    std::os::unix::fs::symlink(&real, &link).unwrap();

    let error = resolve_music_folder(&link.to_string_lossy()).unwrap_err();

    assert!(error.contains("symbolic link"), "{error}");
    let _ = fs::remove_dir_all(root);
}

#[test]
fn a_missing_directory_is_reported_rather_than_created() {
    let root = temp_root("resolve-missing");
    let missing = root.join("nowhere");

    resolve_music_folder(&missing.to_string_lossy()).unwrap_err();

    assert!(!missing.exists());
    let _ = fs::remove_dir_all(root);
}

#[test]
fn an_empty_path_is_refused() {
    resolve_music_folder("").unwrap_err();
}

#[test]
fn a_path_carrying_terminal_control_characters_is_refused() {
    let root = temp_root("resolve-control");
    // A directory name with a zero-width joiner would render deceptively in status output.
    let hostile = format!("{}/we\u{200b}ird", root.to_string_lossy());
    fs::create_dir_all(&hostile).unwrap();

    let error = resolve_music_folder(&hostile).unwrap_err();

    assert!(error.contains("unsafe control characters"), "{error}");
    let _ = fs::remove_dir_all(root);
}
