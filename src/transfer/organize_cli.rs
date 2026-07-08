//! `ytt transfer organize` dry-run surface for import-session move previews.

use std::path::PathBuf;

use super::cli::{EXIT_FAILED, EXIT_OK, EXIT_USAGE};
use super::organize_plan::{
    DEFAULT_IMPORT_ORGANIZE_TEMPLATE, ImportOrganizeDecision, ImportOrganizeOptions,
    build_import_organize_plan,
};
use super::session::ImportSession;

const USAGE: &str = "\
Usage:
  ytt transfer organize <JOB-ID> --root DIR --dry-run [--template TEMPLATE]

Preview where downloaded import-session files would move before committing them.";

pub fn run(args: &[&str]) -> i32 {
    match run_inner(args) {
        Ok(()) => EXIT_OK,
        Err(OrganizeCliError::Usage(message)) => {
            eprintln!("ytt transfer organize: {message}");
            eprintln!("{USAGE}");
            EXIT_USAGE
        }
        Err(OrganizeCliError::Other(error)) => {
            eprintln!("ytt transfer organize: {error:#}");
            EXIT_FAILED
        }
    }
}

fn run_inner(args: &[&str]) -> Result<(), OrganizeCliError> {
    let parsed = parse_args(args)?;
    let session = ImportSession::load(&parsed.session_id).map_err(OrganizeCliError::Other)?;
    let plan = build_import_organize_plan(
        &session,
        &ImportOrganizeOptions {
            root: parsed.root,
            template: parsed.template,
        },
    )
    .map_err(OrganizeCliError::Other)?;

    println!("Organize preview: {}", plan.session_id);
    println!("Root: {}", plan.root.display());
    println!("Template: {}", plan.template);
    println!(
        "Rows: {} move, {} already, {} skipped",
        plan.move_count, plan.already_count, plan.skipped_count
    );
    for row in &plan.rows {
        match row.decision {
            ImportOrganizeDecision::Move => {
                println!(
                    "  #{:<4} MOVE    {} -> {}",
                    row.source_order,
                    display_path(row.current_path.as_ref()),
                    display_path(row.target_path.as_ref())
                );
            }
            ImportOrganizeDecision::AlreadyAtTarget => {
                println!(
                    "  #{:<4} READY   {}",
                    row.source_order,
                    display_path(row.current_path.as_ref())
                );
            }
            ImportOrganizeDecision::NotAccepted => {
                println!("  #{:<4} SKIP    not accepted", row.source_order);
            }
            ImportOrganizeDecision::MissingLocalPath => {
                println!("  #{:<4} SKIP    no local file", row.source_order);
            }
        }
        for warning in &row.warnings {
            println!("        warning: {warning}");
        }
    }
    Ok(())
}

#[derive(Debug)]
struct OrganizeArgs {
    session_id: String,
    root: PathBuf,
    template: String,
}

fn parse_args(args: &[&str]) -> Result<OrganizeArgs, OrganizeCliError> {
    let mut session_id = None;
    let mut root = None;
    let mut template = DEFAULT_IMPORT_ORGANIZE_TEMPLATE.to_owned();
    let mut dry_run = false;
    let mut it = args.iter().copied();
    while let Some(arg) = it.next() {
        match arg {
            "--dry-run" => dry_run = true,
            "--root" => {
                let value = it
                    .next()
                    .ok_or_else(|| OrganizeCliError::Usage("--root needs a value".to_owned()))?;
                root = Some(PathBuf::from(value));
            }
            "--template" => {
                template = it
                    .next()
                    .ok_or_else(|| OrganizeCliError::Usage("--template needs a value".to_owned()))?
                    .to_owned();
            }
            "--help" | "-h" => return Err(OrganizeCliError::Usage("help requested".to_owned())),
            other if other.starts_with('-') => {
                return Err(OrganizeCliError::Usage(format!("unknown flag `{other}`")));
            }
            other => {
                if session_id.replace(other.to_owned()).is_some() {
                    return Err(OrganizeCliError::Usage(
                        "too many organize arguments".to_owned(),
                    ));
                }
            }
        }
    }
    if !dry_run {
        return Err(OrganizeCliError::Usage(
            "organize currently requires --dry-run".to_owned(),
        ));
    }
    let session_id =
        session_id.ok_or_else(|| OrganizeCliError::Usage("missing <JOB-ID>".to_owned()))?;
    let root = root.ok_or_else(|| OrganizeCliError::Usage("missing --root DIR".to_owned()))?;
    Ok(OrganizeArgs {
        session_id,
        root,
        template,
    })
}

fn display_path(path: Option<&PathBuf>) -> String {
    path.map(|path| path.display().to_string())
        .unwrap_or_else(|| "-".to_owned())
}

#[derive(Debug)]
enum OrganizeCliError {
    Usage(String),
    Other(anyhow::Error),
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_requires_dry_run_and_root() {
        let err = parse_args(&["sp2yt-1", "--root", "/tmp/library"])
            .expect_err("missing dry-run should be usage");
        match err {
            OrganizeCliError::Usage(message) => assert!(message.contains("--dry-run")),
            OrganizeCliError::Other(error) => panic!("unexpected error: {error:#}"),
        }

        let err = parse_args(&["sp2yt-1", "--dry-run"]).expect_err("missing root should be usage");
        match err {
            OrganizeCliError::Usage(message) => assert!(message.contains("--root")),
            OrganizeCliError::Other(error) => panic!("unexpected error: {error:#}"),
        }
    }

    #[test]
    fn parse_accepts_custom_template() {
        let parsed = parse_args(&[
            "sp2yt-1",
            "--root",
            "/tmp/library",
            "--dry-run",
            "--template",
            "{artist}/{title}",
        ])
        .unwrap();

        assert_eq!(parsed.session_id, "sp2yt-1");
        assert_eq!(parsed.root, PathBuf::from("/tmp/library"));
        assert_eq!(parsed.template, "{artist}/{title}");
    }
}
