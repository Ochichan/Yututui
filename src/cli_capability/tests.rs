use super::*;

fn args(values: &[&str]) -> Vec<String> {
    values.iter().map(|value| (*value).to_owned()).collect()
}

#[test]
fn interactive_instances_have_explicit_capabilities() {
    assert_eq!(
        interactive_persistence_capability(true),
        CliPersistenceCapability::ReadOnly
    );
    assert_eq!(
        interactive_persistence_capability(false),
        CliPersistenceCapability::Writer
    );
}

#[test]
fn capability_policy_truth_table_is_explicit_and_write_implies_persistence() {
    let cases = [
        (CliPersistenceCapability::NotRequired, false, false),
        (CliPersistenceCapability::ReadOnly, true, false),
        (CliPersistenceCapability::Writer, true, true),
    ];
    for (capability, requires_persistence, allows_writes) in cases {
        assert_eq!(
            capability.requires_persistence(),
            requires_persistence,
            "{capability:?} persistence policy"
        );
        assert_eq!(
            capability.allows_writes(),
            allows_writes,
            "{capability:?} write policy"
        );
        assert!(!capability.allows_writes() || capability.requires_persistence());
    }
}

#[test]
fn transfer_download_and_review_views_are_observational() {
    let classify = |values: &[&str]| OneShotCommand::Transfer.persistence_capability(&args(values));
    assert_eq!(
        classify(&["download", "sp2yt-1", "--accepted", "--dry-run"]),
        CliPersistenceCapability::ReadOnly
    );
    assert_eq!(
        classify(&["review", "sp2yt-1"]),
        CliPersistenceCapability::ReadOnly
    );
    assert_eq!(
        classify(&["review", "sp2yt-1", "--accepted"]),
        CliPersistenceCapability::ReadOnly
    );
    assert_eq!(
        classify(&["review", "sp2yt-1", "accept", "1"]),
        CliPersistenceCapability::Writer
    );
    assert_eq!(
        classify(&["review", "sp2yt-1", "unknown", "1"]),
        CliPersistenceCapability::Writer
    );
    assert_eq!(classify(&["review"]), CliPersistenceCapability::NotRequired);
}

#[test]
fn transfer_organize_only_downgrades_a_valid_dry_run_shape() {
    let classify = |values: &[&str]| OneShotCommand::Transfer.persistence_capability(&args(values));
    assert_eq!(
        classify(&["organize", "sp2yt-1", "--root", "/tmp/library", "--dry-run",]),
        CliPersistenceCapability::ReadOnly
    );
    for values in [
        &[
            "organize",
            "sp2yt-1",
            "--root",
            "/tmp/library",
            "--apply",
            "--yes",
        ][..],
        &[
            "organize",
            "sp2yt-1",
            "--root",
            "/tmp/library",
            "--dry-run",
            "--apply",
            "--yes",
        ][..],
        &["organize", "sp2yt-1", "--dry-run"][..],
        &[
            "organize",
            "sp2yt-1",
            "--root",
            "/tmp/library",
            "--dry-run",
            "--unknown",
        ][..],
    ] {
        assert_eq!(classify(values), CliPersistenceCapability::Writer);
    }
}

#[test]
fn obvious_parse_errors_do_not_initialize_persistence() {
    let tools = |values: &[&str]| OneShotCommand::Tools.persistence_capability(&args(values));
    assert_eq!(tools(&["use"]), CliPersistenceCapability::NotRequired);
    assert_eq!(tools(&["reset"]), CliPersistenceCapability::NotRequired);
    assert_eq!(tools(&["use", "system"]), CliPersistenceCapability::Writer);
    assert_eq!(
        tools(&["use", "system", "extra"]),
        CliPersistenceCapability::Writer
    );
    assert_eq!(
        tools(&["reset", "--not-playback"]),
        CliPersistenceCapability::Writer
    );

    let auth = |values: &[&str]| OneShotCommand::Auth.persistence_capability(&args(values));
    assert_eq!(
        auth(&["spotify", "--help"]),
        CliPersistenceCapability::NotRequired
    );
    assert_eq!(
        auth(&["spotify", "--client-id"]),
        CliPersistenceCapability::NotRequired
    );
    assert_eq!(
        auth(&["spotify", "--unknown"]),
        CliPersistenceCapability::NotRequired
    );
    assert_eq!(
        auth(&["spotify", "--client-id", "--help"]),
        CliPersistenceCapability::Writer
    );
    assert_eq!(
        auth(&["spotify", "--status"]),
        CliPersistenceCapability::Writer
    );
    assert_eq!(
        auth(&["spotify", "--logout"]),
        CliPersistenceCapability::Writer
    );
    assert_eq!(auth(&["spotify"]), CliPersistenceCapability::Writer);
}

#[test]
fn compatibility_corpus_preserves_parser_precedence() {
    let cases: &[(OneShotCommand, &[&str], CliPersistenceCapability)] = &[
        (
            OneShotCommand::Auth,
            &[],
            CliPersistenceCapability::NotRequired,
        ),
        (
            OneShotCommand::Auth,
            &["lastfm", "--help"],
            CliPersistenceCapability::Writer,
        ),
        (
            OneShotCommand::Auth,
            &["listenbrainz"],
            CliPersistenceCapability::Writer,
        ),
        (
            OneShotCommand::Auth,
            &["spotify", "--help", "--status"],
            CliPersistenceCapability::NotRequired,
        ),
        (
            OneShotCommand::Auth,
            &["spotify", "--client-id", "--help"],
            CliPersistenceCapability::Writer,
        ),
        (
            OneShotCommand::Auth,
            &["spotify", "--client-id", "client", "--help"],
            CliPersistenceCapability::NotRequired,
        ),
        (
            OneShotCommand::Auth,
            &["spotify", "--logout", "--help"],
            CliPersistenceCapability::NotRequired,
        ),
        (
            OneShotCommand::Transfer,
            &["help"],
            CliPersistenceCapability::NotRequired,
        ),
        (
            OneShotCommand::Transfer,
            &["download", "--help"],
            CliPersistenceCapability::ReadOnly,
        ),
        (
            OneShotCommand::Transfer,
            &["import", "--help"],
            CliPersistenceCapability::Writer,
        ),
        (
            OneShotCommand::Transfer,
            &["review"],
            CliPersistenceCapability::NotRequired,
        ),
        (
            OneShotCommand::Transfer,
            &["review", "job", "--all", "accept", "1"],
            CliPersistenceCapability::ReadOnly,
        ),
        (
            OneShotCommand::Transfer,
            &["review", "job", "--help"],
            CliPersistenceCapability::Writer,
        ),
        (
            OneShotCommand::Transfer,
            &["organize", "job", "--root", "--apply", "--dry-run"],
            CliPersistenceCapability::ReadOnly,
        ),
        (
            OneShotCommand::Transfer,
            &[
                "organize",
                "job",
                "--template",
                "--apply",
                "--root",
                "/tmp/library",
                "--dry-run",
            ],
            CliPersistenceCapability::ReadOnly,
        ),
        (
            OneShotCommand::Transfer,
            &[
                "organize",
                "--dry-run",
                "--root",
                "/tmp/one",
                "job",
                "--root",
                "/tmp/two",
            ],
            CliPersistenceCapability::ReadOnly,
        ),
        (
            OneShotCommand::Transfer,
            &[
                "organize",
                "job",
                "--root",
                "/tmp/library",
                "--dry-run",
                "--apply",
            ],
            CliPersistenceCapability::Writer,
        ),
        (
            OneShotCommand::Tools,
            &[],
            CliPersistenceCapability::ReadOnly,
        ),
        (
            OneShotCommand::Tools,
            &["status", "--unknown", "ignored"],
            CliPersistenceCapability::ReadOnly,
        ),
        (
            OneShotCommand::Tools,
            &["use"],
            CliPersistenceCapability::NotRequired,
        ),
        (
            OneShotCommand::Tools,
            &["use", "--help"],
            CliPersistenceCapability::Writer,
        ),
        (
            OneShotCommand::Tools,
            &["unknown"],
            CliPersistenceCapability::NotRequired,
        ),
        (
            OneShotCommand::Doctor,
            &[],
            CliPersistenceCapability::ReadOnly,
        ),
        (
            OneShotCommand::Doctor,
            &["--help", "ignored"],
            CliPersistenceCapability::NotRequired,
        ),
        (
            OneShotCommand::Doctor,
            &["privacy", "--cleanup"],
            CliPersistenceCapability::Writer,
        ),
        (
            OneShotCommand::Doctor,
            &["privacy", "--cleanup", "extra"],
            CliPersistenceCapability::ReadOnly,
        ),
        (
            OneShotCommand::Doctor,
            &["privacy", "--help"],
            CliPersistenceCapability::ReadOnly,
        ),
        (
            OneShotCommand::Update,
            &["--help", "ignored"],
            CliPersistenceCapability::ReadOnly,
        ),
    ];

    for &(command, values, expected) in cases {
        assert_eq!(
            command.persistence_capability(&args(values)),
            expected,
            "command {command:?}, args {values:?}"
        );
    }
}

#[derive(Clone, Copy)]
struct DeterministicFuzz(u64);

impl DeterministicFuzz {
    fn next(&mut self) -> u64 {
        self.0 = self
            .0
            .wrapping_mul(6_364_136_223_846_793_005)
            .wrapping_add(1_442_695_040_888_963_407);
        self.0
    }

    fn token(&mut self) -> String {
        const TOKENS: &[&str] = &[
            "",
            "auth",
            "spotify",
            "lastfm",
            "listenbrainz",
            "list",
            "jobs",
            "sessions",
            "session",
            "report",
            "review",
            "download",
            "organize",
            "import",
            "export",
            "backup",
            "resume",
            "status",
            "use",
            "unpin",
            "update",
            "reset",
            "diagnose",
            "privacy",
            "--cleanup",
            "--client-id",
            "--status",
            "--logout",
            "--help",
            "-h",
            "help",
            "--all",
            "--review",
            "--accepted",
            "--rejected",
            "--skipped",
            "--undecided",
            "--root",
            "--template",
            "--dry-run",
            "--apply",
            "--yes",
            "--unknown",
            "sp2yt-1",
            "/tmp/library",
        ];
        if !self.next().is_multiple_of(4) {
            return TOKENS[self.next() as usize % TOKENS.len()].to_owned();
        }

        const CHARS: &[char] = &[
            'a', 'Z', '0', '-', '_', '/', '\\', ' ', '\0', 'é', '한', '🦀', '\u{fffd}',
        ];
        let len = self.next() as usize % 24;
        (0..len)
            .map(|_| CHARS[self.next() as usize % CHARS.len()])
            .collect()
    }

    fn argv(&mut self) -> Vec<String> {
        let len = self.next() as usize % 12;
        (0..len).map(|_| self.token()).collect()
    }
}

fn model(command: OneShotCommand, args: &[String]) -> CliPersistenceCapability {
    match command {
        OneShotCommand::Auth => match args.first().map(String::as_str) {
            Some("lastfm" | "listenbrainz") => CliPersistenceCapability::Writer,
            Some("spotify") => model_spotify(&args[1..]),
            _ => CliPersistenceCapability::NotRequired,
        },
        OneShotCommand::Transfer => model_transfer(args),
        OneShotCommand::Doctor => {
            if args == ["privacy", "--cleanup"] {
                CliPersistenceCapability::Writer
            } else if matches!(
                args.first().map(String::as_str),
                Some("--help" | "-h" | "help")
            ) {
                CliPersistenceCapability::NotRequired
            } else {
                CliPersistenceCapability::ReadOnly
            }
        }
        OneShotCommand::Tools => match args.first().map(String::as_str) {
            None | Some("status") => CliPersistenceCapability::ReadOnly,
            Some("use" | "reset") if args.len() == 1 => CliPersistenceCapability::NotRequired,
            Some("use" | "unpin" | "update" | "reset" | "diagnose") => {
                CliPersistenceCapability::Writer
            }
            Some(_) => CliPersistenceCapability::NotRequired,
        },
        OneShotCommand::Update => CliPersistenceCapability::ReadOnly,
    }
}

fn model_spotify(args: &[String]) -> CliPersistenceCapability {
    let mut index = 0;
    while index < args.len() {
        match args[index].as_str() {
            "--client-id" if index + 1 < args.len() => index += 2,
            "--status" | "--logout" => index += 1,
            "--client-id" | "--help" | "-h" => {
                return CliPersistenceCapability::NotRequired;
            }
            _ => return CliPersistenceCapability::NotRequired,
        }
    }
    CliPersistenceCapability::Writer
}

fn model_transfer(args: &[String]) -> CliPersistenceCapability {
    match args.first().map(String::as_str) {
        Some("list" | "jobs" | "sessions" | "session" | "report" | "download") => {
            CliPersistenceCapability::ReadOnly
        }
        Some("review") if args.len() == 1 => CliPersistenceCapability::NotRequired,
        Some("review") => match args.get(2).map(String::as_str) {
            None
            | Some(
                "--all" | "--review" | "--accepted" | "--rejected" | "--skipped" | "--undecided",
            ) => CliPersistenceCapability::ReadOnly,
            Some(_) => CliPersistenceCapability::Writer,
        },
        Some("organize") => model_organize(&args[1..]),
        Some("import" | "export" | "backup" | "resume") => CliPersistenceCapability::Writer,
        _ => CliPersistenceCapability::NotRequired,
    }
}

fn model_organize(args: &[String]) -> CliPersistenceCapability {
    let mut dry_run = false;
    let mut apply = false;
    let mut session_count = 0;
    let mut root_count = 0;
    let mut invalid = false;
    let mut index = 0;
    while index < args.len() {
        match args[index].as_str() {
            "--dry-run" => dry_run = true,
            "--apply" => apply = true,
            "--yes" => {}
            "--root" if index + 1 < args.len() => {
                root_count += 1;
                index += 1;
            }
            "--template" if index + 1 < args.len() => index += 1,
            "--root" | "--template" | "--help" | "-h" => invalid = true,
            other if other.starts_with('-') => invalid = true,
            _ => session_count += 1,
        }
        index += 1;
    }
    if !invalid && session_count == 1 && root_count >= 1 && dry_run && !apply {
        CliPersistenceCapability::ReadOnly
    } else {
        CliPersistenceCapability::Writer
    }
}

#[test]
fn classifier_matches_reference_model_across_deterministic_fuzz_corpus() {
    const COMMANDS: [OneShotCommand; 5] = [
        OneShotCommand::Auth,
        OneShotCommand::Transfer,
        OneShotCommand::Doctor,
        OneShotCommand::Tools,
        OneShotCommand::Update,
    ];
    let mut fuzz = DeterministicFuzz(0x6a09_e667_f3bc_c909);
    for case in 0..25_000 {
        let args = fuzz.argv();
        for command in COMMANDS {
            let actual = command.persistence_capability(&args);
            assert_eq!(
                actual,
                model(command, &args),
                "case {case}, command {command:?}, args {args:?}"
            );
            assert_eq!(
                actual,
                command.persistence_capability(&args),
                "classification must be deterministic"
            );
        }
    }
}

#[test]
fn mutating_transfer_routes_never_downgrade_for_fuzzed_suffixes() {
    let mut fuzz = DeterministicFuzz(0xbb67_ae85_84ca_a73b);
    for route in ["import", "export", "backup", "resume"] {
        for _ in 0..2_000 {
            let mut candidate = vec![route.to_owned()];
            candidate.extend(fuzz.argv());
            assert_eq!(
                OneShotCommand::Transfer.persistence_capability(&candidate),
                CliPersistenceCapability::Writer,
                "{candidate:?}"
            );
        }
    }
}

#[test]
fn fixed_auth_and_observational_transfer_routes_ignore_fuzzed_suffixes_safely() {
    let mut fuzz = DeterministicFuzz(0x3c6e_f372_fe94_f82b);
    for route in ["lastfm", "listenbrainz"] {
        for _ in 0..2_000 {
            let mut candidate = vec![route.to_owned()];
            candidate.extend(fuzz.argv());
            assert_eq!(
                OneShotCommand::Auth.persistence_capability(&candidate),
                CliPersistenceCapability::Writer,
                "{candidate:?}"
            );
        }
    }
    for route in ["list", "jobs", "sessions", "session", "report", "download"] {
        for _ in 0..2_000 {
            let mut candidate = vec![route.to_owned()];
            candidate.extend(fuzz.argv());
            assert_eq!(
                OneShotCommand::Transfer.persistence_capability(&candidate),
                CliPersistenceCapability::ReadOnly,
                "{candidate:?}"
            );
        }
    }
}

#[cfg(unix)]
#[test]
fn non_unicode_unix_arguments_are_normalized_without_panicking() {
    use std::os::unix::ffi::OsStringExt;

    let normalized = collect_lossy_cli_args([
        OsString::from("organize"),
        OsString::from_vec(vec![b's', b'p', 0xff, b'1']),
        OsString::from("--root"),
        OsString::from_vec(vec![b'/', 0xfe, b'x']),
        OsString::from("--dry-run"),
    ]);
    assert_eq!(
        OneShotCommand::Transfer.persistence_capability(&normalized),
        CliPersistenceCapability::ReadOnly
    );
    let replaced = normalized
        .iter()
        .find(|arg| arg.contains('\u{fffd}'))
        .expect("invalid bytes must be replaced");
    for keyword in [
        "--apply",
        "--dry-run",
        "--root",
        "--template",
        "--help",
        "-h",
    ] {
        assert_ne!(replaced, keyword);
    }

    let invalid_action = collect_lossy_cli_args([
        OsString::from("review"),
        OsString::from("job"),
        OsString::from_vec(vec![b'a', 0xff, b'c']),
    ]);
    assert_eq!(
        OneShotCommand::Transfer.persistence_capability(&invalid_action),
        CliPersistenceCapability::Writer
    );
}

#[cfg(windows)]
#[test]
fn non_unicode_windows_arguments_are_normalized_without_panicking() {
    use std::os::windows::ffi::OsStringExt;

    let normalized = collect_lossy_cli_args([
        OsString::from("spotify"),
        OsString::from("--client-id"),
        OsString::from_wide(&[0xd800]),
    ]);
    assert_eq!(
        OneShotCommand::Auth.persistence_capability(&normalized),
        CliPersistenceCapability::Writer
    );
    let replaced = normalized
        .iter()
        .find(|arg| arg.contains('\u{fffd}'))
        .expect("unpaired surrogate must be replaced");
    for keyword in ["--client-id", "--status", "--logout", "--help", "-h"] {
        assert_ne!(replaced, keyword);
    }

    let invalid_action = collect_lossy_cli_args([
        OsString::from("review"),
        OsString::from("job"),
        OsString::from_wide(&[0xd800]),
    ]);
    assert_eq!(
        OneShotCommand::Transfer.persistence_capability(&invalid_action),
        CliPersistenceCapability::Writer
    );
}
