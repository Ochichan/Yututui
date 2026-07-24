//! One-shot `ytt data` commands. Personal export deliberately stays out of normal TUI startup:
//! a capable running owner supplies the freshest in-memory snapshot, while an offline invocation
//! loads persisted state through `data_export`.

use std::fs;
use std::path::{Path, PathBuf};

#[cfg(unix)]
use std::os::unix::fs::PermissionsExt as _;

use yututui::remote::{self, PERSONAL_EXPORT_CAPABILITY, PERSONAL_STATE_V2_CAPABILITY};

const EXIT_OK: i32 = 0;
const EXIT_RUNTIME: i32 = 1;
const EXIT_USAGE: i32 = 2;

const DATA_USAGE: &str = "\
Usage: ytt data <command>

Export portable personal data without credentials, machine paths, or media files.

Commands:
  export [--to DIR] [--schema 1|2]
                          Export personal state (schema 2 by default)
  import <FILE> [--dry-run] [--apply]
                          Preview or apply a personal-state import

Options:
  -h, --help              Show this help
";

const EXPORT_USAGE: &str = "\
Usage: ytt data export [--to DIR] [--schema 1|2]

Write one versioned JSON export. By default, DIR is the operating system's Downloads
folder. An explicit DIR must already exist.

Privacy: the unencrypted JSON includes private listening history. Credentials, filesystem
paths, and media files are excluded.

Options:
      --to DIR            Existing destination directory
      --schema 1|2        Export schema (default: 2)
  -h, --help              Show this help
";

const PRIVACY_NOTE: &str =
    "Private listening history is included; credentials, filesystem paths, and media are excluded.";
const PRIMARY_SNAPSHOT_NOTE: &str = "When `--new-instance` players are open, this command exports \
only the advertised primary; export each secondary from its Settings screen.";

const IMPORT_USAGE: &str = "\
Usage: ytt data import <FILE> [--dry-run] [--apply]

Preview a v1 or v2 personal-data import. No data is changed unless --apply is present.
Foreign datasets use a deletion-free merge; same-dataset v2 bundles use causal merge.

Options:
      --dry-run           Preview only (the default)
      --apply             Atomically apply the previewed merge
  -h, --help              Show this help
";

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
        "import" => run_import(&args[1..]),
        other => data_usage_error(&format!("unknown command `{other}`")),
    }
}

fn run_import(args: &[String]) -> i32 {
    let request = match parse_import_args(args) {
        Ok(ParseImport::Help) => {
            print!("{IMPORT_USAGE}");
            return EXIT_OK;
        }
        Ok(ParseImport::Request(request)) => request,
        Err(message) => return import_usage_error(&message),
    };
    let path = match resolve_import_source(&request.file) {
        Ok(path) => path,
        Err(message) => {
            eprintln!("ytt data import: {message}");
            return EXIT_RUNTIME;
        }
    };
    let access = if request.apply {
        yututui::persist::initialize_persistence_writer(false)
    } else {
        yututui::persist::initialize_persistence_reader()
    };
    if let Err(error) = access {
        let message = if error.kind() == std::io::ErrorKind::WouldBlock {
            "another ytt process owns personal data. Close it before applying an import".to_owned()
        } else {
            format!(
                "could not secure a coherent personal-state snapshot: {}",
                yututui::util::sanitize::sanitize_error_text(error.to_string())
            )
        };
        eprintln!("ytt data import: {message}");
        return EXIT_RUNTIME;
    }

    let planned = if request.apply {
        yututui::data_import::plan_from_file(&path)
    } else {
        yututui::data_import::preview_from_file(&path)
    };
    let (paths, plan) = match planned {
        Ok(plan) => plan,
        Err(error) => {
            eprintln!(
                "ytt data import: {}",
                yututui::util::sanitize::sanitize_error_text(error.to_string())
            );
            return EXIT_RUNTIME;
        }
    };
    print_import_summary(&plan.summary);
    if !request.apply {
        println!("Preview only; run again with --apply to save these changes.");
        return EXIT_OK;
    }
    if !plan.summary.changed {
        println!("No changes were needed.");
        return EXIT_OK;
    }
    match yututui::data_import::apply_plan(&paths, plan) {
        Ok(_) => {
            println!("Personal data import applied atomically.");
            EXIT_OK
        }
        Err(error) => {
            eprintln!(
                "ytt data import: {}",
                yututui::util::sanitize::sanitize_error_text(error.to_string())
            );
            EXIT_RUNTIME
        }
    }
}

fn print_import_summary(summary: &yututui::personal_state::ImportSummary) {
    println!(
        "Import preview: {} operation(s), +{} favorite(s), +{} history item(s), \
         +{} radio favorite(s), +{} playlist(s), +{} playlist item(s), +{} signal track(s).",
        summary.operations_added,
        summary.favorites_added,
        summary.history_added,
        summary.radio_favorites_added,
        summary.playlists_added,
        summary.playlist_entries_added,
        summary.signal_tracks_added,
    );
    if summary.duplicate_operations > 0 {
        println!(
            "{} operation(s) were already present and will not be applied again.",
            summary.duplicate_operations
        );
    }
}

fn run_export(args: &[String]) -> i32 {
    let requested = match parse_export_args(args) {
        Ok(ParseExport::Help) => {
            print!("{EXPORT_USAGE}");
            return EXIT_OK;
        }
        Ok(ParseExport::Request(request)) => request,
        Err(message) => return export_usage_error(&message),
    };

    let directory = match resolve_destination(requested.destination.as_deref()) {
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
        export_capability(requested.schema),
    )) {
        Ok(instance) => instance,
        Err(error) => {
            eprintln!("ytt data export: {}", error.human_message());
            return EXIT_RUNTIME;
        }
    };

    let result = if let Some(instance) = instance {
        export_through_owner_or_recover_stale(&runtime, instance, &directory, requested.schema)
    } else {
        export_offline_with_guard(&runtime, &directory, requested.schema)
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

/// Close the discovery-to-read race with the same process-wide writer lease used by every other
/// mutating owner. A primary that starts between the first lookup and lease acquisition is retried
/// through its live owner; an unpublished owner fails closed instead of permitting a mixed disk
/// snapshot.
fn export_offline_with_guard(
    runtime: &tokio::runtime::Runtime,
    directory: &Path,
    schema: u32,
) -> Result<PathBuf, String> {
    if let Err(error) = yututui::persist::initialize_persistence_writer(false) {
        if error.kind() == std::io::ErrorKind::WouldBlock {
            return match runtime.block_on(remote::client::instance_with_capability(
                export_capability(schema),
            )) {
                Ok(Some(instance)) => export_through_owner(runtime, instance, directory, schema)
                    .map_err(|error| owner_export_error_message(error, directory)),
                Ok(None) | Err(_) => Err(
                    "another ytt process owns personal data but is not an export-capable primary. \
                     Export from that instance's Settings screen, or close every ytt instance \
                     (including `--new-instance`) and retry."
                        .to_string(),
                ),
            };
        }
        return Err(format!(
            "could not secure a coherent offline snapshot: {}",
            yututui::util::sanitize::sanitize_error_text(error.to_string())
        ));
    }

    // Recheck after establishing the process-wide lease. Current-version owners cannot cross this
    // point; this catches a compatible older primary that published during acquisition.
    match runtime.block_on(remote::client::instance_with_capability(export_capability(
        schema,
    ))) {
        Ok(Some(instance)) => {
            export_through_owner_or_recover_stale(runtime, instance, directory, schema)
        }
        Ok(None) => {
            yututui::data_export::export_from_disk_with_schema(directory, export_schema(schema))
                .map_err(offline_export_error_message)
        }
        Err(error) => Err(error.human_message()),
    }
}

/// Recover only from a stale advertised transport endpoint. The descriptor remains in place;
/// the process-wide writer lease prevents current-version owners from starting while the
/// canonical current and legacy primary endpoints are independently checked for absence.
fn export_offline_after_stale_descriptor(
    runtime: &tokio::runtime::Runtime,
    directory: &Path,
    schema: u32,
) -> Result<PathBuf, String> {
    if let Err(error) = yututui::persist::initialize_persistence_writer(false) {
        if error.kind() == std::io::ErrorKind::WouldBlock {
            return Err(
                "the advertised primary became unreachable, but another ytt process still owns \
                 personal data; refusing an offline snapshot. Export from that instance's \
                 Settings screen, or close every ytt instance (including `--new-instance`) and \
                 retry."
                    .to_string(),
            );
        }
        return Err(format!(
            "could not secure a coherent offline snapshot: {}",
            yututui::util::sanitize::sanitize_error_text(error.to_string())
        ));
    }

    if let Err(error) = runtime.block_on(remote::client::prove_primary_endpoints_absent()) {
        return Err(error.human_message());
    }

    yututui::data_export::export_from_disk_with_schema(directory, export_schema(schema))
        .map_err(offline_export_error_message)
}

fn offline_export_error_message(error: yututui::data_export::ExportError) -> String {
    yututui::util::sanitize::sanitize_error_text(format!("could not export personal data: {error}"))
}

fn export_through_owner(
    runtime: &tokio::runtime::Runtime,
    instance: remote::proto::InstanceFile,
    directory: &Path,
    schema: u32,
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
        schema: Some(schema),
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

fn export_through_owner_or_recover_stale(
    runtime: &tokio::runtime::Runtime,
    instance: remote::proto::InstanceFile,
    directory: &Path,
    schema: u32,
) -> Result<PathBuf, String> {
    match export_through_owner(runtime, instance, directory, schema) {
        Ok(path) => Ok(path),
        Err(error) if permits_stale_descriptor_recovery(&error) => {
            export_offline_after_stale_descriptor(runtime, directory, schema)
        }
        Err(error) => Err(owner_export_error_message(error, directory)),
    }
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
    Request(ExportRequest),
}

enum ParseImport {
    Help,
    Request(ImportRequest),
}

struct ImportRequest {
    file: String,
    apply: bool,
}

fn parse_import_args(args: &[String]) -> Result<ParseImport, String> {
    if args
        .iter()
        .any(|argument| matches!(argument.as_str(), "-h" | "--help"))
    {
        return Ok(ParseImport::Help);
    }
    let mut file = None;
    let mut apply = false;
    let mut dry_run = false;
    for argument in args {
        match argument.as_str() {
            "--apply" if apply => return Err("`--apply` may only be specified once".to_owned()),
            "--apply" => apply = true,
            "--dry-run" if dry_run => {
                return Err("`--dry-run` may only be specified once".to_owned());
            }
            "--dry-run" => dry_run = true,
            option if option.starts_with('-') => {
                return Err(format!("unexpected option `{option}`"));
            }
            value => {
                if file.is_some() {
                    return Err("exactly one import file is required".to_owned());
                }
                file = Some(value.to_owned());
            }
        }
    }
    if apply && dry_run {
        return Err("`--apply` and `--dry-run` cannot be used together".to_owned());
    }
    let file = file.ok_or_else(|| "an import file is required".to_owned())?;
    Ok(ParseImport::Request(ImportRequest { file, apply }))
}

struct ExportRequest {
    destination: Option<String>,
    schema: u32,
}

fn parse_export_args(args: &[String]) -> Result<ParseExport, String> {
    if args
        .iter()
        .any(|argument| matches!(argument.as_str(), "-h" | "--help"))
    {
        return Ok(ParseExport::Help);
    }

    let mut destination = None;
    let mut schema = None;
    let mut index = 0;
    while index < args.len() {
        let argument = &args[index];
        if argument == "--schema" || argument.starts_with("--schema=") {
            let raw = if argument == "--schema" {
                index += 1;
                args.get(index)
                    .filter(|value| !value.is_empty())
                    .ok_or_else(|| "`--schema` requires 1 or 2".to_string())?
                    .as_str()
            } else {
                argument
                    .strip_prefix("--schema=")
                    .filter(|value| !value.is_empty())
                    .ok_or_else(|| "`--schema` requires 1 or 2".to_string())?
            };
            let parsed = raw
                .parse::<u32>()
                .ok()
                .filter(|value| matches!(value, 1 | 2))
                .ok_or_else(|| "`--schema` must be 1 or 2".to_string())?;
            if schema.replace(parsed).is_some() {
                return Err("`--schema` may only be specified once".to_string());
            }
            index += 1;
            continue;
        }
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

    Ok(ParseExport::Request(ExportRequest {
        destination,
        schema: schema.unwrap_or(2),
    }))
}

fn export_schema(schema: u32) -> yututui::data_export::ExportSchema {
    if schema == 1 {
        yututui::data_export::ExportSchema::V1
    } else {
        yututui::data_export::ExportSchema::V2
    }
}

fn export_capability(schema: u32) -> &'static str {
    if schema == 1 {
        PERSONAL_EXPORT_CAPABILITY
    } else {
        PERSONAL_STATE_V2_CAPABILITY
    }
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
        .any(is_terminal_unsafe_character)
    {
        return Err(
            "destination paths cannot contain control or bidirectional characters".to_string(),
        );
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
    let canonical = fs::canonicalize(&path).map_err(|error| {
        format!(
            "could not resolve destination `{}`: {error}",
            path.display()
        )
    })?;
    if canonical
        .to_string_lossy()
        .chars()
        .any(is_terminal_unsafe_character)
    {
        return Err("destination paths cannot contain control or bidirectional characters".into());
    }
    Ok(canonical)
}

fn resolve_import_source(raw: &str) -> Result<PathBuf, String> {
    let path = expand_tilde(raw)?;
    let path = std::path::absolute(path)
        .map_err(|error| format!("could not resolve import file: {error}"))?;
    if path
        .to_string_lossy()
        .chars()
        .any(is_terminal_unsafe_character)
    {
        return Err("import paths cannot contain control or bidirectional characters".to_owned());
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

fn import_usage_error(message: &str) -> i32 {
    eprintln!("ytt data import: {message}\n\n{IMPORT_USAGE}");
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
        assert_eq!(run(&strings(&["import", "--help"])), EXIT_OK);
        assert_eq!(run(&[]), EXIT_USAGE);
        assert_eq!(run(&strings(&["unknown"])), EXIT_USAGE);
    }

    #[test]
    fn export_parser_accepts_default_and_one_destination() {
        let ParseExport::Request(request) = parse_export_args(&[]).unwrap() else {
            panic!("expected export request");
        };
        assert_eq!(request.destination, None);
        assert_eq!(request.schema, 2);
        match parse_export_args(&strings(&["--to", "/tmp"])).unwrap() {
            ParseExport::Request(request) => {
                assert_eq!(request.destination.as_deref(), Some("/tmp"));
                assert_eq!(request.schema, 2);
            }
            _ => panic!("expected destination"),
        }
        match parse_export_args(&strings(&["--to=/tmp", "--schema", "1"])).unwrap() {
            ParseExport::Request(request) => {
                assert_eq!(request.destination.as_deref(), Some("/tmp"));
                assert_eq!(request.schema, 1);
            }
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
            strings(&["--schema", "3"]),
            strings(&["--schema=1", "--schema=2"]),
            strings(&["unexpected"]),
        ] {
            assert!(parse_export_args(&args).is_err(), "accepted {args:?}");
        }
    }

    #[test]
    fn import_parser_is_preview_first_and_apply_is_explicit() {
        let ParseImport::Request(preview) = parse_import_args(&strings(&["backup.json"])).unwrap()
        else {
            panic!("expected import request");
        };
        assert_eq!(preview.file, "backup.json");
        assert!(!preview.apply);

        let ParseImport::Request(apply) =
            parse_import_args(&strings(&["backup.json", "--apply"])).unwrap()
        else {
            panic!("expected import request");
        };
        assert!(apply.apply);
        assert!(parse_import_args(&strings(&["backup.json", "--apply", "--dry-run"])).is_err());
        assert!(parse_import_args(&[]).is_err());
        assert!(parse_import_args(&strings(&["one.json", "two.json"])).is_err());
    }

    #[test]
    fn explicit_destination_must_be_an_existing_real_directory() {
        let root = std::env::temp_dir().join(format!(
            "yututui-data-cli-{}-destination",
            std::process::id()
        ));
        let _ = fs::remove_dir_all(&root);
        fs::create_dir(&root).unwrap();

        assert_eq!(
            resolve_destination(root.to_str()).unwrap(),
            fs::canonicalize(&root).unwrap()
        );
        let via_parent = root.join("child").join("..");
        fs::create_dir(root.join("child")).unwrap();
        assert_eq!(
            resolve_destination(via_parent.to_str()).unwrap(),
            fs::canonicalize(&root).unwrap(),
            "the CLI and live owner must compare the same canonical directory"
        );
        let missing = root.join("missing");
        assert!(resolve_destination(missing.to_str()).is_err());
        assert!(resolve_destination(Some("bad\npath")).is_err());

        fs::remove_dir_all(&root).unwrap();
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

    #[cfg(unix)]
    #[test]
    fn intermediate_symlink_is_canonicalized_before_owner_path_validation() {
        use std::os::unix::fs::{PermissionsExt, symlink};

        let root = std::env::temp_dir().join(format!(
            "yututui-data-cli-intermediate-link-{}",
            std::process::id()
        ));
        let alias = root.with_extension("alias");
        let _ = fs::remove_file(&alias);
        let _ = fs::remove_dir_all(&root);
        let destination = root.join("exports");
        fs::create_dir_all(&destination).unwrap();
        symlink(&root, &alias).unwrap();

        let resolved = resolve_destination(alias.join("exports").to_str()).unwrap();
        assert_eq!(resolved, fs::canonicalize(&destination).unwrap());
        let export = resolved.join("yututui-personal-data-v1-1783704534-0123456789abcdef.json");
        fs::write(&export, b"{}\n").unwrap();
        fs::set_permissions(&export, fs::Permissions::from_mode(0o600)).unwrap();
        assert_eq!(
            validate_owner_export_path(&resolved, export.to_str().unwrap()).unwrap(),
            export
        );

        fs::remove_file(alias).unwrap();
        fs::remove_dir_all(root).unwrap();
    }

    #[cfg(unix)]
    #[test]
    fn canonical_destination_rejects_hidden_terminal_controls() {
        use std::os::unix::fs::symlink;

        let root = std::env::temp_dir().join(format!(
            "yututui-data-cli-hidden-control-{}",
            std::process::id()
        ));
        let alias = root.with_extension("alias");
        let _ = fs::remove_file(&alias);
        let _ = fs::remove_dir_all(&root);
        let hidden = root.join("private\u{202e}target");
        fs::create_dir_all(hidden.join("exports")).unwrap();
        symlink(&hidden, &alias).unwrap();

        assert!(
            resolve_destination(alias.join("exports").to_str()).is_err(),
            "canonical target controls must be rejected before export"
        );

        fs::remove_file(alias).unwrap();
        fs::remove_dir_all(root).unwrap();
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
