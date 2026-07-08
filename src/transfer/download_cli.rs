//! CLI surface for import download planning.

use super::cli::{EXIT_FAILED, EXIT_OK, EXIT_USAGE};
use super::download_plan::{
    ImportDownloadDecision, ImportDownloadDedupeIndex, ImportDownloadPlan,
    build_import_download_plan,
};
use super::session::ImportSession;

const USAGE: &str = "\
Usage:
  ytt transfer download <JOB-ID> --accepted --dry-run

This command prints the session-aware download plan. It does not start downloads.";

pub fn run(args: &[&str]) -> i32 {
    match run_inner(args) {
        Ok(message) => {
            print!("{message}");
            EXIT_OK
        }
        Err(DownloadCliError::Usage(message)) => {
            eprintln!("ytt transfer download: {message}");
            eprintln!("{USAGE}");
            EXIT_USAGE
        }
        Err(DownloadCliError::Failed(error)) => {
            eprintln!("ytt transfer download: {error:#}");
            EXIT_FAILED
        }
    }
}

fn run_inner(args: &[&str]) -> Result<String, DownloadCliError> {
    let Some(job_id) = args.first().copied() else {
        return Err(DownloadCliError::Usage("missing <JOB-ID>".to_owned()));
    };
    let mut accepted = false;
    let mut dry_run = false;
    for arg in &args[1..] {
        match *arg {
            "--accepted" => accepted = true,
            "--dry-run" => dry_run = true,
            other => {
                return Err(DownloadCliError::Usage(format!(
                    "unknown download flag `{other}`"
                )));
            }
        }
    }
    if !accepted {
        return Err(DownloadCliError::Usage(
            "download planning currently requires --accepted".to_owned(),
        ));
    }
    if !dry_run {
        return Err(DownloadCliError::Usage(
            "CLI downloads are not started directly yet; use --dry-run to inspect the plan"
                .to_owned(),
        ));
    }

    let session = ImportSession::load(job_id).map_err(DownloadCliError::Failed)?;
    let existing = existing_index();
    let plan = build_import_download_plan(&session, &existing);
    Ok(format_plan(&plan))
}

#[derive(Debug)]
enum DownloadCliError {
    Usage(String),
    Failed(anyhow::Error),
}

fn existing_index() -> ImportDownloadDedupeIndex {
    let existing =
        ImportDownloadDedupeIndex::from_download_store(&crate::downloads::DownloadStore::load());
    #[cfg(not(test))]
    {
        let mut existing = existing;
        if let Some(path) = crate::local::default_index_path() {
            existing.add_local_index(&crate::local::LocalIndex::load(&path));
        }
        existing
    }
    #[cfg(test)]
    {
        existing
    }
}

fn format_plan(plan: &ImportDownloadPlan) -> String {
    let mut out = format!(
        "Download plan: {}\n  enqueue: {}, linked: {}, duplicate: {}, skipped: {}\n",
        plan.session_id,
        plan.enqueue_count,
        plan.linked_existing_count,
        plan.duplicate_count,
        plan.skipped_count
    );
    for row in &plan.rows {
        out.push_str(&format!(
            "  {:>4}. {:<21} {}",
            row.source_order,
            decision_label(&row.decision),
            row.title
        ));
        if let Some(key) = &row.selected_key {
            out.push_str(&format!(" ({key})"));
        }
        out.push('\n');
    }
    out
}

fn decision_label(decision: &ImportDownloadDecision) -> String {
    match decision {
        ImportDownloadDecision::Enqueue => "enqueue".to_owned(),
        ImportDownloadDecision::AlreadyWritten { path }
        | ImportDownloadDecision::AlreadyDownloaded { path } => {
            format!("linked:{}", optional_path(path.as_ref()))
        }
        ImportDownloadDecision::AlreadyInLocalDeck { path } => {
            format!("local:{}", path.display())
        }
        ImportDownloadDecision::DuplicateInSession { first_source_order } => {
            format!("duplicate:row-{first_source_order}")
        }
        ImportDownloadDecision::NotAccepted => "skip:not_accepted".to_owned(),
        ImportDownloadDecision::NoSelectedKey => "skip:no_selected_key".to_owned(),
    }
}

fn optional_path(path: Option<&std::path::PathBuf>) -> String {
    path.map(|path| path.display().to_string())
        .unwrap_or_else(|| "-".to_owned())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::transfer::session::{ImportSessionRow, ImportSessionRowStatus, SessionEndpoint};

    fn save_session(job_id: &str) {
        let session = ImportSession {
            schema_version: 1,
            session_id: job_id.to_owned(),
            job_id: job_id.to_owned(),
            created_at: 0,
            updated_at: 0,
            stage: crate::transfer::Stage::Writing,
            source: SessionEndpoint::default(),
            destination: SessionEndpoint::default(),
            counts: Default::default(),
            rows: vec![ImportSessionRow {
                row_id: "row-00001".to_owned(),
                source_order: 1,
                status: ImportSessionRowStatus::Matched,
                title: "Accepted".to_owned(),
                artists: vec!["Artist".to_owned()],
                source_key: "spotify:track:download-cli".to_owned(),
                selected_key: Some("downloadcli1".to_owned()),
                ..ImportSessionRow::default()
            }],
        };
        session.save().expect("save import session");
    }

    #[test]
    fn dry_run_prints_download_plan() {
        let job_id = "sp2yt-download-cli-plan";
        save_session(job_id);

        let output = run_inner(&[job_id, "--accepted", "--dry-run"]).expect("download dry-run");

        assert!(output.contains("Download plan: sp2yt-download-cli-plan"));
        assert!(output.contains("enqueue: 1"));
        assert!(output.contains("Accepted"));
    }

    #[test]
    fn dry_run_is_required() {
        let err = run_inner(&["sp2yt-download-cli-plan", "--accepted"])
            .expect_err("missing dry-run should be usage");
        match err {
            DownloadCliError::Usage(message) => assert!(message.contains("--dry-run")),
            DownloadCliError::Failed(error) => panic!("unexpected failure: {error:#}"),
        }
    }

    #[test]
    fn accepted_filter_is_required() {
        let err = run_inner(&["sp2yt-download-cli-plan", "--dry-run"])
            .expect_err("missing accepted should be usage");
        match err {
            DownloadCliError::Usage(message) => assert!(message.contains("--accepted")),
            DownloadCliError::Failed(error) => panic!("unexpected failure: {error:#}"),
        }
    }
}
