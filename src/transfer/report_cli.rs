//! Saved transfer report export surface.

use anyhow::{Context, anyhow};

use super::checkpoint::{ReportCandidate, ReportRow, TransferReport, report_path};
use super::cli::{EXIT_FAILED, EXIT_OK, EXIT_USAGE};

const USAGE: &str = "\
Usage:
  ytt transfer report <JOB-ID> [--format text|json]
  ytt transfer report <JOB-ID> --json";

pub fn run(args: &[&str]) -> i32 {
    match run_inner(args) {
        Ok(message) => {
            print!("{message}");
            EXIT_OK
        }
        Err(ReportError::Usage(message)) => {
            eprintln!("ytt transfer report: {message}");
            eprintln!("{USAGE}");
            EXIT_USAGE
        }
        Err(ReportError::Failed(error)) => {
            eprintln!("ytt transfer report: {error:#}");
            EXIT_FAILED
        }
    }
}

fn run_inner(args: &[&str]) -> Result<String, ReportError> {
    let Some(job_id) = args.first().copied() else {
        return Err(ReportError::Usage("missing <JOB-ID>".to_owned()));
    };
    let format = parse_format(&args[1..])?;
    let report = load_report(job_id).map_err(ReportError::Failed)?;
    match format {
        ReportFormat::Text => Ok(format_text_report(&report)),
        ReportFormat::Json => serde_json::to_string_pretty(&report)
            .map(|json| format!("{json}\n"))
            .map_err(|error| ReportError::Failed(error.into())),
    }
}

#[derive(Debug)]
enum ReportError {
    Usage(String),
    Failed(anyhow::Error),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ReportFormat {
    Text,
    Json,
}

fn parse_format(args: &[&str]) -> Result<ReportFormat, ReportError> {
    if args.is_empty() {
        return Ok(ReportFormat::Text);
    }
    match args {
        [] => Ok(ReportFormat::Text),
        ["--json"] => Ok(ReportFormat::Json),
        ["--format", "text"] => Ok(ReportFormat::Text),
        ["--format", "json"] => Ok(ReportFormat::Json),
        ["--format"] => Err(ReportError::Usage("--format needs text or json".to_owned())),
        [flag, ..] => Err(ReportError::Usage(format!("unknown report flag `{flag}`"))),
    }
}

fn load_report(job_id: &str) -> anyhow::Result<TransferReport> {
    let path = report_path(job_id).ok_or_else(|| anyhow!("bad job id `{job_id}`"))?;
    let text = crate::util::safe_fs::read_to_string_no_symlink(&path)
        .with_context(|| format!("no saved report for job `{job_id}`"))?;
    serde_json::from_str(&text).context("saved report is corrupt")
}

fn format_text_report(report: &TransferReport) -> String {
    let mut out = format!("Report: {}\n{}\n", report.job_id, report.render_text());
    append_rows(&mut out, "Ambiguous", &report.ambiguous);
    append_rows(&mut out, "Not found", &report.not_found);
    out
}

fn append_rows(out: &mut String, label: &str, rows: &[ReportRow]) {
    if rows.is_empty() {
        return;
    }
    out.push_str(label);
    out.push_str(":\n");
    for row in rows {
        out.push_str(&format!(
            "  {:>4}. {} - {}",
            row.source_order.unwrap_or_default(),
            row.artists,
            row.title
        ));
        if !row.note.is_empty() {
            out.push_str(&format!(" ({})", row.note));
        }
        out.push('\n');
        if let Some(album) = &row.album {
            out.push_str(&format!("        album: {album}\n"));
        }
        if let Some(isrc) = &row.isrc {
            out.push_str(&format!("        isrc: {isrc}\n"));
        }
        if let Some(source) = &row.source_key {
            out.push_str(&format!("        source: {source}\n"));
        }
        if let Some(kind) = &row.source_kind {
            out.push_str(&format!("        source kind: {kind}\n"));
        }
        if let Some(tier) = &row.quality_tier {
            out.push_str(&format!("        quality: {tier}\n"));
        }
        if let Some(delta) = row.duration_delta_secs {
            out.push_str(&format!("        duration delta: {delta:+}s\n"));
        }
        if let Some(reason) = &row.reject_reason {
            out.push_str(&format!("        blocked: {reason}\n"));
        } else if !row.reason_codes.is_empty() {
            out.push_str(&format!(
                "        reasons: {}\n",
                row.reason_codes.join(", ")
            ));
        }
        for (idx, candidate) in row.candidates.iter().enumerate() {
            out.push_str(&format_report_candidate(idx + 1, candidate));
        }
    }
}

fn format_report_candidate(index: usize, candidate: &ReportCandidate) -> String {
    format!(
        "        candidate {index}: {:.2} {} ({})\n",
        candidate.score, candidate.display, candidate.key
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample_report(job_id: &str) -> TransferReport {
        TransferReport {
            job_id: job_id.to_owned(),
            total: 2,
            matched: 1,
            written: 1,
            ambiguous: vec![ReportRow {
                title: "Maybe".to_owned(),
                artists: "Artist".to_owned(),
                note: "top candidates are close".to_owned(),
                source_order: Some(1),
                source_key: Some("spotify:track:maybe".to_owned()),
                album: Some("Album".to_owned()),
                isrc: Some("USRC17607839".to_owned()),
                candidates: vec![ReportCandidate {
                    key: "vid1".to_owned(),
                    score: 0.78,
                    display: "Artist - Maybe".to_owned(),
                    score_breakdown: None,
                }],
                ..ReportRow::default()
            }],
            not_found: vec![ReportRow {
                title: "Missing".to_owned(),
                artists: "Artist".to_owned(),
                note: "no match on the destination".to_owned(),
                source_order: Some(2),
                ..ReportRow::default()
            }],
            ..TransferReport::default()
        }
    }

    #[test]
    fn text_report_prints_saved_attention_rows() {
        let job_id = "sp2yt-report-text";
        sample_report(job_id).save().expect("save report");

        let output = run_inner(&[job_id]).expect("text report");

        for expected in [
            "Report: sp2yt-report-text",
            "1/2 matched",
            "Ambiguous:",
            "1. Artist - Maybe (top candidates are close)",
            "album: Album",
            "source: spotify:track:maybe",
            "candidate 1: 0.78 Artist - Maybe (vid1)",
            "Not found:",
            "2. Artist - Missing (no match on the destination)",
        ] {
            assert!(
                output.contains(expected),
                "missing {expected:?} in {output}"
            );
        }
    }

    #[test]
    fn json_report_prints_saved_report() {
        let job_id = "sp2yt-report-json";
        sample_report(job_id).save().expect("save report");

        let output = run_inner(&[job_id, "--format", "json"]).expect("json report");

        assert!(output.contains(r#""job_id": "sp2yt-report-json""#));
        assert!(output.contains(r#""schema_version": 4"#));
        assert!(output.contains(r#""source_key": "spotify:track:maybe""#));
    }

    #[test]
    fn parse_rejects_bad_flags() {
        assert!(matches!(parse_format(&[]).unwrap(), ReportFormat::Text));
        assert!(matches!(
            parse_format(&["--json"]).unwrap(),
            ReportFormat::Json
        ));
        assert!(matches!(
            parse_format(&["--format", "json"]).unwrap(),
            ReportFormat::Json
        ));
        assert!(matches!(
            parse_format(&["--format", "text"]).unwrap(),
            ReportFormat::Text
        ));
        assert!(matches!(
            parse_format(&["--format"]).unwrap_err(),
            ReportError::Usage(_)
        ));
        assert!(matches!(
            parse_format(&["--bad"]).unwrap_err(),
            ReportError::Usage(_)
        ));
    }
}
