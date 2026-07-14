//! Typed persistence-capability classification for non-interactive CLI entrypoints.
//!
//! Classification deliberately runs before the command parser so help and provable usage errors
//! can exit without contending for the writer lease. Ambiguous forms fail closed as writers; only
//! syntactically provable observational forms are downgraded to read-only access.

use std::ffi::OsString;

#[must_use]
#[non_exhaustive]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CliPersistenceCapability {
    NotRequired,
    ReadOnly,
    Writer,
}

impl CliPersistenceCapability {
    pub const fn requires_persistence(self) -> bool {
        match self {
            Self::NotRequired => false,
            Self::ReadOnly | Self::Writer => true,
        }
    }

    pub const fn allows_writes(self) -> bool {
        match self {
            Self::NotRequired | Self::ReadOnly => false,
            Self::Writer => true,
        }
    }
}

#[must_use]
#[non_exhaustive]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum OneShotCommand {
    Auth,
    Transfer,
    Doctor,
    Tools,
    Update,
}

impl OneShotCommand {
    pub const fn label(self) -> &'static str {
        match self {
            Self::Auth => "auth",
            Self::Transfer => "transfer",
            Self::Doctor => "doctor",
            Self::Tools => "tools",
            Self::Update => "update",
        }
    }

    pub fn persistence_capability(self, args: &[String]) -> CliPersistenceCapability {
        match self {
            Self::Auth => auth_persistence(args),
            Self::Transfer => transfer_persistence(args),
            Self::Doctor => doctor_persistence(args),
            Self::Tools => tools_persistence(args),
            Self::Update => CliPersistenceCapability::ReadOnly,
        }
    }
}

pub const fn interactive_persistence_capability(new_instance: bool) -> CliPersistenceCapability {
    if new_instance {
        CliPersistenceCapability::ReadOnly
    } else {
        CliPersistenceCapability::Writer
    }
}

/// Preserve the CLI's existing lossy `OsString` boundary without exposing non-Unicode arguments
/// to parsers that have always accepted owned UTF-8 strings.
pub fn collect_lossy_cli_args(args: impl IntoIterator<Item = OsString>) -> Vec<String> {
    args.into_iter()
        .map(|arg| arg.to_string_lossy().into_owned())
        .collect()
}

fn auth_persistence(args: &[String]) -> CliPersistenceCapability {
    match args.first().map(String::as_str) {
        Some("lastfm" | "listenbrainz") => CliPersistenceCapability::Writer,
        Some("spotify") => spotify_auth_persistence(&args[1..]),
        _ => CliPersistenceCapability::NotRequired,
    }
}

fn spotify_auth_persistence(args: &[String]) -> CliPersistenceCapability {
    let mut it = args.iter().map(String::as_str);
    while let Some(arg) = it.next() {
        match arg {
            // The command parser consumes the next token even when it looks like a flag.
            "--client-id" if it.next().is_some() => {}
            "--client-id" => return CliPersistenceCapability::NotRequired,
            "--status" | "--logout" => {}
            "--help" | "-h" => return CliPersistenceCapability::NotRequired,
            // Unknown flags are rejected before Config/token access.
            _ => return CliPersistenceCapability::NotRequired,
        }
    }
    // Status can refresh credentials, logout removes them, and the default connect path may save
    // both config and token state. Those valid or otherwise ambiguous forms require the writer.
    CliPersistenceCapability::Writer
}

fn transfer_persistence(args: &[String]) -> CliPersistenceCapability {
    match args.first().map(String::as_str) {
        Some("list" | "jobs" | "sessions" | "session" | "report") => {
            CliPersistenceCapability::ReadOnly
        }
        Some("review") => review_persistence(&args[1..]),
        // This surface only builds and prints a plan; the parser requires `--dry-run` and has no
        // apply path. Keeping even malformed invocations observational also preserves usage exits
        // when another process owns the writer lease.
        Some("download") => CliPersistenceCapability::ReadOnly,
        Some("organize") => organize_persistence(&args[1..]),
        Some("import" | "export" | "backup" | "resume") => CliPersistenceCapability::Writer,
        None | Some("--help" | "-h" | "help") => CliPersistenceCapability::NotRequired,
        Some(_) => CliPersistenceCapability::NotRequired,
    }
}

fn review_persistence(args: &[String]) -> CliPersistenceCapability {
    let Some(_) = args.first() else {
        return CliPersistenceCapability::NotRequired;
    };
    match args.get(1).map(String::as_str) {
        None
        | Some("--all" | "--review" | "--accepted" | "--rejected" | "--skipped" | "--undecided") => {
            CliPersistenceCapability::ReadOnly
        }
        // Known actions mutate, and an unrecognized second token enters the action parser. Keep
        // that ambiguous path fail-closed under the writer lease rather than guessing from flags.
        Some(_) => CliPersistenceCapability::Writer,
    }
}

fn organize_persistence(args: &[String]) -> CliPersistenceCapability {
    let mut dry_run = false;
    let mut apply = false;
    let mut has_session_id = false;
    let mut has_root = false;
    let mut invalid = false;
    let mut it = args.iter().map(String::as_str);
    while let Some(arg) = it.next() {
        match arg {
            "--dry-run" => dry_run = true,
            "--apply" => apply = true,
            "--yes" => {}
            "--root" => {
                if it.next().is_some() {
                    has_root = true;
                } else {
                    invalid = true;
                }
            }
            "--template" => {
                if it.next().is_none() {
                    invalid = true;
                }
            }
            "--help" | "-h" => invalid = true,
            other if other.starts_with('-') => invalid = true,
            _ if has_session_id => invalid = true,
            _ => has_session_id = true,
        }
    }

    if !invalid && has_session_id && has_root && dry_run && !apply {
        CliPersistenceCapability::ReadOnly
    } else {
        // `--apply` is mutating. Malformed or otherwise ambiguous forms retain the conservative
        // writer classification instead of duplicating the command parser's full validation here.
        CliPersistenceCapability::Writer
    }
}

fn tools_persistence(args: &[String]) -> CliPersistenceCapability {
    match args.first().map(String::as_str) {
        None | Some("status") => CliPersistenceCapability::ReadOnly,
        // These two parsers reject the missing-argument form before loading or saving state. Do
        // not let writer-lease contention turn their stable usage exit (2) into an I/O exit (1).
        Some("use" | "reset") if args.len() == 1 => CliPersistenceCapability::NotRequired,
        Some("use" | "unpin" | "update" | "reset" | "diagnose") => CliPersistenceCapability::Writer,
        Some(_) => CliPersistenceCapability::NotRequired,
    }
}

fn doctor_persistence(args: &[String]) -> CliPersistenceCapability {
    if matches!(args, [command, flag] if command == "privacy" && flag == "--cleanup") {
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

#[cfg(test)]
mod tests;
