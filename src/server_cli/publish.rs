//! `ytt server publish` — copy downloaded tracks into the server's music folder.
//!
//! The music folder is a plain filesystem path because OpenSubsonic has no upload endpoint:
//! `stream`, `download` and `getCoverArt` are read-only and Navidrome exposes no ingest API. So the
//! path cannot be validated against the server — `getMusicFolders` returns ids and names, never
//! paths — and a successful copy proves only that bytes reached the directory the user named.
//! Status therefore reports *published* and *confirmed on the server* as separate counts rather
//! than implying the first means the second.

use std::path::{Path, PathBuf};

use serde::Serialize;

use yututui::open_subsonic::publish::{
    PublishError, PublishLedger, PublishReport, PublishRequest, load as load_ledger, publish_track,
    set_music_folder,
};
use yututui::open_subsonic::{LibraryScanRequest, OpenSubsonicPaths, request_library_scan};

const MAX_MUSIC_FOLDER_BYTES: usize = 4 * 1024;
const SCAN_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(25);

#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) enum PublishCommand {
    Setup,
    Status { json: bool },
    List { json: bool },
    Track { video_id: String },
}

pub(super) fn parse(args: &[String]) -> Result<PublishCommand, String> {
    match args {
        [action] if action == "setup" => Ok(PublishCommand::Setup),
        [action] if action == "status" => Ok(PublishCommand::Status { json: false }),
        [action, flag] if action == "status" && flag == "--json" => {
            Ok(PublishCommand::Status { json: true })
        }
        [action] if action == "list" => Ok(PublishCommand::List { json: false }),
        [action, flag] if action == "list" && flag == "--json" => {
            Ok(PublishCommand::List { json: true })
        }
        [action, video_id] if action == "track" => {
            if video_id.trim().is_empty() {
                return Err("track requires a VIDEO_ID".to_owned());
            }
            Ok(PublishCommand::Track {
                video_id: video_id.clone(),
            })
        }
        [] => Err("publish requires `setup`, `status`, `list`, or `track <VIDEO_ID>`".to_owned()),
        _ => Err(
            "publish accepts `setup`, `status [--json]`, `list [--json]`, or `track <VIDEO_ID>`"
                .to_owned(),
        ),
    }
}

/// Resolve a user-supplied music folder.
///
/// Deliberately not the personal-data-export validators, which prove no other user can write the
/// directory: a music folder shared with a server daemon fails that by construction. This is the
/// permissive library-destination check — a real directory, not a symlink — plus the same refusal
/// of control and bidirectional characters the export path applies to its `--to` argument.
pub(super) fn resolve_music_folder(raw: &str) -> Result<PathBuf, String> {
    let expanded = crate::data_cli::expand_tilde(raw)?;
    if expanded.as_os_str().is_empty() {
        return Err("the music folder cannot be empty".to_owned());
    }
    let absolute = std::path::absolute(&expanded)
        .map_err(|error| format!("resolve `{}`: {error}", expanded.display()))?;
    reject_terminal_unsafe(&absolute)?;
    let metadata = std::fs::symlink_metadata(&absolute)
        .map_err(|error| format!("inspect `{}`: {error}", absolute.display()))?;
    if metadata.file_type().is_symlink() {
        return Err(format!(
            "`{}` is a symbolic link; choose the real directory",
            absolute.display()
        ));
    }
    if !metadata.is_dir() {
        return Err(format!("`{}` is not a directory", absolute.display()));
    }
    let canonical = std::fs::canonicalize(&absolute)
        .map_err(|error| format!("resolve `{}`: {error}", absolute.display()))?;
    reject_terminal_unsafe(&canonical)?;
    Ok(canonical)
}

fn reject_terminal_unsafe(path: &Path) -> Result<(), String> {
    if path
        .to_string_lossy()
        .chars()
        .any(crate::data_cli::is_terminal_unsafe_character)
    {
        return Err("the music folder contains unsafe control characters".to_owned());
    }
    Ok(())
}

pub(super) fn run_setup() -> Result<(), String> {
    super::initialize_writer()?;
    let paths = super::paths()?;
    println!("Enter the path to your music server's library folder.");
    #[cfg(windows)]
    println!("On Windows this relies on the share's own permissions; YuTuTui sets none.");
    let raw = super::prompt_required("Music folder: ", MAX_MUSIC_FOLDER_BYTES)?;
    let folder = resolve_music_folder(&raw)?;
    set_music_folder(&paths, &folder).map_err(|error| error.to_string())?;
    println!("Music folder saved: {}", folder.display());
    println!("Publish a track with `ytt server publish track <VIDEO_ID>`.");
    Ok(())
}

#[derive(Serialize)]
struct JsonStatus {
    music_folder_configured: bool,
    published: usize,
    confirmed_on_server: usize,
    needing_attention: usize,
}

pub(super) fn run_status(json: bool) -> Result<(), String> {
    super::initialize_reader()?;
    let paths = super::paths()?;
    let ledger = load_ledger(&paths).map_err(|error| error.to_string())?;
    let status = JsonStatus {
        music_folder_configured: ledger.music_folder().is_some(),
        published: ledger.published().len(),
        confirmed_on_server: ledger.confirmed_count(),
        needing_attention: ledger.pending().len(),
    };
    if json {
        // The folder path itself is deliberately omitted; it can name a mount the user would not
        // want in a pasted diagnostic.
        println!(
            "{}",
            serde_json::to_string_pretty(&status).map_err(|error| error.to_string())?
        );
        return Ok(());
    }
    match ledger.music_folder() {
        Some(folder) => println!("Music folder: {}", folder.display()),
        None => println!("Music folder: not set (run `ytt server publish setup`)"),
    }
    println!("Published: {}", status.published);
    println!(
        "Confirmed on the server: {} (a copy alone does not prove the server can see it)",
        status.confirmed_on_server
    );
    if status.needing_attention > 0 {
        println!("Needing attention: {}", status.needing_attention);
        for (video_id, pending) in ledger.pending() {
            println!("  {video_id}  {}", pending.relative_path);
        }
    }
    Ok(())
}

#[derive(Serialize)]
struct JsonListRow {
    video_id: String,
    title: String,
    state: &'static str,
}

pub(super) fn run_list(json: bool) -> Result<(), String> {
    super::initialize_reader()?;
    let paths = super::paths()?;
    let ledger = load_ledger(&paths).map_err(|error| error.to_string())?;
    let store = yututui::downloads::DownloadStore::load();
    let rows: Vec<JsonListRow> = store
        .tracks()
        .iter()
        .filter(|song| song.local_path.is_some())
        .map(|song| JsonListRow {
            video_id: song.video_id.clone(),
            title: song.title.clone(),
            state: publish_state(&ledger, &song.video_id),
        })
        .collect();
    if json {
        println!(
            "{}",
            serde_json::to_string_pretty(&rows).map_err(|error| error.to_string())?
        );
        return Ok(());
    }
    if rows.is_empty() {
        println!("No downloaded tracks to publish.");
        return Ok(());
    }
    for row in rows {
        println!("{:<16}{:<10}{}", row.video_id, row.state, row.title);
    }
    Ok(())
}

fn publish_state(ledger: &PublishLedger, video_id: &str) -> &'static str {
    if ledger.pending().contains_key(video_id) {
        return "attention";
    }
    match ledger.published().get(video_id) {
        Some(track) if track.confirmed_on_server => "confirmed",
        Some(_) => "published",
        None => "local",
    }
}

pub(super) fn run_track(video_id: &str) -> Result<(), String> {
    super::initialize_writer()?;
    let paths = super::paths()?;
    let request = request_for(video_id)?;
    let now = now_unix();
    match publish_track(&paths, &request, now) {
        Ok(PublishReport::Published { relative_path }) => {
            println!("Published: {relative_path}");
            report_scan(&paths);
            Ok(())
        }
        Ok(PublishReport::AlreadyPublished { relative_path }) => {
            println!("Already published, unchanged: {relative_path}");
            Ok(())
        }
        Ok(PublishReport::Conflict { relative_path }) => Err(format!(
            "`{relative_path}` already holds different audio; nothing was replaced. Move or rename \
             the server copy if you want this track published."
        )),
        Err(PublishError::NotConfigured) => {
            Err("no music folder is set; run `ytt server publish setup` first".to_owned())
        }
        Err(error) => Err(error.to_string()),
    }
}

fn request_for(video_id: &str) -> Result<PublishRequest, String> {
    let store = yututui::downloads::DownloadStore::load();
    let song = store
        .tracks()
        .iter()
        .find(|song| song.video_id == video_id)
        .ok_or_else(|| {
            format!("`{video_id}` is not a downloaded track; see `ytt server publish list`")
        })?;
    let source = song.local_path.clone().ok_or_else(|| {
        format!("`{video_id}` has no downloaded file; download it before publishing")
    })?;
    Ok(PublishRequest {
        video_id: song.video_id.clone(),
        title: song.title.clone(),
        artist: Some(song.artist.clone()),
        album_artist: song.album_artist.clone(),
        album: song.album.clone(),
        track_number: song.track_number,
        source,
    })
}

/// Ask the server to rescan, and say only what was actually established.
///
/// The scan is a courtesy, never a gate: the bytes are already in the music folder by the time
/// this runs. Nothing here can turn a completed publication into a failure, and none of these
/// messages claims the server has the track — only a later confirmation can say that.
fn report_scan(paths: &OpenSubsonicPaths) {
    let advice = match run_scan(paths) {
        LibraryScanRequest::Started => {
            "Asked your server to rescan; the track appears once that finishes."
        }
        LibraryScanRequest::NoServer => {
            "No music server is connected, so nothing was asked to rescan."
        }
        LibraryScanRequest::Unsupported => {
            "Your server does not support scan requests; it will pick the track up on its own schedule."
        }
        LibraryScanRequest::NotPermitted => {
            "This account may not trigger a scan, so the track appears at your server's next scheduled one."
        }
        LibraryScanRequest::Unavailable => {
            "Could not reach your server to request a scan; the file is already in place."
        }
    };
    println!("{advice}");
    println!(
        "If it never appears, the folder you configured is probably not the one your server indexes."
    );
}

fn run_scan(paths: &OpenSubsonicPaths) -> LibraryScanRequest {
    let Ok(runtime) = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
    else {
        return LibraryScanRequest::Unavailable;
    };
    runtime
        .block_on(async { tokio::time::timeout(SCAN_TIMEOUT, request_library_scan(paths)).await })
        .unwrap_or(LibraryScanRequest::Unavailable)
}

fn now_unix() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|elapsed| elapsed.as_secs() as i64)
        .unwrap_or_default()
}

#[cfg(test)]
mod tests;
