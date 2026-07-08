//! `ytt transfer organize` dry-run surface for import-session move previews.

use std::path::PathBuf;

use super::cli::{EXIT_FAILED, EXIT_OK, EXIT_USAGE};
use super::organize_plan::{
    ImportOrganizeDecision, ImportOrganizeOptions, apply_import_organize_plan,
    build_import_organize_plan,
};
use super::session::ImportSession;
use crate::config::Config;

const USAGE: &str = "\
Usage:
  ytt transfer organize <JOB-ID> --root DIR --dry-run [--template TEMPLATE]
  ytt transfer organize <JOB-ID> --root DIR --apply --yes [--template TEMPLATE]

Preview or apply where downloaded import-session files move before committing them.";

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
    let template = parsed
        .template
        .unwrap_or_else(|| Config::load().local.import_path_template().to_owned());
    let plan = build_import_organize_plan(
        &session,
        &ImportOrganizeOptions {
            root: parsed.root,
            template,
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
    if parsed.apply {
        let report = apply_import_organize_plan(&plan).map_err(OrganizeCliError::Other)?;
        println!(
            "Applied: {} moved, {} already, {} skipped",
            report.moved_count, report.already_count, report.skipped_count
        );
    }
    Ok(())
}

#[derive(Debug)]
struct OrganizeArgs {
    session_id: String,
    root: PathBuf,
    template: Option<String>,
    apply: bool,
}

fn parse_args(args: &[&str]) -> Result<OrganizeArgs, OrganizeCliError> {
    let mut session_id = None;
    let mut root = None;
    let mut template = None;
    let mut dry_run = false;
    let mut apply = false;
    let mut yes = false;
    let mut it = args.iter().copied();
    while let Some(arg) = it.next() {
        match arg {
            "--dry-run" => dry_run = true,
            "--apply" => apply = true,
            "--yes" => yes = true,
            "--root" => {
                let value = it
                    .next()
                    .ok_or_else(|| OrganizeCliError::Usage("--root needs a value".to_owned()))?;
                root = Some(PathBuf::from(value));
            }
            "--template" => {
                template = Some(
                    it.next()
                        .ok_or_else(|| {
                            OrganizeCliError::Usage("--template needs a value".to_owned())
                        })?
                        .to_owned(),
                );
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
    match (dry_run, apply) {
        (true, false) | (false, true) => {}
        (false, false) => {
            return Err(OrganizeCliError::Usage(
                "organize requires --dry-run or --apply".to_owned(),
            ));
        }
        (true, true) => {
            return Err(OrganizeCliError::Usage(
                "choose only one of --dry-run or --apply".to_owned(),
            ));
        }
    }
    if apply && !yes {
        return Err(OrganizeCliError::Usage("--apply requires --yes".to_owned()));
    }
    let session_id =
        session_id.ok_or_else(|| OrganizeCliError::Usage("missing <JOB-ID>".to_owned()))?;
    let root = root.ok_or_else(|| OrganizeCliError::Usage("missing --root DIR".to_owned()))?;
    Ok(OrganizeArgs {
        session_id,
        root,
        template,
        apply,
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
            .expect_err("missing mode should be usage");
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
        assert_eq!(parsed.template.as_deref(), Some("{artist}/{title}"));
        assert!(!parsed.apply);
    }

    #[test]
    fn parse_apply_requires_yes() {
        let err = parse_args(&["sp2yt-1", "--root", "/tmp/library", "--apply"])
            .expect_err("apply without yes should be usage");
        match err {
            OrganizeCliError::Usage(message) => assert!(message.contains("--yes")),
            OrganizeCliError::Other(error) => panic!("unexpected error: {error:#}"),
        }

        let parsed =
            parse_args(&["sp2yt-1", "--root", "/tmp/library", "--apply", "--yes"]).unwrap();

        assert!(parsed.apply);
    }
}
