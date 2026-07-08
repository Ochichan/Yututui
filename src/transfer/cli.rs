//! `ytt transfer …` and `ytt auth spotify` — the terminal surface of the transfer
//! engine. Follows the `daemon::run_cli` pattern: hand-parsed args, a current-thread
//! runtime per invocation, plain exit codes.
//!
//! The pre-write confirmation gate reuses the dry-run→resume machinery: without
//! `--yes`, the job first runs `dry_run` (fetch + match + checkpoint), prints the match
//! summary, asks, and on "y" resumes the same checkpoint to perform the writes — so the
//! confirmation costs zero duplicate matching.

use std::io::Write as _;
use std::path::PathBuf;
use std::time::Duration;

use super::checkpoint::{Checkpoint, ReviewDecision, TransferReport};
use super::session::{ImportSession, ImportSessionRowStatus};
use super::{
    FileFormat, JobCtx, JobError, JobSpec, Stage, TransferDest, TransferProgress, TransferSource,
    new_job_id, run_job,
};
use crate::config::Config;
use crate::spotify::auth::{self, SpotifyToken};
use crate::spotify::client::{SpotifyClient, SpotifyError};

pub const EXIT_OK: i32 = 0;
pub const EXIT_FAILED: i32 = 1;
pub const EXIT_USAGE: i32 = 2;
/// Aborted but resumable (rate-limit budget, expired YTM cookie mid-job).
pub const EXIT_RESUMABLE: i32 = 3;

const USAGE: &str = "\
Usage: ytt transfer <command>

Commands:
  list spotify | list ytm          Playlists on either side (id, tracks, name)
  jobs                             Known transfer jobs and their state
  sessions                         Import sessions for review/download follow-up
  session <JOB-ID>                 Show one import session's review rows
  report <JOB-ID> [--format text|json]
                                   Print a saved transfer report
  review <JOB-ID> [filter]         Show or update import review decisions
  download <JOB-ID> --accepted --dry-run
                                   Preview session-aware download decisions
  organize <JOB-ID> --root DIR --dry-run|--apply --yes [--template TEMPLATE]
                                   Preview/apply Local Deck library move paths
  import <SOURCE> [flags]          Spotify/file → YouTube Music
      SOURCE: a Spotify playlist URL/URI/id, the word `liked`, or a .json/.csv file
      --to-playlist NAME           Append to (or create) this YTM account playlist
      --to likes                   Like each track instead of building a playlist
      --to local[:NAME]            Into the TUI's own Library playlists (playable in-app)
      --dry-run                    Fetch + match only; `resume` performs the writes
      --yes                        Skip the confirmation gate
      --min-score 0.80             Match accept threshold (0..1)
      --take-best                  Accept the best ambiguous candidate too
      --rematch                    Ignore cached matches / file fast-path ids
  export <ytm:ID|local:KEY> --to spotify [--name NAME] [--dry-run] [--yes] [--min-score X]
  export <ytm:ID|local:KEY> --to <FILE.json|FILE.csv>
  backup --dir DIR [--csv]         Every YTM playlist → one JSON (+CSV) file each
  resume <JOB-ID> [--yes]          Continue a checkpointed job

Run `ytt auth spotify` first for anything touching Spotify.";

fn runtime() -> Option<tokio::runtime::Runtime> {
    match tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
    {
        Ok(rt) => Some(rt),
        Err(e) => {
            eprintln!("ytt transfer: could not start runtime: {e}");
            None
        }
    }
}

pub fn run(args: &[String]) -> i32 {
    let mut it = args.iter().map(String::as_str).peekable();
    match it.next() {
        Some("list") => match it.next() {
            Some("spotify") => runtime().map_or(EXIT_FAILED, |rt| rt.block_on(list_spotify())),
            Some("ytm") => runtime().map_or(EXIT_FAILED, |rt| rt.block_on(list_ytm())),
            _ => {
                eprintln!("ytt transfer list: expected `spotify` or `ytm`");
                EXIT_USAGE
            }
        },
        Some("jobs") => jobs(),
        Some("sessions") => sessions(),
        Some("session") => match it.next() {
            Some(job_id) => show_session(job_id),
            None => {
                eprintln!("ytt transfer session: missing <JOB-ID> (see `ytt transfer sessions`)");
                EXIT_USAGE
            }
        },
        Some("review") => {
            let rest: Vec<&str> = it.collect();
            super::review_cli::run(&rest)
        }
        Some("report") => {
            let rest: Vec<&str> = it.collect();
            super::report_cli::run(&rest)
        }
        Some("download") => {
            let rest: Vec<&str> = it.collect();
            super::download_cli::run(&rest)
        }
        Some("organize") => {
            let rest: Vec<&str> = it.collect();
            super::organize_cli::run(&rest)
        }
        Some("import") => {
            let rest: Vec<&str> = it.collect();
            match parse_import(&rest) {
                Ok((spec, yes)) => runtime().map_or(EXIT_FAILED, |rt| {
                    rt.block_on(execute_new(spec, yes, "sp2yt"))
                }),
                Err(msg) => {
                    eprintln!("ytt transfer import: {msg}");
                    EXIT_USAGE
                }
            }
        }
        Some("export") => {
            let rest: Vec<&str> = it.collect();
            match parse_export(&rest) {
                Ok((spec, yes)) => runtime().map_or(EXIT_FAILED, |rt| {
                    rt.block_on(execute_new(spec, yes, "yt2sp"))
                }),
                Err(msg) => {
                    eprintln!("ytt transfer export: {msg}");
                    EXIT_USAGE
                }
            }
        }
        Some("backup") => {
            let rest: Vec<&str> = it.collect();
            match parse_backup(&rest) {
                Ok((dir, csv)) => runtime().map_or(EXIT_FAILED, |rt| rt.block_on(backup(dir, csv))),
                Err(msg) => {
                    eprintln!("ytt transfer backup: {msg}");
                    EXIT_USAGE
                }
            }
        }
        Some("resume") => {
            let job_id = it.next().map(str::to_owned);
            let yes = it.any(|a| a == "--yes");
            match job_id {
                Some(job_id) => {
                    runtime().map_or(EXIT_FAILED, |rt| rt.block_on(resume(job_id, yes)))
                }
                None => {
                    eprintln!("ytt transfer resume: missing <JOB-ID> (see `ytt transfer jobs`)");
                    EXIT_USAGE
                }
            }
        }
        Some("--help" | "-h") | None => {
            println!("{USAGE}");
            if args.is_empty() { EXIT_USAGE } else { EXIT_OK }
        }
        Some(other) => {
            eprintln!("ytt transfer: unknown command `{other}`");
            eprintln!("{USAGE}");
            EXIT_USAGE
        }
    }
}

// Parsing ---------------------------------------------------------------------------

struct CommonFlags {
    dry_run: bool,
    yes: bool,
    min_score: f32,
    take_best: bool,
    rematch: bool,
}

fn parse_common(args: &mut Vec<&str>) -> Result<CommonFlags, String> {
    let mut flags = CommonFlags {
        dry_run: false,
        yes: false,
        min_score: 0.80,
        take_best: false,
        rematch: false,
    };
    let mut keep = Vec::new();
    let mut it = args.iter().copied().peekable();
    while let Some(arg) = it.next() {
        match arg {
            "--dry-run" => flags.dry_run = true,
            "--yes" => flags.yes = true,
            "--take-best" => flags.take_best = true,
            "--rematch" => flags.rematch = true,
            "--min-score" => {
                let v = it.next().ok_or("--min-score needs a value")?;
                let score: f32 = v.parse().map_err(|_| format!("bad --min-score `{v}`"))?;
                if !(0.0..=1.0).contains(&score) {
                    return Err(format!("--min-score must be 0..1 (got {score})"));
                }
                flags.min_score = score;
            }
            other => keep.push(other),
        }
    }
    *args = keep;
    Ok(flags)
}

/// A Spotify playlist reference in any of the shapes people paste.
fn parse_spotify_playlist_id(s: &str) -> Option<String> {
    if let Some(rest) = s.strip_prefix("spotify:playlist:") {
        return Some(rest.to_owned());
    }
    if let Some(idx) = s.find("open.spotify.com/playlist/") {
        let rest = &s[idx + "open.spotify.com/playlist/".len()..];
        let id: String = rest
            .chars()
            .take_while(|c| c.is_ascii_alphanumeric())
            .collect();
        return (!id.is_empty()).then_some(id);
    }
    (s.len() == 22 && s.bytes().all(|b| b.is_ascii_alphanumeric())).then(|| s.to_owned())
}

fn parse_import(args: &[&str]) -> Result<(JobSpec, bool), String> {
    let mut args: Vec<&str> = args.to_vec();
    let flags = parse_common(&mut args)?;
    let mut source_arg = None;
    let mut dest = None;
    let mut it = args.iter().copied().peekable();
    while let Some(arg) = it.next() {
        match arg {
            "--to-playlist" => {
                let name = it.next().ok_or("--to-playlist needs a name")?;
                dest = Some(TransferDest::YtmExistingPlaylist {
                    name: name.to_owned(),
                });
            }
            "--to" => match it.next() {
                Some("likes") => dest = Some(TransferDest::YtmLikes),
                Some("local") => dest = Some(TransferDest::LocalPlaylist { name: None }),
                Some(other) if other.starts_with("local:") => {
                    dest = Some(TransferDest::LocalPlaylist {
                        name: Some(other["local:".len()..].to_owned())
                            .filter(|n| !n.trim().is_empty()),
                    });
                }
                other => {
                    return Err(format!(
                        "--to expects `likes`, `local`, or `local:NAME` (got {other:?})"
                    ));
                }
            },
            other if source_arg.is_none() => source_arg = Some(other.to_owned()),
            other => return Err(format!("unexpected argument `{other}`")),
        }
    }
    let source_arg = source_arg.ok_or("missing source (Spotify URL/id, `liked`, or a file)")?;
    // Deterministic source resolution: existing file → file import; `liked`; else it
    // must parse as a Spotify playlist reference.
    let source = if std::path::Path::new(&source_arg).is_file() {
        TransferSource::File {
            path: PathBuf::from(&source_arg),
        }
    } else if source_arg.eq_ignore_ascii_case("liked") {
        TransferSource::SpotifyLiked
    } else if let Some(id) = parse_spotify_playlist_id(&source_arg) {
        TransferSource::SpotifyPlaylist { id }
    } else {
        return Err(format!(
            "`{source_arg}` is not an existing file, `liked`, or a Spotify playlist URL/URI/id"
        ));
    };
    let dest = dest.unwrap_or(TransferDest::YtmNewPlaylist { name: None });
    Ok((
        JobSpec {
            source,
            dest,
            dry_run: flags.dry_run,
            min_score: flags.min_score,
            take_best: flags.take_best,
            rematch: flags.rematch,
        },
        flags.yes,
    ))
}

fn parse_ytm_source(s: &str) -> Result<TransferSource, String> {
    if let Some(id) = s.strip_prefix("ytm:") {
        Ok(TransferSource::YtmPlaylist { id: id.to_owned() })
    } else if let Some(key) = s.strip_prefix("local:") {
        Ok(TransferSource::LocalPlaylist {
            key: key.to_owned(),
        })
    } else {
        Err(format!(
            "`{s}` — expected `ytm:<playlist-id>` (see `ytt transfer list ytm`) or `local:<name>`"
        ))
    }
}

fn parse_export(args: &[&str]) -> Result<(JobSpec, bool), String> {
    let mut args: Vec<&str> = args.to_vec();
    let flags = parse_common(&mut args)?;
    let mut source = None;
    let mut dest = None;
    let mut name = None;
    let mut it = args.iter().copied().peekable();
    while let Some(arg) = it.next() {
        match arg {
            "--to" => match it.next() {
                Some("spotify") => dest = Some(None),
                Some(path) => dest = Some(Some(PathBuf::from(path))),
                None => return Err("--to needs `spotify` or a file path".to_owned()),
            },
            "--name" => name = Some(it.next().ok_or("--name needs a value")?.to_owned()),
            other if source.is_none() => source = Some(parse_ytm_source(other)?),
            other => return Err(format!("unexpected argument `{other}`")),
        }
    }
    let source = source.ok_or("missing source (`ytm:<id>` or `local:<key>`)")?;
    let dest = match dest.ok_or("missing --to (spotify | FILE.json | FILE.csv)")? {
        None => TransferDest::SpotifyNewPlaylist { name },
        Some(path) => {
            let format = match path
                .extension()
                .and_then(|e| e.to_str())
                .map(str::to_ascii_lowercase)
                .as_deref()
            {
                Some("json") => FileFormat::Json,
                Some("csv") => FileFormat::Csv,
                other => {
                    return Err(format!(
                        "--to file must end in .json or .csv (got {other:?})"
                    ));
                }
            };
            TransferDest::File { path, format }
        }
    };
    Ok((
        JobSpec {
            source,
            dest,
            dry_run: flags.dry_run,
            min_score: flags.min_score,
            take_best: flags.take_best,
            rematch: flags.rematch,
        },
        flags.yes,
    ))
}

fn parse_backup(args: &[&str]) -> Result<(PathBuf, bool), String> {
    let mut dir = None;
    let mut csv = false;
    let mut it = args.iter().copied();
    while let Some(arg) = it.next() {
        match arg {
            "--dir" => dir = Some(PathBuf::from(it.next().ok_or("--dir needs a path")?)),
            "--csv" => csv = true,
            other => return Err(format!("unexpected argument `{other}`")),
        }
    }
    Ok((dir.ok_or("missing --dir DIR")?, csv))
}

// Execution ---------------------------------------------------------------------------

/// Build the clients the spec needs (and only those), with setup hints on failure.
async fn build_ctx(spec: &JobSpec, cfg: &Config) -> Result<JobCtx, String> {
    let needs_spotify = matches!(
        spec.source,
        TransferSource::SpotifyPlaylist { .. } | TransferSource::SpotifyLiked
    ) || matches!(spec.dest, TransferDest::SpotifyNewPlaylist { .. });
    // LocalPlaylist writes locally but still *matches* against YouTube Music.
    let needs_ytm = matches!(spec.source, TransferSource::YtmPlaylist { .. })
        || matches!(
            spec.dest,
            TransferDest::YtmNewPlaylist { .. }
                | TransferDest::YtmExistingPlaylist { .. }
                | TransferDest::YtmLikes
                | TransferDest::LocalPlaylist { .. }
        );
    let spotify = if needs_spotify {
        Some(
            SpotifyClient::from_saved(cfg.spotify.client_id.as_deref())
                .map_err(|e| e.to_string())?,
        )
    } else {
        None
    };
    let ytm = if needs_ytm {
        match cfg.effective_cookie() {
            Some(cookie) => Some(
                crate::api::ytmusic::YtMusicApi::from_cookie(&cookie)
                    .await
                    .map_err(|e| {
                        crate::util::sanitize::sanitize_error_text(format!(
                            "YouTube Music auth failed: {e:#}"
                        ))
                    })?,
            ),
            None => {
                let account_op = matches!(spec.source, TransferSource::YtmPlaylist { .. })
                    || matches!(
                        spec.dest,
                        TransferDest::YtmNewPlaylist { .. }
                            | TransferDest::YtmExistingPlaylist { .. }
                            | TransferDest::YtmLikes
                    );
                if account_op {
                    return Err(
                        "this needs a YouTube Music cookie — add cookies.txt (or `cookie`) in Settings › General"
                            .to_owned(),
                    );
                }
                Some(crate::api::ytmusic::YtMusicApi::Anonymous)
            }
        }
    } else {
        None
    };
    Ok(JobCtx {
        ytm,
        spotify,
        search_config: cfg.effective_search(),
        market: cfg.spotify.market.clone(),
    })
}

/// New job: honor the confirmation gate by running dry first unless `--yes`/`--dry-run`.
async fn execute_new(spec: JobSpec, yes: bool, kind: &str) -> i32 {
    let cfg = Config::load();
    let job_id = new_job_id(kind);
    let gate = !yes && !spec.dry_run && !matches!(spec.dest, TransferDest::File { .. });

    let first_spec = JobSpec {
        dry_run: spec.dry_run || gate,
        ..spec.clone()
    };
    let mut ctx = match build_ctx(&first_spec, &cfg).await {
        Ok(ctx) => ctx,
        Err(msg) => {
            eprintln!("ytt transfer: {msg}");
            return EXIT_FAILED;
        }
    };
    let report = match run_job(
        job_id.clone(),
        first_spec,
        None,
        &mut ctx,
        &mut print_progress,
    )
    .await
    {
        Ok(report) => report,
        Err(e) => return print_job_error(&job_id, e),
    };
    finish_progress_line();

    if spec.dry_run {
        println!("{}", report.render_text());
        print_report_details(&report);
        println!("Matches are checkpointed — `ytt transfer resume {job_id}` performs the writes.");
        return EXIT_OK;
    }
    if gate {
        println!("{}", report.render_text());
        print_report_details(&report);
        if !confirm(&format!(
            "Write {} tracks to the destination? [y/N] ",
            report.matched
        )) {
            println!(
                "Aborted — nothing was written. Resume later with `ytt transfer resume {job_id}`."
            );
            return EXIT_OK;
        }
        // Perform the writes by resuming the checkpoint we just made.
        let cp = match Checkpoint::load(&job_id) {
            Ok(cp) => cp,
            Err(e) => {
                eprintln!("ytt transfer: {e:#}");
                return EXIT_FAILED;
            }
        };
        return finish_resumed(job_id, cp, &cfg).await;
    }
    conclude(&report)
}

async fn resume(job_id: String, _yes: bool) -> i32 {
    let cfg = Config::load();
    let cp = match Checkpoint::load(&job_id) {
        Ok(cp) => cp,
        Err(e) => {
            eprintln!("ytt transfer: {e:#}");
            return EXIT_FAILED;
        }
    };
    if cp.stage == Stage::Done {
        println!("Job {job_id} already finished.");
        return EXIT_OK;
    }
    finish_resumed(job_id, cp, &cfg).await
}

async fn finish_resumed(job_id: String, cp: Checkpoint, cfg: &Config) -> i32 {
    // A resumed dry-run means "now do the writes".
    let spec = JobSpec {
        dry_run: false,
        rematch: false,
        ..cp.spec.clone()
    };
    let mut ctx = match build_ctx(&spec, cfg).await {
        Ok(ctx) => ctx,
        Err(msg) => {
            eprintln!("ytt transfer: {msg}");
            return EXIT_FAILED;
        }
    };
    match run_job(
        job_id.clone(),
        spec,
        Some(cp),
        &mut ctx,
        &mut print_progress,
    )
    .await
    {
        Ok(report) => {
            finish_progress_line();
            conclude(&report)
        }
        Err(e) => print_job_error(&job_id, e),
    }
}

fn conclude(report: &TransferReport) -> i32 {
    println!("{}", report.render_text());
    print_report_details(report);
    if let Some(path) = super::checkpoint::report_path(&report.job_id) {
        println!("Report: {}", path.display());
    }
    print_next_steps(report);
    EXIT_OK
}

fn print_next_steps(report: &TransferReport) {
    let session = ImportSession::load(&report.job_id).ok();
    for line in next_step_lines(report, session.as_ref()) {
        println!("{line}");
    }
}

fn next_step_lines(report: &TransferReport, session: Option<&ImportSession>) -> Vec<String> {
    if report.job_id.is_empty() {
        return Vec::new();
    }
    let mut lines = vec![format!(
        "Session review: ytt transfer session {}",
        report.job_id
    )];
    if session.is_some_and(|session| session.destination.kind == "local_playlist") {
        let dest = session
            .map(|session| session.destination.display())
            .unwrap_or_else(|| "the imported playlist".to_owned());
        lines.push(format!(
            "Next: open Library > Playlists > {dest}, press Shift+D to download accepted tracks, rescan Local Deck, then open Import Sessions."
        ));
        lines.push(format!(
            "Preview downloads: ytt transfer download {} --accepted --dry-run",
            report.job_id
        ));
        lines.push(format!(
            "Preview organize: ytt transfer organize {} --root <LOCAL_ROOT> --dry-run",
            report.job_id
        ));
    }
    lines
}

fn print_job_error(job_id: &str, e: JobError) -> i32 {
    finish_progress_line();
    eprintln!("ytt transfer: {:#}", e.error);
    if e.resumable {
        eprintln!("The job is checkpointed — continue with: ytt transfer resume {job_id}");
        EXIT_RESUMABLE
    } else {
        EXIT_FAILED
    }
}

fn print_report_details(report: &TransferReport) {
    for row in &report.ambiguous {
        println!(
            "  ambiguous: {} — {}  →  {}",
            row.artists, row.title, row.note
        );
    }
    for row in &report.not_found {
        println!("  not found: {} — {}", row.artists, row.title);
    }
}

fn print_progress(p: TransferProgress) {
    let counts = if p.stage == Stage::Matching {
        format!(
            " ({} ok, {} ambiguous, {} miss)",
            p.matched, p.ambiguous, p.not_found
        )
    } else {
        String::new()
    };
    eprint!(
        "\r\x1b[2K[{}] {} {}/{}{}  {}",
        p.job_id,
        p.stage.label(),
        p.done,
        p.total,
        counts,
        p.current
    );
    let _ = std::io::stderr().flush();
}

fn finish_progress_line() {
    eprint!("\r\x1b[2K");
    let _ = std::io::stderr().flush();
}

fn confirm(prompt: &str) -> bool {
    print!("{prompt}");
    let _ = std::io::stdout().flush();
    let mut line = String::new();
    if std::io::stdin().read_line(&mut line).is_err() {
        return false;
    }
    matches!(line.trim().to_ascii_lowercase().as_str(), "y" | "yes")
}

// Simple commands ------------------------------------------------------------------

fn jobs() -> i32 {
    let jobs = Checkpoint::list_all();
    if jobs.is_empty() {
        println!("No transfer jobs.");
        return EXIT_OK;
    }
    println!("{:<28} {:<10} UPDATED", "JOB", "STAGE");
    for (job_id, stage, updated) in jobs {
        println!("{job_id:<28} {:<10} {updated}", stage.label());
    }
    EXIT_OK
}

fn sessions() -> i32 {
    let sessions = ImportSession::list_all();
    if sessions.is_empty() {
        println!("No import sessions.");
        return EXIT_OK;
    }
    println!(
        "{:<28} {:<10} {:>5} {:>6} {:>5} {:>7}  {:<24} -> {:<24} UPDATED",
        "SESSION", "STAGE", "OK", "REVIEW", "MISS", "WRITTEN", "SOURCE", "DEST"
    );
    for session in sessions {
        println!(
            "{:<28} {:<10} {:>5} {:>6} {:>5} {:>7}  {:<24} -> {:<24} {}",
            session.session_id,
            session.stage.label(),
            session.counts.matched,
            session.counts.ambiguous,
            session.counts.not_found,
            session.counts.written,
            truncate_col(&session.source.display(), 24),
            truncate_col(&session.destination.display(), 24),
            session.updated_at
        );
    }
    EXIT_OK
}

fn show_session(job_id: &str) -> i32 {
    let session = match ImportSession::load(job_id) {
        Ok(session) => session,
        Err(e) => {
            eprintln!("ytt transfer session: {e:#}");
            return EXIT_FAILED;
        }
    };
    println!("Session: {}", session.session_id);
    println!("Stage: {}", session.stage.label());
    println!("Source: {}", session.source.display());
    println!("Destination: {}", session.destination.display());
    println!(
        "Rows: {} total, {} matched, {} review, {} not found, {} pending, {} skipped, {} written",
        session.counts.total,
        session.counts.matched,
        session.counts.ambiguous,
        session.counts.not_found,
        session.counts.pending,
        session.counts.skipped_local,
        session.counts.written
    );
    let mut printed = 0usize;
    for row in &session.rows {
        if matches!(row.status, ImportSessionRowStatus::Matched) {
            continue;
        }
        printed += 1;
        println!(
            "  {:>4}. {:<11} {} — {}",
            row.source_order,
            row_status_label(row.status),
            row.artists.join(", "),
            row.title
        );
        if let Some(album) = &row.album {
            println!("        album: {album}");
        }
        if let Some(isrc) = &row.isrc {
            println!("        isrc: {isrc}");
        }
        if let Some(decision) = &row.review_decision {
            println!("        decision: {}", review_decision_label(decision));
        }
        if !row.candidates.is_empty() {
            for candidate in &row.candidates {
                println!(
                    "        candidate {:.2}: {} ({})",
                    candidate.score, candidate.display, candidate.key
                );
            }
        }
        for warning in &row.warnings {
            println!("        warning: {warning}");
        }
        for error in &row.errors {
            println!("        error: {error}");
        }
    }
    if printed == 0 {
        println!("No review rows.");
    }
    EXIT_OK
}

fn review_decision_label(decision: &ReviewDecision) -> String {
    match decision {
        ReviewDecision::Accepted {
            key,
            score,
            display,
        } => format!("accepted {display} ({key}, {score:.2})"),
        ReviewDecision::Rejected => "rejected".to_owned(),
        ReviewDecision::Skipped => "skipped".to_owned(),
    }
}

fn row_status_label(status: ImportSessionRowStatus) -> &'static str {
    match status {
        ImportSessionRowStatus::Pending => "pending",
        ImportSessionRowStatus::Matched => "matched",
        ImportSessionRowStatus::Ambiguous => "review",
        ImportSessionRowStatus::NotFound => "not_found",
        ImportSessionRowStatus::SkippedLocal => "skipped",
    }
}

fn truncate_col(value: &str, max_chars: usize) -> String {
    let mut out = String::new();
    for (idx, ch) in value.chars().enumerate() {
        if idx + 1 >= max_chars {
            out.push('~');
            return out;
        }
        out.push(ch);
    }
    out
}

async fn backup(dir: PathBuf, also_csv: bool) -> i32 {
    let cfg = Config::load();
    let Some(cookie) = cfg.effective_cookie() else {
        eprintln!(
            "ytt transfer backup: this needs a YouTube Music cookie — add cookies.txt in Settings."
        );
        return EXIT_FAILED;
    };
    let ytm = match crate::api::ytmusic::YtMusicApi::from_cookie(&cookie).await {
        Ok(api) => api,
        Err(e) => {
            eprintln!(
                "ytt transfer backup: {}",
                crate::util::sanitize::sanitize_error_text(format!("{e:#}"))
            );
            return EXIT_FAILED;
        }
    };
    if let Err(e) = std::fs::create_dir_all(&dir) {
        eprintln!(
            "ytt transfer backup: could not create {}: {e}",
            dir.display()
        );
        return EXIT_FAILED;
    }
    let playlists = match ytm.library_playlists().await {
        Ok(p) => p,
        Err(e) => {
            eprintln!(
                "ytt transfer backup: {}",
                crate::util::sanitize::sanitize_error_text(format!("{e:#}"))
            );
            return EXIT_FAILED;
        }
    };
    let mut pace = super::matching::Pacing::ytm_default();
    let mut ok = 0usize;
    // Two playlist names that sanitize to the same stem would otherwise overwrite each other's
    // export; track used stems so colliding ones get a `-2`, `-3`, … suffix instead.
    let mut used_stems: std::collections::HashSet<String> = std::collections::HashSet::new();
    for (id, title, _count) in &playlists {
        pace.tick().await;
        let songs = match ytm.playlist_tracks_full(id).await {
            Ok(songs) => songs,
            Err(e) => {
                eprintln!(
                    "  skipped {title}: {}",
                    crate::util::sanitize::sanitize_error_text(format!("{e:#}"))
                );
                continue;
            }
        };
        let stem = unique_stem(sanitize_filename(title), &mut used_stems);
        let json_path = dir.join(format!("{stem}.json"));
        let file =
            super::json::PlaylistFile::new(title.clone(), format!("ytm:{id}"), songs.clone());
        if let Err(e) = super::json::write_playlist(&json_path, &file) {
            eprintln!("  skipped {title}: {e:#}");
            continue;
        }
        if also_csv
            && let Err(e) = super::csv::write_songs(&dir.join(format!("{stem}.csv")), &songs)
        {
            eprintln!("  csv for {title} failed: {e:#}");
        }
        println!("  {} ({} tracks)", json_path.display(), songs.len());
        ok += 1;
    }
    println!(
        "Backed up {ok}/{} playlists to {}",
        playlists.len(),
        dir.display()
    );
    EXIT_OK
}

/// Disambiguate export filenames: return `base` if unused, else `base-2`, `base-3`, … so two
/// playlists that sanitize to the same stem don't clobber each other. Records the winner.
fn unique_stem(base: String, used: &mut std::collections::HashSet<String>) -> String {
    if used.insert(base.clone()) {
        return base;
    }
    let mut n = 2usize;
    loop {
        let candidate = format!("{base}-{n}");
        if used.insert(candidate.clone()) {
            return candidate;
        }
        n += 1;
    }
}

fn sanitize_filename(name: &str) -> String {
    let cleaned: String = name
        .chars()
        .map(|c| match c {
            '/' | '\\' | ':' | '*' | '?' | '"' | '<' | '>' | '|' => '_',
            c => c,
        })
        .collect();
    let trimmed = cleaned.trim();
    if trimmed.is_empty() {
        "playlist".to_owned()
    } else {
        trimmed.chars().take(120).collect()
    }
}

// Auth + listing (also reachable as `ytt auth spotify`) ------------------------------

/// `ytt auth spotify [--client-id ID] [--status] [--logout]`.
pub fn run_auth(args: &[String]) -> i32 {
    let mut client_id_flag: Option<String> = None;
    let mut status = false;
    let mut logout = false;
    let mut it = args.iter().map(String::as_str);
    while let Some(arg) = it.next() {
        match arg {
            "--client-id" => match it.next() {
                Some(v) => client_id_flag = Some(v.to_owned()),
                None => {
                    eprintln!("ytt auth spotify: --client-id needs a value");
                    return EXIT_USAGE;
                }
            },
            "--status" => status = true,
            "--logout" => logout = true,
            "--help" | "-h" => {
                println!("Usage: ytt auth spotify [--client-id ID] [--status] [--logout]");
                return EXIT_OK;
            }
            other => {
                eprintln!("ytt auth spotify: unknown flag `{other}`");
                return EXIT_USAGE;
            }
        }
    }
    if logout {
        return match SpotifyToken::delete_saved() {
            Ok(()) => {
                println!("Spotify disconnected (token removed).");
                EXIT_OK
            }
            Err(e) => {
                eprintln!("ytt auth spotify: could not remove token: {e}");
                EXIT_FAILED
            }
        };
    }
    let Some(rt) = runtime() else {
        return EXIT_FAILED;
    };
    if status {
        return rt.block_on(async {
            let cfg = Config::load();
            let mut client = match SpotifyClient::from_saved(cfg.spotify.client_id.as_deref()) {
                Ok(c) => c,
                Err(e) => {
                    eprintln!("ytt auth spotify: {e}");
                    return EXIT_FAILED;
                }
            };
            match client.me().await {
                Ok(user) => {
                    println!("Connected as {}.", user.label());
                    EXIT_OK
                }
                Err(e) => {
                    eprintln!("ytt auth spotify: {e}");
                    EXIT_FAILED
                }
            }
        });
    }
    rt.block_on(connect(client_id_flag))
}

async fn connect(client_id_flag: Option<String>) -> i32 {
    let mut cfg = Config::load();
    // A --client-id flag also persists (it's step one of the setup guide).
    if let Some(id) = client_id_flag
        .as_deref()
        .map(str::trim)
        .filter(|i| !i.is_empty())
    {
        cfg.spotify.client_id = Some(id.to_owned());
        if let Err(e) = cfg.save() {
            eprintln!("ytt auth spotify: could not save config: {e}");
            return EXIT_FAILED;
        }
    }
    let Some(client_id) = cfg
        .spotify
        .client_id
        .clone()
        .map(|s| s.trim().to_owned())
        .filter(|s| !s.is_empty())
    else {
        eprintln!("ytt auth spotify: no Client ID configured.");
        eprintln!("Create an app at https://developer.spotify.com/dashboard with redirect URI");
        eprintln!(
            "  http://127.0.0.1:{}/callback",
            cfg.effective_spotify_port()
        );
        eprintln!("then run:  ytt auth spotify --client-id <YOUR-CLIENT-ID>");
        return EXIT_FAILED;
    };
    let port = cfg.effective_spotify_port();
    let http = reqwest::Client::builder()
        .user_agent(format!("yututui/{}", env!("CARGO_PKG_VERSION")))
        .timeout(Duration::from_secs(20))
        .build()
        .unwrap_or_default();
    let flow = auth::run_pkce_flow(&http, &client_id, port, &mut |url| {
        println!("Approve YuTuTui! in your browser:");
        println!();
        println!("  {url}");
        println!();
        let opened = crate::util::browser::open_in_browser_checked(&url);
        if !opened.launched() {
            eprintln!(
                "ytt auth spotify: could not open a browser automatically: {}",
                opened.failure_summary()
            );
            eprintln!("ytt auth spotify: paste the URL above into your browser to continue.");
        }
        println!("Waiting for approval (up to 5 minutes; Ctrl-C to abort)…");
    })
    .await;
    let token = match flow {
        Ok(token) => token,
        Err(e) => {
            eprintln!("ytt auth spotify: {e:#}");
            return EXIT_FAILED;
        }
    };
    let mut client = SpotifyClient::with_token(token);
    match client.me().await {
        Ok(user) => {
            println!("Connected as {}.", user.label());
            EXIT_OK
        }
        Err(SpotifyError::NotAllowlisted) => {
            eprintln!("ytt auth spotify: {}", SpotifyError::NotAllowlisted);
            EXIT_FAILED
        }
        Err(e) => {
            eprintln!("ytt auth spotify: connected, but /me failed: {e}");
            EXIT_FAILED
        }
    }
}

async fn list_spotify() -> i32 {
    let cfg = Config::load();
    let mut client = match SpotifyClient::from_saved(cfg.spotify.client_id.as_deref()) {
        Ok(c) => c,
        Err(e) => {
            eprintln!("ytt transfer: {e}");
            return EXIT_FAILED;
        }
    };
    match client.my_playlists().await {
        Ok(playlists) => {
            if playlists.is_empty() {
                println!("No playlists.");
                return EXIT_OK;
            }
            println!("{:<24} {:>6}  NAME", "ID", "TRACKS");
            for p in playlists {
                let collab = if p.collaborative { " (collab)" } else { "" };
                let owner = if p.owner.is_empty() {
                    String::new()
                } else {
                    format!("  [{}]", p.owner)
                };
                println!("{:<24} {:>6}  {}{}{}", p.id, p.total, p.name, collab, owner);
            }
            EXIT_OK
        }
        Err(e) => {
            eprintln!("ytt transfer: {e}");
            EXIT_FAILED
        }
    }
}

async fn list_ytm() -> i32 {
    let cfg = Config::load();
    let Some(cookie) = cfg.effective_cookie() else {
        eprintln!(
            "ytt transfer: this needs a YouTube Music cookie — add cookies.txt (or `cookie`) in Settings › General."
        );
        return EXIT_FAILED;
    };
    let api = match crate::api::ytmusic::YtMusicApi::from_cookie(&cookie).await {
        Ok(api) => api,
        Err(e) => {
            eprintln!(
                "ytt transfer: YouTube Music auth failed: {}",
                crate::util::sanitize::sanitize_error_text(format!("{e:#}"))
            );
            return EXIT_FAILED;
        }
    };
    match api.library_playlists().await {
        Ok(playlists) => {
            if playlists.is_empty() {
                println!("No playlists.");
                return EXIT_OK;
            }
            println!("{:<40} {:>6}  NAME", "ID", "TRACKS");
            for (id, title, count) in playlists {
                println!("{id:<40} {count:>6}  {title}");
            }
            EXIT_OK
        }
        Err(e) => {
            eprintln!(
                "ytt transfer: {}",
                crate::util::sanitize::sanitize_error_text(format!("{e:#}"))
            );
            EXIT_FAILED
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn raw_playlist_id() -> &'static str {
        "37i9dQZF1DXcBWIGoYBM5M"
    }

    fn file_spec(dest: TransferDest) -> JobSpec {
        JobSpec {
            source: TransferSource::File {
                path: "input.csv".into(),
            },
            dest,
            dry_run: false,
            min_score: 0.80,
            take_best: false,
            rematch: false,
        }
    }

    fn config_without_cookie() -> Config {
        Config {
            cookie: None,
            cookies_file: Some(
                std::env::temp_dir().join(format!("yututui-missing-cookie-{}", std::process::id())),
            ),
            ..Config::default()
        }
    }

    fn parse_common_err(args: &mut Vec<&str>) -> String {
        match parse_common(args) {
            Ok(_) => panic!("parse_common should have failed"),
            Err(error) => error,
        }
    }

    #[test]
    fn spotify_playlist_id_accepts_uri_url_and_raw_id() {
        assert_eq!(
            parse_spotify_playlist_id("spotify:playlist:abcDEF123"),
            Some("abcDEF123".to_owned())
        );
        assert_eq!(
            parse_spotify_playlist_id(
                "https://open.spotify.com/playlist/37i9dQZF1DXcBWIGoYBM5M?si=abc"
            ),
            Some(raw_playlist_id().to_owned())
        );
        assert_eq!(
            parse_spotify_playlist_id(raw_playlist_id()),
            Some(raw_playlist_id().to_owned())
        );
        assert_eq!(parse_spotify_playlist_id("not a playlist"), None);
    }

    #[test]
    fn common_flags_are_removed_and_validated() {
        let mut args = vec![
            "--dry-run",
            "liked",
            "--yes",
            "--min-score",
            "0.65",
            "--take-best",
            "--rematch",
            "--to",
            "likes",
        ];
        let flags = parse_common(&mut args).expect("valid flags");

        assert!(flags.dry_run);
        assert!(flags.yes);
        assert_eq!(flags.min_score, 0.65);
        assert!(flags.take_best);
        assert!(flags.rematch);
        assert_eq!(args, vec!["liked", "--to", "likes"]);

        let mut missing = vec!["--min-score"];
        assert!(parse_common_err(&mut missing).contains("needs a value"));

        let mut bad = vec!["--min-score", "nope"];
        assert!(parse_common_err(&mut bad).contains("bad --min-score"));

        let mut out_of_range = vec!["--min-score", "1.2"];
        assert!(parse_common_err(&mut out_of_range).contains("must be 0..1"));
    }

    #[test]
    fn import_liked_defaults_to_new_playlist_and_keeps_flags() {
        let (spec, yes) = parse_import(&[
            "liked",
            "--dry-run",
            "--yes",
            "--min-score",
            "0.72",
            "--take-best",
            "--rematch",
        ])
        .expect("liked import");

        assert!(yes);
        assert!(matches!(spec.source, TransferSource::SpotifyLiked));
        assert!(matches!(
            spec.dest,
            TransferDest::YtmNewPlaylist { name: None }
        ));
        assert!(spec.dry_run);
        assert_eq!(spec.min_score, 0.72);
        assert!(spec.take_best);
        assert!(spec.rematch);
    }

    #[test]
    fn import_liked_can_still_target_ytm_likes_explicitly() {
        let (spec, _yes) = parse_import(&["liked", "--to", "likes"]).expect("liked import");

        assert!(matches!(spec.source, TransferSource::SpotifyLiked));
        assert!(matches!(spec.dest, TransferDest::YtmLikes));
    }

    #[test]
    fn import_spotify_playlist_can_target_named_local_playlist() {
        let (spec, yes) = parse_import(&["spotify:playlist:abc123", "--to", "local:Road Trip"])
            .expect("playlist import");

        assert!(!yes);
        match spec.source {
            TransferSource::SpotifyPlaylist { id } => assert_eq!(id, "abc123"),
            other => panic!("wrong source: {other:?}"),
        }
        match spec.dest {
            TransferDest::LocalPlaylist { name } => {
                assert_eq!(name, Some("Road Trip".to_owned()));
            }
            other => panic!("wrong destination: {other:?}"),
        }
        assert!(!spec.dry_run);
        assert_eq!(spec.min_score, 0.80);
    }

    #[test]
    fn import_existing_file_wins_over_source_keywords() {
        let path =
            std::env::temp_dir().join(format!("ytt-transfer-cli-{}-liked", std::process::id()));
        std::fs::write(&path, "title,artist\nSong,Artist\n").expect("create temp import file");

        let source = path.to_string_lossy().to_string();
        let (spec, _yes) = parse_import(&[&source]).expect("file import");
        let _ = std::fs::remove_file(&path);

        match spec.source {
            TransferSource::File { path: parsed } => assert_eq!(parsed, path),
            other => panic!("wrong source: {other:?}"),
        }
        assert!(matches!(
            spec.dest,
            TransferDest::YtmNewPlaylist { name: None }
        ));
    }

    #[test]
    fn import_rejects_bad_source_destination_and_extra_args() {
        assert!(
            parse_import(&["not-a-source"])
                .unwrap_err()
                .contains("not an existing file")
        );
        assert!(
            parse_import(&["liked", "--to", "spotify"])
                .unwrap_err()
                .contains("--to expects")
        );
        assert!(
            parse_import(&["liked", "extra"])
                .unwrap_err()
                .contains("unexpected argument")
        );
    }

    #[test]
    fn ytm_export_sources_are_explicitly_namespaced() {
        match parse_ytm_source("ytm:PL123").expect("ytm source") {
            TransferSource::YtmPlaylist { id } => assert_eq!(id, "PL123"),
            other => panic!("wrong source: {other:?}"),
        }
        match parse_ytm_source("local:Favorites").expect("local source") {
            TransferSource::LocalPlaylist { key } => assert_eq!(key, "Favorites"),
            other => panic!("wrong source: {other:?}"),
        }
        assert!(parse_ytm_source("PL123").unwrap_err().contains("expected"));
    }

    #[test]
    fn export_to_spotify_and_files_preserves_names_and_formats() {
        let (spec, yes) = parse_export(&[
            "ytm:PL123",
            "--to",
            "spotify",
            "--name",
            "Mirror",
            "--dry-run",
        ])
        .expect("spotify export");
        assert!(!yes);
        assert!(spec.dry_run);
        match spec.dest {
            TransferDest::SpotifyNewPlaylist { name } => {
                assert_eq!(name, Some("Mirror".to_owned()));
            }
            other => panic!("wrong destination: {other:?}"),
        }

        let (spec, yes) =
            parse_export(&["local:Favorites", "--to", "backup.csv", "--yes"]).expect("file export");
        assert!(yes);
        match spec.source {
            TransferSource::LocalPlaylist { key } => assert_eq!(key, "Favorites"),
            other => panic!("wrong source: {other:?}"),
        }
        match spec.dest {
            TransferDest::File { path, format } => {
                assert_eq!(path, PathBuf::from("backup.csv"));
                assert_eq!(format, FileFormat::Csv);
            }
            other => panic!("wrong destination: {other:?}"),
        }

        let (spec, _yes) =
            parse_export(&["ytm:PL123", "--to", "backup.JSON"]).expect("json export");
        assert!(matches!(
            spec.dest,
            TransferDest::File {
                format: FileFormat::Json,
                ..
            }
        ));
    }

    #[test]
    fn export_rejects_missing_destination_and_bad_file_extension() {
        assert!(
            parse_export(&["ytm:PL123"])
                .unwrap_err()
                .contains("missing --to")
        );
        assert!(
            parse_export(&["ytm:PL123", "--to", "backup.txt"])
                .unwrap_err()
                .contains("must end in .json or .csv")
        );
        assert!(
            parse_export(&["ytm:PL123", "--name"])
                .unwrap_err()
                .contains("--name needs a value")
        );
    }

    #[test]
    fn backup_parse_requires_dir_and_accepts_csv_flag() {
        let (dir, csv) = parse_backup(&["--csv", "--dir", "out"]).expect("backup args");
        assert_eq!(dir, PathBuf::from("out"));
        assert!(csv);
        assert!(
            parse_backup(&["--csv"])
                .unwrap_err()
                .contains("missing --dir")
        );
        assert!(
            parse_backup(&["--dir"])
                .unwrap_err()
                .contains("--dir needs a path")
        );
        assert!(
            parse_backup(&["--dir", "out", "--bad"])
                .unwrap_err()
                .contains("unexpected argument")
        );
    }

    #[test]
    fn sanitized_backup_stems_are_nonempty_limited_and_unique() {
        assert_eq!(sanitize_filename("  "), "playlist");
        assert_eq!(
            sanitize_filename(r#"a/b\c:d*e?f"g<h>i|j"#),
            "a_b_c_d_e_f_g_h_i_j"
        );
        assert_eq!(sanitize_filename(&"x".repeat(140)).len(), 120);

        let mut used = std::collections::HashSet::new();
        assert_eq!(unique_stem("mix".to_owned(), &mut used), "mix");
        assert_eq!(unique_stem("mix".to_owned(), &mut used), "mix-2");
        assert_eq!(unique_stem("mix".to_owned(), &mut used), "mix-3");
    }

    #[test]
    fn next_steps_include_session_review_and_local_download_hint() {
        let report = TransferReport {
            job_id: "sp2yt-20260708-abcd".to_owned(),
            ..TransferReport::default()
        };
        let session = ImportSession {
            session_id: report.job_id.clone(),
            destination: crate::transfer::session::SessionEndpoint {
                kind: "local_playlist".to_owned(),
                key: Some("playlist-key".to_owned()),
                label: Some("Imported Mix".to_owned()),
            },
            ..ImportSession::default()
        };

        let lines = next_step_lines(&report, Some(&session));

        assert_eq!(
            lines[0],
            "Session review: ytt transfer session sp2yt-20260708-abcd"
        );
        assert!(lines[1].contains("Imported Mix"));
        assert!(lines[1].contains("Shift+D"));
        assert!(lines[1].contains("Local Deck"));
        assert!(lines[1].contains("Import Sessions"));
        assert_eq!(
            lines[2],
            "Preview downloads: ytt transfer download sp2yt-20260708-abcd --accepted --dry-run"
        );
        assert_eq!(
            lines[3],
            "Preview organize: ytt transfer organize sp2yt-20260708-abcd --root <LOCAL_ROOT> --dry-run"
        );
    }

    #[tokio::test]
    async fn build_ctx_uses_anonymous_ytm_for_local_playlist_without_cookie() {
        let spec = file_spec(TransferDest::LocalPlaylist {
            name: Some("Imported".to_owned()),
        });
        let cfg = config_without_cookie();

        let ctx = build_ctx(&spec, &cfg).await.unwrap();

        assert!(matches!(
            ctx.ytm,
            Some(crate::api::ytmusic::YtMusicApi::Anonymous)
        ));
        assert!(ctx.spotify.is_none());
    }

    #[tokio::test]
    async fn build_ctx_requires_cookie_for_account_writes() {
        let spec = file_spec(TransferDest::YtmLikes);
        let cfg = config_without_cookie();

        let err = match build_ctx(&spec, &cfg).await {
            Ok(_) => panic!("account write without a cookie should fail"),
            Err(err) => err,
        };

        assert!(err.contains("YouTube Music cookie"));
    }

    #[test]
    fn run_reports_usage_without_starting_runtime_for_parse_failures() {
        assert_eq!(run(&[]), EXIT_USAGE);
        assert_eq!(run(&["--help".to_owned()]), EXIT_OK);
        assert_eq!(run(&["unknown".to_owned()]), EXIT_USAGE);
        assert_eq!(
            run(&["list".to_owned(), "unknown-side".to_owned()]),
            EXIT_USAGE
        );
        assert_eq!(
            run(&["import".to_owned(), "not-a-source".to_owned()]),
            EXIT_USAGE
        );
    }
}
