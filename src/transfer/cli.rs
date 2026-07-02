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

use super::checkpoint::{Checkpoint, TransferReport};
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
        Some("import") => {
            let rest: Vec<&str> = it.collect();
            match parse_import(&rest) {
                Ok((spec, yes)) => runtime()
                    .map_or(EXIT_FAILED, |rt| rt.block_on(execute_new(spec, yes, "sp2yt"))),
                Err(msg) => {
                    eprintln!("ytt transfer import: {msg}");
                    EXIT_USAGE
                }
            }
        }
        Some("export") => {
            let rest: Vec<&str> = it.collect();
            match parse_export(&rest) {
                Ok((spec, yes)) => runtime()
                    .map_or(EXIT_FAILED, |rt| rt.block_on(execute_new(spec, yes, "yt2sp"))),
                Err(msg) => {
                    eprintln!("ytt transfer export: {msg}");
                    EXIT_USAGE
                }
            }
        }
        Some("backup") => {
            let rest: Vec<&str> = it.collect();
            match parse_backup(&rest) {
                Ok((dir, csv)) => {
                    runtime().map_or(EXIT_FAILED, |rt| rt.block_on(backup(dir, csv)))
                }
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
    let dest = dest.unwrap_or(match source {
        // Liked songs default to the like button; playlists to a fresh playlist.
        TransferSource::SpotifyLiked => TransferDest::YtmLikes,
        _ => TransferDest::YtmNewPlaylist { name: None },
    });
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
    let needs_ytm = matches!(
        spec.source,
        TransferSource::YtmPlaylist { .. }
    ) || matches!(
        spec.dest,
        TransferDest::YtmNewPlaylist { .. }
            | TransferDest::YtmExistingPlaylist { .. }
            | TransferDest::YtmLikes
            | TransferDest::LocalPlaylist { .. }
    );
    let spotify = if needs_spotify {
        Some(SpotifyClient::from_saved(cfg.spotify.client_id.as_deref()).map_err(|e| e.to_string())?)
    } else {
        None
    };
    let ytm = if needs_ytm {
        let cookie = cfg.effective_cookie().ok_or_else(|| {
            "this needs a YouTube Music cookie — add cookies.txt (or `cookie`) in Settings › General"
                .to_owned()
        })?;
        Some(
            crate::api::ytmusic::YtMusicApi::from_cookie(&cookie)
                .await
                .map_err(|e| {
                    crate::util::sanitize::sanitize_error_text(format!(
                        "YouTube Music auth failed: {e:#}"
                    ))
                })?,
        )
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
    let report = match run_job(job_id.clone(), first_spec, None, &mut ctx, &mut print_progress)
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
            println!("Aborted — nothing was written. Resume later with `ytt transfer resume {job_id}`.");
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
    match run_job(job_id.clone(), spec, Some(cp), &mut ctx, &mut print_progress).await {
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
    EXIT_OK
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
        println!("  ambiguous: {} — {}  →  {}", row.artists, row.title, row.note);
    }
    for row in &report.not_found {
        println!("  not found: {} — {}", row.artists, row.title);
    }
}

fn print_progress(p: TransferProgress) {
    let counts = if p.stage == Stage::Matching {
        format!(" ({} ok, {} ambiguous, {} miss)", p.matched, p.ambiguous, p.not_found)
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
        eprintln!("ytt transfer backup: could not create {}: {e}", dir.display());
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
        let stem = sanitize_filename(title);
        let json_path = dir.join(format!("{stem}.json"));
        let file =
            super::json::PlaylistFile::new(title.clone(), format!("ytm:{id}"), songs.clone());
        if let Err(e) = super::json::write_playlist(&json_path, &file) {
            eprintln!("  skipped {title}: {e:#}");
            continue;
        }
        if also_csv && let Err(e) = super::csv::write_songs(&dir.join(format!("{stem}.csv")), &songs)
        {
            eprintln!("  csv for {title} failed: {e:#}");
        }
        println!("  {} ({} tracks)", json_path.display(), songs.len());
        ok += 1;
    }
    println!("Backed up {ok}/{} playlists to {}", playlists.len(), dir.display());
    EXIT_OK
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
        .user_agent(format!("ytm-tui/{}", env!("CARGO_PKG_VERSION")))
        .timeout(Duration::from_secs(20))
        .build()
        .unwrap_or_default();
    let flow = auth::run_pkce_flow(&http, &client_id, port, &mut |url| {
        println!("Approve ytm-tui in your browser:");
        println!();
        println!("  {url}");
        println!();
        crate::util::browser::open_in_browser(&url);
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
