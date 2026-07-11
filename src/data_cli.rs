//! One-shot `ytt data` commands. Personal export deliberately stays out of normal TUI startup:
//! a capable running owner supplies the freshest in-memory snapshot, while an offline invocation
//! loads persisted state through `data_export`.

use std::fs;
use std::path::{Path, PathBuf};

#[cfg(unix)]
use std::os::unix::fs::PermissionsExt as _;

use yututui::data_ownership::{self, AcquireError};
use yututui::remote::{self, PERSONAL_EXPORT_CAPABILITY};

const EXIT_OK: i32 = 0;
const EXIT_RUNTIME: i32 = 1;
const EXIT_USAGE: i32 = 2;

const DATA_USAGE: &str = "\
Usage: ytt data <command>

Export portable personal data without credentials, machine paths, or media files.

Commands:
  export [--to DIR]       Export settings, library, history, playlists, and preferences

Options:
  -h, --help              Show this help
";

const EXPORT_USAGE: &str = "\
Usage: ytt data export [--to DIR]

Write one versioned JSON export. By default, DIR is the operating system's Downloads
folder. An explicit DIR must already exist.

Privacy: the unencrypted JSON includes private listening history. Credentials, filesystem
paths, and media files are excluded.

Options:
      --to DIR            Existing destination directory
  -h, --help              Show this help
";

const PRIVACY_NOTE: &str =
    "Private listening history is included; credentials, filesystem paths, and media are excluded.";
const PRIMARY_SNAPSHOT_NOTE: &str = "When `--new-instance` players are open, this command exports \
only the advertised primary; export each secondary from its Settings screen.";

#[derive(Debug)]
enum OwnerExportError {
    Transport(remote::client::ClientError),
    Message(String),
}

pub fn run(args: &[String]) -> i32 {
    let Some(command) = args.first().map(String::as_str) else {
        return data_usage_error("missing command");
    };

    match command {
        "-h" | "--help" => {
            print!("{DATA_USAGE}");
            EXIT_OK
        }
        "export" => run_export(&args[1..]),
        other => data_usage_error(&format!("unknown command `{other}`")),
    }
}

fn run_export(args: &[String]) -> i32 {
    let requested = match parse_export_args(args) {
        Ok(ParseExport::Help) => {
            print!("{EXPORT_USAGE}");
            return EXIT_OK;
        }
        Ok(ParseExport::Destination(path)) => path,
        Err(message) => return export_usage_error(&message),
    };

    let directory = match resolve_destination(requested.as_deref()) {
        Ok(path) => path,
        Err(message) => {
            eprintln!("ytt data export: {message}");
            return EXIT_RUNTIME;
        }
    };

    let runtime = match tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
    {
        Ok(runtime) => runtime,
        Err(error) => {
            eprintln!("ytt data export: could not start runtime: {error}");
            return EXIT_RUNTIME;
        }
    };
    let instance = match runtime.block_on(remote::client::instance_with_capability(
        PERSONAL_EXPORT_CAPABILITY,
    )) {
        Ok(instance) => instance,
        Err(error) => {
            eprintln!("ytt data export: {}", error.human_message());
            return EXIT_RUNTIME;
        }
    };

    let result = if let Some(instance) = instance {
        match export_through_owner(&runtime, instance, &directory) {
            Ok(path) => Ok(path),
            Err(error) if permits_stale_descriptor_recovery(&error) => {
                export_offline_after_stale_descriptor(&runtime, &directory)
            }
            Err(error) => Err(owner_export_error_message(error, &directory)),
        }
    } else {
        export_offline_with_guard(&runtime, &directory)
    };

    match result {
        Ok(path) => {
            println!("Exported personal data to {}", path.display());
            println!("{PRIVACY_NOTE}");
            println!("{PRIMARY_SNAPSHOT_NOTE}");
            EXIT_OK
        }
        Err(message) => {
            eprintln!("ytt data export: {message}");
            EXIT_RUNTIME
        }
    }
}

/// Close the discovery-to-read race with a cross-process exclusive guard. A primary that starts
/// between the first lookup and lock acquisition is retried through its live owner; a secondary
/// or unpublished owner fails with guidance instead of permitting a mixed disk snapshot.
fn export_offline_with_guard(
    runtime: &tokio::runtime::Runtime,
    directory: &Path,
) -> Result<PathBuf, String> {
    let guard = match data_ownership::acquire_offline_export() {
        Ok(guard) => guard,
        Err(AcquireError::Busy) => {
            return match runtime.block_on(remote::client::instance_with_capability(
                PERSONAL_EXPORT_CAPABILITY,
            )) {
                Ok(Some(instance)) => export_through_owner(runtime, instance, directory)
                    .map_err(|error| owner_export_error_message(error, directory)),
                Ok(None) | Err(_) => Err(
                    "another ytt process owns personal data but is not an export-capable primary. \
                     Export from that instance's Settings screen, or close every ytt instance \
                     (including `--new-instance`) and retry."
                        .to_string(),
                ),
            };
        }
        Err(AcquireError::Io(error)) => {
            return Err(format!(
                "could not secure a coherent offline snapshot: {}",
                yututui::util::sanitize::sanitize_error_text(error.to_string())
            ));
        }
    };

    // Recheck while holding the exclusive guard. Current-version owners cannot cross this point;
    // this catches an older primary that published after the initial lookup.
    match runtime.block_on(remote::client::instance_with_capability(
        PERSONAL_EXPORT_CAPABILITY,
    )) {
        Ok(Some(instance)) => {
            drop(guard);
            export_through_owner(runtime, instance, directory)
                .map_err(|error| owner_export_error_message(error, directory))
        }
        Ok(None) => {
            let result = yututui::data_export::export_from_disk(directory)
                .map_err(offline_export_error_message);
            drop(guard);
            result
        }
        Err(error) => Err(error.human_message()),
    }
}

/// Recover only from a stale advertised transport endpoint. The descriptor remains in place;
/// the exclusive ownership guard prevents current-version owners from starting while the
/// canonical current and legacy primary endpoints are independently checked for absence.
fn export_offline_after_stale_descriptor(
    runtime: &tokio::runtime::Runtime,
    directory: &Path,
) -> Result<PathBuf, String> {
    let guard = match data_ownership::acquire_offline_export() {
        Ok(guard) => guard,
        Err(AcquireError::Busy) => {
            return Err(
                "the advertised primary became unreachable, but another ytt process still owns \
                 personal data; refusing an offline snapshot. Export from that instance's \
                 Settings screen, or close every ytt instance (including `--new-instance`) and \
                 retry."
                    .to_string(),
            );
        }
        Err(AcquireError::Io(error)) => {
            return Err(format!(
                "could not secure a coherent offline snapshot: {}",
                yututui::util::sanitize::sanitize_error_text(error.to_string())
            ));
        }
    };

    if let Err(error) = runtime.block_on(remote::client::prove_primary_endpoints_absent()) {
        return Err(error.human_message());
    }

    let result =
        yututui::data_export::export_from_disk(directory).map_err(offline_export_error_message);
    drop(guard);
    result
}

fn offline_export_error_message(error: yututui::data_export::ExportError) -> String {
    yututui::util::sanitize::sanitize_error_text(format!("could not export personal data: {error}"))
}

fn export_through_owner(
    runtime: &tokio::runtime::Runtime,
    instance: remote::proto::InstanceFile,
    directory: &Path,
) -> Result<PathBuf, OwnerExportError> {
    let requested_directory = directory;
    let Some(directory) = requested_directory.to_str() else {
        return Err(OwnerExportError::Message(
            "destination path is not valid UTF-8 and cannot be sent to the running instance"
                .to_string(),
        ));
    };
    let command = remote::proto::RemoteCommand::ExportPersonalData {
        directory: directory.to_string(),
    };
    let response = runtime
        .block_on(remote::client::send_to(instance, command))
        .map_err(OwnerExportError::Transport)?;

    if !response.ok {
        return Err(OwnerExportError::Message(
            response
                .message
                .or(response.reason)
                .unwrap_or_else(|| "export_rejected".to_string()),
        ));
    }
    let path = response
        .message
        .filter(|message| !message.trim().is_empty())
        .ok_or_else(|| {
            OwnerExportError::Message("the running instance returned no export path".to_string())
        })?;
    validate_owner_export_path(requested_directory, &path)
}

fn validate_owner_export_path(
    directory: &Path,
    response_path: &str,
) -> Result<PathBuf, OwnerExportError> {
    let invalid = || {
        OwnerExportError::Message(
            "the running instance returned an invalid or unverifiable export path".to_string(),
        )
    };
    if response_path.is_empty() || response_path.chars().any(is_terminal_unsafe_character) {
        return Err(invalid());
    }
    let path = PathBuf::from(response_path);
    let name = path
        .file_name()
        .and_then(|name| name.to_str())
        .ok_or_else(&invalid)?;
    if !path.is_absolute()
        || path.parent() != Some(directory)
        || !yututui::data_export::is_personal_export_file_name(name)
    {
        return Err(invalid());
    }
    let metadata = fs::symlink_metadata(&path).map_err(|_| invalid())?;
    if metadata.file_type().is_symlink()
        || !metadata.is_file()
        || metadata.len() == 0
        || metadata.len() > yututui::data_export::EXPORT_MAX_BYTES
    {
        return Err(invalid());
    }
    #[cfg(unix)]
    if !yututui::util::safe_fs::is_owned_by_current_user(&metadata)
        || metadata.permissions().mode() & 0o777 != 0o600
    {
        return Err(invalid());
    }
    Ok(path)
}

fn permits_stale_descriptor_recovery(error: &OwnerExportError) -> bool {
    matches!(
        error,
        OwnerExportError::Transport(remote::client::ClientError::ConnectFailed)
    )
}

fn owner_export_error_message(error: OwnerExportError, directory: &Path) -> String {
    match error {
        OwnerExportError::Transport(remote::client::ClientError::NoResponse) => format!(
            "the running instance did not confirm completion; the export may still finish in \
             `{}`. Check that directory before retrying",
            directory.display()
        ),
        OwnerExportError::Transport(error) => error.human_message(),
        OwnerExportError::Message(message) => terminal_safe_owner_message(message),
    }
}

fn terminal_safe_owner_message(message: impl AsRef<str>) -> String {
    yututui::util::sanitize::sanitize_error_text(message)
        .chars()
        .map(|character| {
            if is_terminal_unsafe_character(character) {
                '�'
            } else {
                character
            }
        })
        .collect()
}

fn is_terminal_unsafe_character(character: char) -> bool {
    character.is_control()
        || matches!(
            character,
            '\u{200b}'
                | '\u{200c}'
                | '\u{200d}'
                | '\u{200e}'
                | '\u{200f}'
                | '\u{202a}'..='\u{202e}'
                | '\u{2066}'..='\u{2069}'
                | '\u{feff}'
        )
}

enum ParseExport {
    Help,
    Destination(Option<String>),
}

fn parse_export_args(args: &[String]) -> Result<ParseExport, String> {
    if args
        .iter()
        .any(|argument| matches!(argument.as_str(), "-h" | "--help"))
    {
        return Ok(ParseExport::Help);
    }

    let mut destination = None;
    let mut index = 0;
    while index < args.len() {
        let argument = &args[index];
        let value = if argument == "--to" {
            index += 1;
            args.get(index)
                .filter(|value| !value.is_empty() && value.as_str() != "--to")
                .cloned()
                .ok_or_else(|| "`--to` requires a directory".to_string())?
        } else if let Some(value) = argument.strip_prefix("--to=") {
            if value.is_empty() {
                return Err("`--to` requires a directory".to_string());
            }
            value.to_string()
        } else {
            return Err(format!("unexpected argument `{argument}`"));
        };

        if destination.replace(value).is_some() {
            return Err("`--to` may only be specified once".to_string());
        }
        index += 1;
    }

    Ok(ParseExport::Destination(destination))
}

fn resolve_destination(requested: Option<&str>) -> Result<PathBuf, String> {
    let path = match requested {
        Some(raw) => expand_tilde(raw)?,
        None => directories::UserDirs::new()
            .and_then(|dirs| dirs.download_dir().map(Path::to_path_buf))
            .ok_or_else(|| {
                "could not find the operating system Downloads folder; use `--to DIR`".to_string()
            })?,
    };

    let path = std::path::absolute(&path).map_err(|error| {
        format!(
            "could not resolve destination `{}`: {error}",
            path.display()
        )
    })?;
    if path
        .to_string_lossy()
        .chars()
        .any(|character| character.is_control())
    {
        return Err("destination paths cannot contain control characters".to_string());
    }
    let metadata = fs::symlink_metadata(&path).map_err(|error| {
        if error.kind() == std::io::ErrorKind::NotFound {
            format!(
                "destination `{}` does not exist; create it first or choose another `--to DIR`",
                path.display()
            )
        } else {
            format!(
                "could not inspect destination `{}`: {error}",
                path.display()
            )
        }
    })?;
    if metadata.file_type().is_symlink() {
        return Err(format!(
            "destination `{}` is a symbolic link; choose the real directory",
            path.display()
        ));
    }
    if !metadata.is_dir() {
        return Err(format!(
            "destination `{}` is not a directory",
            path.display()
        ));
    }
    Ok(path)
}

fn expand_tilde(raw: &str) -> Result<PathBuf, String> {
    if raw == "~" {
        return home_dir();
    }
    if let Some(rest) = raw.strip_prefix("~/") {
        return Ok(home_dir()?.join(rest));
    }
    #[cfg(windows)]
    if let Some(rest) = raw.strip_prefix(r"~\") {
        return Ok(home_dir()?.join(rest));
    }
    if raw.starts_with('~') {
        return Err("only `~` and paths beginning with `~/` are supported".to_string());
    }
    Ok(PathBuf::from(raw))
}

fn home_dir() -> Result<PathBuf, String> {
    directories::UserDirs::new()
        .map(|dirs| dirs.home_dir().to_path_buf())
        .ok_or_else(|| "could not find the user home directory".to_string())
}

fn data_usage_error(message: &str) -> i32 {
    eprintln!("ytt data: {message}\n\n{DATA_USAGE}");
    EXIT_USAGE
}

fn export_usage_error(message: &str) -> i32 {
    eprintln!("ytt data export: {message}\n\n{EXPORT_USAGE}");
    EXIT_USAGE
}

#[cfg(test)]
mod tests {
    use super::*;

    fn strings(values: &[&str]) -> Vec<String> {
        values.iter().map(|value| value.to_string()).collect()
    }

    #[test]
    fn help_succeeds_and_missing_or_unknown_commands_are_usage_errors() {
        assert_eq!(run(&strings(&["--help"])), EXIT_OK);
        assert_eq!(run(&strings(&["export", "--help"])), EXIT_OK);
        assert_eq!(run(&[]), EXIT_USAGE);
        assert_eq!(run(&strings(&["unknown"])), EXIT_USAGE);
    }

    #[test]
    fn export_parser_accepts_default_and_one_destination() {
        assert!(matches!(
            parse_export_args(&[]).unwrap(),
            ParseExport::Destination(None)
        ));
        match parse_export_args(&strings(&["--to", "/tmp"])).unwrap() {
            ParseExport::Destination(Some(path)) => assert_eq!(path, "/tmp"),
            _ => panic!("expected destination"),
        }
        match parse_export_args(&strings(&["--to=/tmp"])).unwrap() {
            ParseExport::Destination(Some(path)) => assert_eq!(path, "/tmp"),
            _ => panic!("expected destination"),
        }
    }

    #[test]
    fn export_parser_help_is_successful() {
        assert!(matches!(
            parse_export_args(&strings(&["--help"])).unwrap(),
            ParseExport::Help
        ));
    }

    #[test]
    fn export_parser_rejects_missing_duplicate_and_unknown_arguments() {
        for args in [
            strings(&["--to"]),
            strings(&["--to="]),
            strings(&["--to", "/tmp", "--to", "/var/tmp"]),
            strings(&["unexpected"]),
        ] {
            assert!(parse_export_args(&args).is_err(), "accepted {args:?}");
        }
    }

    #[test]
    fn explicit_destination_must_be_an_existing_real_directory() {
        let root = std::env::temp_dir().join(format!(
            "yututui-data-cli-{}-{}",
            std::process::id(),
            std::thread::current().name().unwrap_or("destination")
        ));
        let _ = fs::remove_dir_all(&root);
        fs::create_dir(&root).unwrap();

        assert_eq!(resolve_destination(root.to_str()).unwrap(), root);
        let missing = root.join("missing");
        assert!(resolve_destination(missing.to_str()).is_err());
        assert!(resolve_destination(Some("bad\npath")).is_err());

        fs::remove_dir(&root).unwrap();
    }

    #[cfg(unix)]
    #[test]
    fn explicit_destination_rejects_a_symlink() {
        use std::os::unix::fs::symlink;

        let root =
            std::env::temp_dir().join(format!("yututui-data-cli-link-{}", std::process::id()));
        let link = root.with_extension("link");
        let _ = fs::remove_file(&link);
        let _ = fs::remove_dir_all(&root);
        fs::create_dir(&root).unwrap();
        symlink(&root, &link).unwrap();

        assert!(resolve_destination(link.to_str()).is_err());

        fs::remove_file(link).unwrap();
        fs::remove_dir(root).unwrap();
    }

    #[test]
    fn offline_export_error_redacts_the_user_home_path() {
        let Some(home) = std::env::var("HOME")
            .ok()
            .or_else(|| std::env::var("USERPROFILE").ok())
            .filter(|home| home.len() > 1)
        else {
            return;
        };
        let error = yututui::data_export::ExportError::SourceStore {
            store: "config",
            detail: format!("cannot safely read `{home}/private/config.json`"),
        };

        let message = offline_export_error_message(error);

        assert!(!message.contains(&home));
        assert!(message.contains("~/private/config.json"));
    }

    #[test]
    fn stale_descriptor_recovery_is_limited_to_connect_failures() {
        assert!(permits_stale_descriptor_recovery(
            &OwnerExportError::Transport(remote::client::ClientError::ConnectFailed)
        ));
        assert!(!permits_stale_descriptor_recovery(
            &OwnerExportError::Transport(remote::client::ClientError::MalformedEndpoint)
        ));
        assert!(!permits_stale_descriptor_recovery(
            &OwnerExportError::Transport(remote::client::ClientError::NoResponse)
        ));
        assert!(!permits_stale_descriptor_recovery(
            &OwnerExportError::Message("rejected".to_string())
        ));
    }

    #[test]
    fn owner_completion_path_must_be_a_private_export_in_the_requested_directory() {
        let root = std::env::temp_dir().join(format!(
            "yututui-data-cli-owner-path-{}",
            std::process::id()
        ));
        let _ = fs::remove_dir_all(&root);
        fs::create_dir(&root).expect("create destination");
        let path = root.join("yututui-personal-data-v1-1783704534-0123456789abcdef.json");
        fs::write(&path, b"{}\n").expect("write export fixture");
        #[cfg(unix)]
        fs::set_permissions(&path, fs::Permissions::from_mode(0o600)).expect("make export private");

        assert_eq!(
            validate_owner_export_path(&root, path.to_str().expect("UTF-8 fixture"))
                .expect("valid completion path"),
            path
        );
        assert!(
            validate_owner_export_path(
                root.parent().expect("temp parent"),
                path.to_str().expect("UTF-8 fixture")
            )
            .is_err(),
            "a different requested parent must be rejected"
        );
        assert!(
            validate_owner_export_path(&root, root.join("other.json").to_string_lossy().as_ref())
                .is_err(),
            "an unexpected file name must be rejected"
        );
        assert!(
            validate_owner_export_path(&root, &format!("{}\u{1b}[2J", path.display())).is_err(),
            "terminal controls must be rejected before printing"
        );

        fs::remove_dir_all(root).expect("cleanup");
    }

    #[cfg(unix)]
    #[test]
    fn owner_completion_path_rejects_a_symlink() {
        use std::os::unix::fs::symlink;

        let root = std::env::temp_dir().join(format!(
            "yututui-data-cli-owner-link-{}",
            std::process::id()
        ));
        let _ = fs::remove_dir_all(&root);
        fs::create_dir(&root).expect("create destination");
        let target = root.join("target");
        fs::write(&target, b"{}\n").expect("write target");
        let link = root.join("yututui-personal-data-v1-1783704534-fedcba9876543210.json");
        symlink(&target, &link).expect("create export-shaped symlink");

        assert!(validate_owner_export_path(&root, link.to_str().expect("UTF-8 fixture")).is_err());
        fs::remove_dir_all(root).expect("cleanup");
    }

    #[test]
    fn owner_messages_are_single_line_and_terminal_safe() {
        let message = terminal_safe_owner_message("failed\n\u{1b}[2J\u{202e}done");
        assert!(message.chars().all(|character| !character.is_control()));
        assert!(!message.contains('\u{202e}'));
        assert!(message.contains("failed"));
        assert!(message.contains("done"));
    }
}
