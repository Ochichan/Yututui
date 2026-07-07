//! The managed yt-dlp binary: on-disk layout, persisted state, and the
//! downloader/updater that keeps it fresh.
//!
//! Layout under the app data dir (`~/.local/share/ytm-tui` on Linux):
//! ```text
//! tools/
//!   yt-dlp[.exe]     the managed standalone binary (official release asset)
//!   ytdlp.json       ManagedState: channel/version/check timestamps + probe cache
//!   .update.lock     cross-process single-flight for downloads (bandwidth dedup)
//!   .yt-dlp.tmp-<pid> in-flight download (atomically renamed into place)
//! ```
//!
//! Two processes (TUI + daemon) may race here. Correctness never depends on the
//! lock: temp names are pid-unique and the final rename is atomic, so concurrent
//! updaters both produce a valid binary and the last writer wins; a reader exec'ing
//! mid-rename sees the old or the new file, never a torn one.
//!
//! Update path (see [`check_and_update`]): resolve the channel's latest tag from the
//! `releases/latest` **302 redirect** (the plain web endpoint — no GitHub API rate
//! limits), then fetch the asset and its `SHA2-256SUMS` line from **tag-pinned**
//! `releases/download/<tag>/…` URLs, verify the sums file with yt-dlp's pinned GPG
//! signing key, and only then download the binary. The body streams to disk with an
//! incremental SHA-256 (never ~40 MB buffered in RAM), is verified, then atomically
//! renamed into place. The opt-out for users who refuse networked executable downloads
//! is `tools.ytdlp_managed = false`.

use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use futures::StreamExt;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use tokio::io::AsyncWriteExt;

use crate::config::ToolsConfig;
use crate::util::safe_fs;

use super::{ToolsEvent, YtdlpChannel};

/// Upper bound on probe-cache entries (override + managed + system is 3; a few
/// spares for paths that moved).
const PROBE_CACHE_MAX: usize = 8;

/// Persisted managed-binary state. Every field `#[serde(default)]` so the file
/// forward-migrates; a corrupt file loads as default (worst case: one redundant
/// version probe / update check).
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct ManagedState {
    /// Channel the installed binary came from (a channel switch forces a reinstall).
    pub channel: Option<YtdlpChannel>,
    /// Version (release tag) of the installed managed binary.
    pub version: Option<String>,
    /// Hex SHA-256 the installed binary was verified against.
    pub sha256: Option<String>,
    /// Last verified mtime of the installed managed binary. A mismatch means the
    /// metadata no longer describes the executable on disk.
    pub installed_mtime_unix: Option<u64>,
    /// Last verified byte length of the installed managed binary.
    pub installed_len: Option<u64>,
    /// Unix seconds of the last *successful* update check.
    pub last_check_unix: Option<u64>,
    /// Unix seconds of the last check *attempt* (backoff for failing networks).
    pub last_attempt_unix: Option<u64>,
    /// `--version` probe results keyed by (path, mtime, len) so steady-state
    /// startups spawn zero yt-dlp subprocesses (a PyInstaller binary takes several
    /// hundred ms just to print its version).
    pub probe_cache: Vec<ProbeEntry>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ProbeEntry {
    pub path: String,
    pub mtime_unix: u64,
    pub len: u64,
    pub version: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BinaryInspection {
    pub version: String,
    pub sha256: String,
    pub mtime_unix: u64,
    pub len: u64,
}

/// `<data dir>/tools`, the directory everything in this module lives in. The
/// `YTM_TOOLS_DIR` env var overrides it — unit tests and the QA sandbox must never
/// touch (or probe-cache into) the user's real managed binary.
pub fn tools_dir() -> Option<PathBuf> {
    if let Ok(env) = std::env::var("YTM_TOOLS_DIR")
        && !env.trim().is_empty()
    {
        return Some(PathBuf::from(env.trim()));
    }
    directories::ProjectDirs::from("", "", "ytm-tui").map(|d| d.data_dir().join("tools"))
}

/// Where the managed binary is installed.
pub fn managed_path() -> Option<PathBuf> {
    let name = if cfg!(windows) {
        "yt-dlp.exe"
    } else {
        "yt-dlp"
    };
    tools_dir().map(|d| d.join(name))
}

/// The managed binary path, but only when a file actually exists there (the
/// selection candidate).
pub fn installed_managed_path() -> Option<PathBuf> {
    managed_path().filter(|p| p.is_file())
}

fn state_path() -> Option<PathBuf> {
    tools_dir().map(|d| d.join("ytdlp.json"))
}

pub fn load_state() -> ManagedState {
    match state_path() {
        Some(p) => safe_fs::load_json_or_default(&p),
        None => ManagedState::default(),
    }
}

pub fn save_state(state: &ManagedState) {
    let Some(path) = state_path() else { return };
    if let Some(dir) = path.parent() {
        let _ = safe_fs::ensure_private_dir(dir);
    }
    if let Err(e) = safe_fs::write_private_atomic_json(&path, state) {
        tracing::warn!(error = %e, "failed to persist yt-dlp tool state");
    }
}

pub fn clear_probe_cache() {
    let mut state = load_state();
    if state.probe_cache.is_empty() {
        return;
    }
    state.probe_cache.clear();
    save_state(&state);
}

pub fn remove_update_lock_if_free() -> Result<bool, String> {
    let Some(dir) = tools_dir() else {
        return Ok(false);
    };
    let lock_path = dir.join(".update.lock");
    if !lock_path.exists() {
        return Ok(false);
    }
    let Some(lock) = UpdateLock::try_acquire(&dir) else {
        return Ok(false);
    };
    drop(lock);
    std::fs::remove_file(&lock_path)
        .map(|()| true)
        .or_else(|e| {
            if e.kind() == std::io::ErrorKind::NotFound {
                Ok(false)
            } else {
                Err(format!("failed to remove {}: {e}", lock_path.display()))
            }
        })
}

pub fn now_unix() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

/// (mtime seconds, byte length) of a file — the probe-cache key alongside the path.
fn file_stamp(path: &Path) -> Option<(u64, u64)> {
    let meta = std::fs::metadata(path).ok()?;
    let mtime = meta
        .modified()
        .ok()?
        .duration_since(UNIX_EPOCH)
        .ok()?
        .as_secs();
    Some((mtime, meta.len()))
}

pub fn sha256_file(path: &Path) -> std::io::Result<String> {
    use std::io::Read;

    let mut file = std::fs::File::open(path)?;
    let mut hasher = Sha256::new();
    let mut buf = [0u8; 64 * 1024];
    loop {
        let n = file.read(&mut buf)?;
        if n == 0 {
            break;
        }
        hasher.update(&buf[..n]);
    }
    Ok(format!("{:x}", hasher.finalize()))
}

pub async fn inspect_binary(path: &Path) -> Result<BinaryInspection, String> {
    let (mtime_unix, len) =
        file_stamp(path).ok_or_else(|| format!("cannot stat {}", path.display()))?;
    let sha256 = sha256_file(path).map_err(|e| format!("cannot hash {}: {e}", path.display()))?;
    let path_str = path.to_string_lossy().into_owned();
    let version = super::probe_version(&path_str)
        .await
        .ok_or_else(|| format!("{} did not report a yt-dlp version", path.display()))?;
    Ok(BinaryInspection {
        version,
        sha256,
        mtime_unix,
        len,
    })
}

fn state_stamp_matches(path: &Path, state: &ManagedState) -> bool {
    file_stamp(path).is_some_and(|(mtime, len)| {
        state.installed_mtime_unix == Some(mtime) && state.installed_len == Some(len)
    })
}

async fn managed_installation_matches(
    dest: &Path,
    state: &ManagedState,
    channel: YtdlpChannel,
    tag: &str,
) -> Result<BinaryInspection, String> {
    if state.channel != Some(channel) || state.version.as_deref() != Some(tag) {
        return Err("metadata channel/version does not match latest tag".to_owned());
    }
    let actual = inspect_binary(dest).await?;
    if actual.version != tag {
        return Err(format!(
            "metadata says {tag}, but binary reports {}",
            actual.version
        ));
    }
    if let Some(expected) = state.sha256.as_deref()
        && !expected.eq_ignore_ascii_case(&actual.sha256)
    {
        return Err("metadata sha256 does not match installed binary".to_owned());
    }
    if let (Some(mtime), Some(len)) = (state.installed_mtime_unix, state.installed_len)
        && (mtime != actual.mtime_unix || len != actual.len)
    {
        return Err("metadata file stamp does not match installed binary".to_owned());
    }
    Ok(actual)
}

/// Probe a binary's version through the persistent cache: a hit for the same
/// (path, mtime, len) returns without spawning anything; a miss runs
/// `--version` and records the result. Failed probes are never cached, so a
/// binary that gets fixed in place re-probes next time.
pub(crate) async fn cached_probe(path: &Path) -> Option<String> {
    let path_str = path.to_string_lossy().into_owned();
    let stamp = file_stamp(path);

    if let Some((mtime, len)) = stamp {
        let state = load_state();
        if let Some(hit) = state
            .probe_cache
            .iter()
            .find(|e| e.path == path_str && e.mtime_unix == mtime && e.len == len)
        {
            return Some(hit.version.clone());
        }
    }

    let version = super::probe_version(&path_str).await?;

    if let Some((mtime, len)) = stamp {
        let mut state = load_state();
        state.probe_cache.retain(|e| e.path != path_str);
        state.probe_cache.push(ProbeEntry {
            path: path_str,
            mtime_unix: mtime,
            len,
            version: version.clone(),
        });
        if state.probe_cache.len() > PROBE_CACHE_MAX {
            let excess = state.probe_cache.len() - PROBE_CACHE_MAX;
            state.probe_cache.drain(..excess);
        }
        save_state(&state);
    }
    Some(version)
}

/// The official release asset for this build's platform, or `None` where upstream
/// publishes no standalone binary (then the managed feature quietly disables itself
/// and the system binary is used). Names verified against the 2026-07 nightly
/// release. The PyInstaller Linux builds link glibc; musl hosts get the dedicated
/// `musllinux` assets.
pub fn asset_name() -> Option<&'static str> {
    if cfg!(target_os = "macos") {
        Some("yt-dlp_macos")
    } else if cfg!(all(
        target_os = "linux",
        target_env = "gnu",
        target_arch = "x86_64"
    )) {
        Some("yt-dlp_linux")
    } else if cfg!(all(
        target_os = "linux",
        target_env = "gnu",
        target_arch = "aarch64"
    )) {
        Some("yt-dlp_linux_aarch64")
    } else if cfg!(all(
        target_os = "linux",
        target_env = "musl",
        target_arch = "x86_64"
    )) {
        Some("yt-dlp_musllinux")
    } else if cfg!(all(
        target_os = "linux",
        target_env = "musl",
        target_arch = "aarch64"
    )) {
        Some("yt-dlp_musllinux_aarch64")
    } else if cfg!(all(target_os = "windows", target_arch = "x86_64")) {
        Some("yt-dlp.exe")
    } else if cfg!(all(target_os = "windows", target_arch = "aarch64")) {
        Some("yt-dlp_arm64.exe")
    } else if cfg!(all(target_os = "windows", target_arch = "x86")) {
        Some("yt-dlp_x86.exe")
    } else {
        None
    }
}

/// Hard cap on the downloaded binary (current assets are ~40 MiB).
const DOWNLOAD_MAX_BYTES: u64 = 100 * 1024 * 1024;
/// Cap on the SHA2-256SUMS manifest (currently ~1.5 KiB).
const SUMS_MAX_BYTES: usize = 64 * 1024;
/// Cap on the detached GPG signature for `SHA2-256SUMS`.
const SUMS_SIG_MAX_BYTES: usize = 64 * 1024;
const GPG_VERIFY_TIMEOUT: Duration = Duration::from_secs(30);

/// Pinned from https://github.com/yt-dlp/yt-dlp/blob/master/public.key.
/// The current signing identity is:
/// Simon Sawicki (yt-dlp signing key) <contact@grub4k.xyz>
const YTDLP_PUBLIC_KEY: &str = r#"-----BEGIN PGP PUBLIC KEY BLOCK-----

mQINBGP78C4BEAD0rF9zjGPAt0thlt5C1ebzccAVX7Nb1v+eqQjk+WEZdTETVCg3
WAM5ngArlHdm/fZqzUgO+pAYrB60GKeg7ffUDf+S0XFKEZdeRLYeAaqqKhSibVal
DjvOBOztu3W607HLETQAqA7wTPuIt2WqmpL60NIcyr27LxqmgdN3mNvZ2iLO+bP0
nKR/C+PgE9H4ytywDa12zMx6PmZCnVOOOu6XZEFmdUxxdQ9fFDqd9LcBKY2LDOcS
Yo1saY0YWiZWHtzVoZu1kOzjnS5Fjq/yBHJLImDH7pNxHm7s/PnaurpmQFtDFruk
t+2lhDnpKUmGr/I/3IHqH/X+9nPoS4uiqQ5HpblB8BK+4WfpaiEg75LnvuOPfZIP
KYyXa/0A7QojMwgOrD88ozT+VCkKkkJ+ijXZ7gHNjmcBaUdKK7fDIEOYI63Lyc6Q
WkGQTigFffSUXWHDCO9aXNhP3ejqFWgGMtCUsrbkcJkWuWY7q5ARy/05HbSM3K4D
U9eqtnxmiV1WQ8nXuI9JgJQRvh5PTkny5LtxqzcmqvWO9TjHBbrs14BPEO9fcXxK
L/CFBbzXDSvvAgArdqqlMoncQ/yicTlfL6qzJ8EKFiqW14QMTdAn6SuuZTodXCTi
InwoT7WjjuFPKKdvfH1GP4bnqdzTnzLxCSDIEtfyfPsIX+9GI7Jkk/zZjQARAQAB
tDdTaW1vbiBTYXdpY2tpICh5dC1kbHAgc2lnbmluZyBrZXkpIDxjb250YWN0QGdy
dWI0ay54eXo+iQJOBBMBCgA4FiEErAy75oSNaoc0ZK9OV89lkztadYEFAmP78C4C
GwMFCwkIBwIGFQoJCAsCBBYCAwECHgECF4AACgkQV89lkztadYEVqQ//cW7TxhXg
7Xbh2EZQzXml0egn6j8QaV9KzGragMiShrlvTO2zXfLXqyizrFP4AspgjSn/4NrI
8mluom+Yi+qr7DXT4BjQqIM9y3AjwZPdywe912Lxcw52NNoPZCm24I9T7ySc8lmR
FQvZC0w4H/VTNj/2lgJ1dwMflpwvNRiWa5YzcFGlCUeDIPskLx9++AJE+xwU3LYm
jQQsPBqpHHiTBEJzMLl+rfd9Fg4N+QNzpFkTDW3EPerLuvJniSBBwZthqxeAtw4M
UiAXh6JvCc2hJkKCoygRfM281MeolvmsGNyQm+axlB0vyldiPP6BnaRgZlx+l6MU
cPqgHblb7RW5j9lfr6OYL7SceBIHNv0CFrt1OnkGo/tVMwcs8LH3Ae4a7UJlIceL
V54aRxSsZU7w4iX+PB79BWkEsQzwKrUuJVOeL4UDwWajp75OFaUqbS/slDDVXvK5
OIeuth3mA/adjdvgjPxhRQjA3l69rRWIJDrqBSHldmRsnX6cvXTDy8wSXZgy51lP
m4IVLHnCy9m4SaGGoAsfTZS0cC9FgjUIyTyrq9M67wOMpUxnuB0aRZgJE1DsI23E
qdvcSNVlO+39xM/KPWUEh6b83wMn88QeW+DCVGWACQq5N3YdPnAJa50617fGbY6I
gXIoRHXkDqe23PZ/jURYCv0sjVtjPoVC+bg=
=bJkn
-----END PGP PUBLIC KEY BLOCK-----
"#;

/// What a [`check_and_update`] run concluded.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum UpdateOutcome {
    /// A new binary was downloaded, verified, installed, and selected.
    Installed { version: String },
    /// The installed managed binary already matches the channel's latest release.
    AlreadyCurrent,
    /// Nothing was installed (network/checksum failure, unsupported platform,
    /// managed disabled, or another process holds the update lock).
    Unavailable(String),
}

/// Check the channel's latest release and install it when newer than (or different
/// from) what's on disk. Emits [`ToolsEvent`]s along the way; the return value is
/// for the caller's control flow. Never blocks playback — callers run it in a
/// background task (the maintainer, the CLI, or the playback self-heal).
pub async fn check_and_update(
    cfg: &ToolsConfig,
    progress: &(dyn Fn(ToolsEvent) + Sync),
) -> UpdateOutcome {
    let outcome = check_and_update_inner(cfg, progress).await;
    match &outcome {
        UpdateOutcome::Installed { version } => {
            tracing::info!(version = %version, "managed yt-dlp installed");
            progress(ToolsEvent::Installed {
                version: version.clone(),
            });
        }
        UpdateOutcome::AlreadyCurrent => {}
        UpdateOutcome::Unavailable(e) => {
            tracing::warn!(error = %e, "managed yt-dlp update unavailable");
            progress(ToolsEvent::Failed { error: e.clone() });
        }
    }
    outcome
}

async fn check_and_update_inner(
    cfg: &ToolsConfig,
    progress: &(dyn Fn(ToolsEvent) + Sync),
) -> UpdateOutcome {
    use UpdateOutcome::Unavailable;

    if !cfg.managed_enabled() {
        return Unavailable("managed yt-dlp is disabled (tools.ytdlp_managed = false)".into());
    }
    let Some(asset) = asset_name() else {
        return Unavailable("no official standalone yt-dlp build for this platform".into());
    };
    let (Some(dir), Some(dest)) = (tools_dir(), managed_path()) else {
        return Unavailable("no data directory on this platform".into());
    };
    if let Err(e) = safe_fs::ensure_private_dir(&dir) {
        return Unavailable(format!("cannot create {}: {e}", dir.display()));
    }

    // Cross-process (and cross-task) single-flight. Only deduplicates bandwidth:
    // if two updaters *did* race, pid-unique temps + atomic rename still guarantee
    // a valid binary. Held until this function returns.
    let Some(_lock) = acquire_update_lock_observing(&dir).await else {
        super::refresh_selection(cfg).await;
        return Unavailable("another update is already running".into());
    };

    // A previous Windows update may have parked the then-running exe as `.old.exe`.
    #[cfg(windows)]
    {
        let _ = std::fs::remove_file(dest.with_extension("old.exe"));
    }

    let mut state = load_state();
    state.last_attempt_unix = Some(now_unix());

    let channel = cfg.channel();
    let tag = match crate::util::github::latest_release_tag(channel.repo()).await {
        Ok(tag) => tag,
        Err(e) => {
            save_state(&state);
            return Unavailable(e);
        }
    };

    if dest.is_file()
        && state.channel == Some(channel)
        && state.version.as_deref() == Some(tag.as_str())
    {
        match managed_installation_matches(&dest, &state, channel, &tag).await {
            Ok(actual) => {
                state.sha256 = Some(actual.sha256);
                state.installed_mtime_unix = Some(actual.mtime_unix);
                state.installed_len = Some(actual.len);
                state.last_check_unix = Some(now_unix());
                save_state(&state);
                return UpdateOutcome::AlreadyCurrent;
            }
            Err(e) => {
                tracing::warn!(
                    path = %dest.display(),
                    error = %e,
                    "managed yt-dlp metadata mismatch; reinstalling"
                );
            }
        }
    }

    progress(ToolsEvent::Progress {
        channel,
        percent: None,
    });

    let client = match download_client() {
        Ok(c) => c,
        Err(e) => {
            save_state(&state);
            return Unavailable(e);
        }
    };
    // Tag-pinned URLs: the sums and the binary always come from the same release.
    let base = format!(
        "https://github.com/{}/releases/download/{tag}",
        channel.repo()
    );
    let expected_sha = match fetch_expected_sha(&client, &base, asset).await {
        Ok(sha) => sha,
        Err(e) => {
            save_state(&state);
            return Unavailable(e);
        }
    };
    let (tmp, actual_sha) = match download_to_temp(
        &client,
        &format!("{base}/{asset}"),
        &dir,
        channel,
        progress,
    )
    .await
    {
        Ok(pair) => pair,
        Err(e) => {
            save_state(&state);
            return Unavailable(e);
        }
    };
    if actual_sha != expected_sha {
        let _ = std::fs::remove_file(&tmp);
        save_state(&state);
        return Unavailable(format!(
            "checksum mismatch for {asset} {tag} — download discarded"
        ));
    }
    if let Err(e) = install_file(&tmp, &dest) {
        let _ = std::fs::remove_file(&tmp);
        save_state(&state);
        return Unavailable(format!("failed to install {}: {e}", dest.display()));
    }
    let installed = match inspect_binary(&dest).await {
        Ok(actual) => actual,
        Err(e) => {
            save_state(&state);
            return Unavailable(format!("installed managed yt-dlp failed verification: {e}"));
        }
    };
    if installed.sha256 != expected_sha {
        save_state(&state);
        return Unavailable(format!(
            "installed checksum mismatch for {asset} {tag} — metadata not updated"
        ));
    }
    if installed.version != tag {
        save_state(&state);
        return Unavailable(format!(
            "installed yt-dlp reports {}, expected {tag}",
            installed.version
        ));
    }

    state.channel = Some(channel);
    state.version = Some(tag.clone());
    state.sha256 = Some(installed.sha256);
    state.installed_mtime_unix = Some(installed.mtime_unix);
    state.installed_len = Some(installed.len);
    state.last_check_unix = Some(now_unix());
    // The fresh binary re-probes on next selection (its mtime/len changed anyway).
    let dest_str = dest.to_string_lossy().into_owned();
    state.probe_cache.retain(|e| e.path != dest_str);
    save_state(&state);

    super::refresh_selection(cfg).await;
    UpdateOutcome::Installed { version: tag }
}

async fn acquire_update_lock_observing(dir: &Path) -> Option<UpdateLock> {
    if let Some(lock) = UpdateLock::try_acquire(dir) {
        return Some(lock);
    }
    for attempt in 0..6 {
        tokio::time::sleep(Duration::from_millis(250 * (attempt + 1))).await;
        if let Some(lock) = UpdateLock::try_acquire(dir) {
            return Some(lock);
        }
    }
    None
}

/// Don't retry after a failed check within this window (dead network, GitHub down).
const ATTEMPT_BACKOFF: Duration = Duration::from_secs(15 * 60);
/// How often a long-running process re-evaluates (daemon / week-long TUI session);
/// each pass is a no-op while the last successful check is within the channel TTL.
const MAINTAIN_INTERVAL: Duration = Duration::from_secs(6 * 60 * 60);

/// Background maintainer: keeps the managed binary present and fresh without ever
/// blocking startup or playback. Spawned once by the TUI and the daemon; `emit`
/// carries download progress/outcomes (the TUI routes them to the status line, the
/// daemon passes a no-op — `check_and_update` already logs).
pub fn spawn_maintainer<F>(cfg: ToolsConfig, emit: F)
where
    F: Fn(ToolsEvent) + Send + Sync + 'static,
{
    if !cfg.managed_enabled() || asset_name().is_none() {
        return;
    }
    tokio::spawn(async move {
        loop {
            maintain_once(&cfg, &emit).await;
            tokio::time::sleep(MAINTAIN_INTERVAL).await;
        }
    });
}

async fn maintain_once(cfg: &ToolsConfig, emit: &(dyn Fn(ToolsEvent) + Sync)) {
    let source = super::ytdlp_selection().map(|s| s.source);
    // An explicit override is the user's choice — don't spend their bandwidth
    // maintaining a copy that wouldn't be selected.
    if source == Some(super::YtdlpSource::Override) {
        return;
    }
    // macOS: the managed copy is strictly a last resort (see `tools::select` — the
    // standalone build pays a ~10s scan per exec there). Never download or refresh
    // it while any other binary serves.
    if cfg!(target_os = "macos") && matches!(source, Some(super::YtdlpSource::System)) {
        return;
    }

    let state = load_state();
    let now = now_unix();
    let channel = cfg.channel();
    let managed_fresh = installed_managed_path()
        .is_some_and(|path| state_stamp_matches(&path, &state))
        && state.channel == Some(channel)
        && state
            .last_check_unix
            .is_some_and(|c| now.saturating_sub(c) < channel.check_ttl().as_secs());
    if managed_fresh {
        return;
    }
    // A recent attempt with no successful check since means it failed — back off.
    let failed_recently = state.last_attempt_unix.is_some_and(|at| {
        now.saturating_sub(at) < ATTEMPT_BACKOFF.as_secs()
            && state.last_check_unix.is_none_or(|c| c < at)
    });
    if failed_recently {
        return;
    }
    let _ = check_and_update(cfg, emit).await;
}

/// Exclusive advisory lock on `<tools>/.update.lock`. Dropping releases it.
struct UpdateLock {
    _file: std::fs::File,
}

impl UpdateLock {
    fn try_acquire(dir: &Path) -> Option<Self> {
        let file = std::fs::OpenOptions::new()
            .create(true)
            .truncate(false)
            .write(true)
            .open(dir.join(".update.lock"))
            .ok()?;
        file.try_lock().ok()?;
        Some(Self { _file: file })
    }
}

fn download_client() -> Result<reqwest::Client, String> {
    reqwest::Client::builder()
        .user_agent(concat!("ytm-tui/", env!("CARGO_PKG_VERSION")))
        .connect_timeout(Duration::from_secs(15))
        // Whole-transfer ceiling, generous enough for a ~40 MiB asset on a slow link.
        .timeout(Duration::from_secs(10 * 60))
        .build()
        .map_err(|e| format!("http client: {e}"))
}

/// Fetch the release's `SHA2-256SUMS` and extract the entry for `asset`.
async fn fetch_expected_sha(
    client: &reqwest::Client,
    base: &str,
    asset: &str,
) -> Result<String, String> {
    let sums_url = format!("{base}/SHA2-256SUMS");
    let sig_url = format!("{base}/SHA2-256SUMS.sig");
    let sums = fetch_release_file(client, &sums_url, SUMS_MAX_BYTES, "checksum manifest").await?;
    let sig =
        fetch_release_file(client, &sig_url, SUMS_SIG_MAX_BYTES, "checksum signature").await?;
    verify_sha256sums_signature(&sums, &sig)?;
    parse_sha256sums(&String::from_utf8_lossy(&sums), asset)
        .ok_or_else(|| format!("SHA2-256SUMS has no entry for {asset}"))
}

async fn fetch_release_file(
    client: &reqwest::Client,
    url: &str,
    max_bytes: usize,
    label: &str,
) -> Result<Vec<u8>, String> {
    let resp = client
        .get(url)
        .timeout(Duration::from_secs(60))
        .send()
        .await
        .and_then(reqwest::Response::error_for_status)
        .map_err(|e| format!("{label} fetch failed: {e}"))?;
    crate::util::http::read_response_limited(resp, max_bytes)
        .await
        .map_err(|e| format!("{label} fetch failed: {e}"))
}

struct GpgTools {
    gpg: PathBuf,
    gpgv: Option<PathBuf>,
}

fn resolve_gpg_tools() -> Result<GpgTools, String> {
    let Some(gpg) = crate::deps::resolve_on_path("gpg") else {
        return Err("GnuPG `gpg` is required to verify yt-dlp release signatures".to_owned());
    };
    Ok(GpgTools {
        gpg,
        gpgv: crate::deps::resolve_on_path("gpgv"),
    })
}

fn verify_sha256sums_signature(sums: &[u8], signature: &[u8]) -> Result<(), String> {
    let tools = resolve_gpg_tools()?;
    let work = signature_work_dir()?;
    let result = verify_sha256sums_signature_in(&tools, sums, signature, &work);
    let _ = std::fs::remove_dir_all(&work);
    result
}

fn signature_work_dir() -> Result<PathBuf, String> {
    let base = signature_temp_base();
    for attempt in 0..16u8 {
        let dir = base.join(format!(
            "ytm-tui-ytdlp-gpg-{}-{}-{attempt}",
            std::process::id(),
            now_unix()
        ));
        match std::fs::create_dir(&dir) {
            Ok(()) => {
                safe_fs::ensure_private_dir(&dir)
                    .map_err(|e| format!("cannot prepare signature verifier dir: {e}"))?;
                return Ok(dir);
            }
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => continue,
            Err(e) => return Err(format!("cannot prepare signature verifier dir: {e}")),
        }
    }
    Err("cannot allocate signature verifier temp dir".to_owned())
}

fn signature_temp_base() -> PathBuf {
    #[cfg(unix)]
    {
        let tmp = PathBuf::from("/tmp");
        if tmp.is_dir() {
            return tmp;
        }
    }
    std::env::temp_dir()
}

fn verify_sha256sums_signature_in(
    tools: &GpgTools,
    sums: &[u8],
    signature: &[u8],
    dir: &Path,
) -> Result<(), String> {
    let home = dir.join("gnupg-home");
    let sums_path = dir.join("SHA2-256SUMS");
    let sig_path = dir.join("SHA2-256SUMS.sig");
    let key_path = dir.join("yt-dlp-public.key");
    let keyring = dir.join("trustedkeys.kbx");

    std::fs::create_dir(&home).map_err(|e| format!("cannot prepare GPG home: {e}"))?;
    safe_fs::ensure_private_dir(&home).map_err(|e| format!("cannot prepare GPG home: {e}"))?;
    std::fs::write(&sums_path, sums).map_err(|e| format!("cannot write checksum manifest: {e}"))?;
    std::fs::write(&sig_path, signature)
        .map_err(|e| format!("cannot write checksum signature: {e}"))?;
    std::fs::write(&key_path, YTDLP_PUBLIC_KEY.as_bytes())
        .map_err(|e| format!("cannot write yt-dlp public key: {e}"))?;

    let mut import = crate::util::process::std_command(
        &tools.gpg.to_string_lossy(),
        crate::util::process::ProcessProfile::YtDlp,
    );
    import
        .arg("--batch")
        .arg("--quiet")
        .arg("--homedir")
        .arg(&home)
        .arg("--no-default-keyring")
        .arg("--keyring")
        .arg(&keyring)
        .arg("--import")
        .arg(&key_path)
        .stdin(Stdio::null());
    run_verifier(import, "import yt-dlp signing key")?;

    if let Some(gpgv) = &tools.gpgv {
        let mut verify = crate::util::process::std_command(
            &gpgv.to_string_lossy(),
            crate::util::process::ProcessProfile::YtDlp,
        );
        verify
            .arg("--homedir")
            .arg(&home)
            .arg("--keyring")
            .arg(&keyring)
            .arg(&sig_path)
            .arg(&sums_path)
            .stdin(Stdio::null());
        run_verifier(verify, "verify yt-dlp checksum signature")
    } else {
        let mut verify = crate::util::process::std_command(
            &tools.gpg.to_string_lossy(),
            crate::util::process::ProcessProfile::YtDlp,
        );
        verify
            .arg("--batch")
            .arg("--quiet")
            .arg("--homedir")
            .arg(&home)
            .arg("--no-default-keyring")
            .arg("--keyring")
            .arg(&keyring)
            .arg("--verify")
            .arg(&sig_path)
            .arg(&sums_path)
            .stdin(Stdio::null());
        run_verifier(verify, "verify yt-dlp checksum signature")
    }
}

fn run_verifier(mut cmd: std::process::Command, label: &str) -> Result<(), String> {
    use std::io::Read;

    let mut child = cmd
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|e| format!("{label} failed to start: {e}"))?;
    let mut stderr = child.stderr.take();
    let start = std::time::Instant::now();
    let status = loop {
        match child
            .try_wait()
            .map_err(|e| format!("{label} failed to poll: {e}"))?
        {
            Some(status) => break status,
            None if start.elapsed() >= GPG_VERIFY_TIMEOUT => {
                let _ = child.kill();
                let _ = child.wait();
                return Err(format!("{label} timed out"));
            }
            None => std::thread::sleep(Duration::from_millis(20)),
        }
    };
    let mut err = String::new();
    if let Some(mut stderr) = stderr.take() {
        let mut limited = stderr.by_ref().take(8 * 1024 + 1);
        let _ = limited.read_to_string(&mut err);
        if err.len() > 8 * 1024 {
            err.truncate(8 * 1024);
        }
    }
    if status.success() {
        Ok(())
    } else if err.trim().is_empty() {
        Err(format!("{label} failed with {status}"))
    } else {
        Err(format!("{label} failed with {status}: {}", err.trim()))
    }
}

/// `<64-hex>  <filename>` lines → the (lowercased) hash for `asset`.
pub(crate) fn parse_sha256sums(sums: &str, asset: &str) -> Option<String> {
    sums.lines().find_map(|line| {
        let mut parts = line.split_whitespace();
        let hash = parts.next()?;
        let name = parts.next()?;
        (name == asset && hash.len() == 64 && hash.chars().all(|c| c.is_ascii_hexdigit()))
            .then(|| hash.to_ascii_lowercase())
    })
}

/// Stream `url` into `<dir>/.yt-dlp.tmp-<pid>`, hashing as it goes. Returns the temp
/// path and the hex SHA-256. The temp file is removed on any error.
async fn download_to_temp(
    client: &reqwest::Client,
    url: &str,
    dir: &Path,
    channel: YtdlpChannel,
    progress: &(dyn Fn(ToolsEvent) + Sync),
) -> Result<(PathBuf, String), String> {
    let resp = client
        .get(url)
        .send()
        .await
        .and_then(reqwest::Response::error_for_status)
        .map_err(|e| format!("download failed: {e}"))?;
    let total = resp.content_length();
    if let Some(len) = total
        && len > DOWNLOAD_MAX_BYTES
    {
        return Err(format!("download too large: {len} bytes"));
    }

    let tmp = dir.join(format!(".yt-dlp.tmp-{}", std::process::id()));
    let cleanup = |file: Option<tokio::fs::File>| {
        drop(file);
        let _ = std::fs::remove_file(&tmp);
    };
    let mut file = match tokio::fs::File::create(&tmp).await {
        Ok(f) => f,
        Err(e) => return Err(format!("cannot write {}: {e}", tmp.display())),
    };

    let mut hasher = Sha256::new();
    let mut received: u64 = 0;
    let mut last_bucket: Option<u8> = None;
    let mut stream = resp.bytes_stream();
    while let Some(chunk) = stream.next().await {
        let chunk = match chunk {
            Ok(c) => c,
            Err(e) => {
                cleanup(Some(file));
                return Err(format!("download failed: {e}"));
            }
        };
        received = received.saturating_add(chunk.len() as u64);
        if received > DOWNLOAD_MAX_BYTES {
            cleanup(Some(file));
            return Err(format!(
                "download too large: more than {DOWNLOAD_MAX_BYTES} bytes"
            ));
        }
        hasher.update(&chunk);
        if let Err(e) = file.write_all(&chunk).await {
            cleanup(Some(file));
            return Err(format!("cannot write {}: {e}", tmp.display()));
        }
        // Throttle progress to 10% buckets so the status line isn't spammed.
        if let Some(total) = total
            && total > 0
        {
            let bucket = ((received * 100 / total).min(100) as u8) / 10 * 10;
            if last_bucket != Some(bucket) {
                last_bucket = Some(bucket);
                progress(ToolsEvent::Progress {
                    channel,
                    percent: Some(bucket),
                });
            }
        }
    }
    if let Err(e) = file.sync_all().await {
        cleanup(Some(file));
        return Err(format!("cannot write {}: {e}", tmp.display()));
    }
    drop(file);
    Ok((tmp, format!("{:x}", hasher.finalize())))
}

/// Make the verified temp file executable and atomically move it into place.
/// Windows can't replace a running exe, so the live one is parked as `.old.exe`
/// first (cleaned up on the next update; short-lived yt-dlp processes keep running
/// off the renamed handle).
pub(crate) fn install_file(tmp: &Path, dest: &Path) -> std::io::Result<()> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(tmp, std::fs::Permissions::from_mode(0o755))?;
    }
    #[cfg(windows)]
    {
        if dest.exists() {
            let old = dest.with_extension("old.exe");
            retry_file_op(|| {
                let _ = std::fs::remove_file(&old);
                std::fs::rename(dest, &old)
            })?;
        }
        retry_file_op(|| std::fs::rename(tmp, dest))
    }

    #[cfg(not(windows))]
    {
        std::fs::rename(tmp, dest)
    }
}

#[cfg(windows)]
fn retry_file_op(mut op: impl FnMut() -> std::io::Result<()>) -> std::io::Result<()> {
    let mut last = None;
    for attempt in 0..5 {
        match op() {
            Ok(()) => return Ok(()),
            Err(e) => {
                last = Some(e);
                if attempt < 4 {
                    std::thread::sleep(std::time::Duration::from_millis(75 * (attempt + 1)));
                }
            }
        }
    }
    Err(last.unwrap_or_else(|| std::io::Error::other("file operation failed")))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn state_round_trips_and_forward_migrates() {
        let state = ManagedState {
            channel: Some(YtdlpChannel::Nightly),
            version: Some("2026.07.03.234421".to_owned()),
            sha256: Some("ab".repeat(32)),
            installed_mtime_unix: Some(1_780_000_050),
            installed_len: Some(42),
            last_check_unix: Some(1_780_000_000),
            last_attempt_unix: Some(1_780_000_100),
            probe_cache: vec![ProbeEntry {
                path: "/x/yt-dlp".to_owned(),
                mtime_unix: 1,
                len: 2,
                version: "2026.07.03.234421".to_owned(),
            }],
        };
        let json = serde_json::to_string(&state).unwrap();
        let back: ManagedState = serde_json::from_str(&json).unwrap();
        assert_eq!(back.version.as_deref(), Some("2026.07.03.234421"));
        assert_eq!(back.installed_mtime_unix, Some(1_780_000_050));
        assert_eq!(back.installed_len, Some(42));
        assert_eq!(back.probe_cache, state.probe_cache);

        // An empty/older file loads as defaults (never fails).
        let empty: ManagedState = serde_json::from_str("{}").unwrap();
        assert!(empty.version.is_none());
        assert!(empty.probe_cache.is_empty());
    }

    #[test]
    fn asset_exists_for_all_shipped_platforms() {
        // The platforms this app is actually distributed on (brew/scoop/deb builds)
        // must all have an official standalone asset.
        #[cfg(any(
            target_os = "macos",
            all(target_os = "windows", target_arch = "x86_64"),
            all(target_os = "linux", target_arch = "x86_64"),
            all(target_os = "linux", target_arch = "aarch64"),
        ))]
        assert!(asset_name().is_some());
    }

    #[test]
    fn managed_path_lives_under_tools_dir() {
        if let (Some(dir), Some(bin)) = (tools_dir(), managed_path()) {
            assert_eq!(bin.parent(), Some(dir.as_path()));
            assert!(
                bin.file_name()
                    .unwrap()
                    .to_string_lossy()
                    .starts_with("yt-dlp")
            );
        }
    }

    // `parse_tag_from_location` moved to `crate::util::github`; its unit test lives there.

    #[test]
    fn sha256sums_line_parses_for_the_exact_asset() {
        let sums = "\
0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef  yt-dlp\n\
AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA  yt-dlp_linux\n\
bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb  yt-dlp_linux_aarch64\n";
        // Exact name match only — `yt-dlp` must not match `yt-dlp_linux`'s line.
        assert_eq!(
            parse_sha256sums(sums, "yt-dlp_linux").as_deref(),
            Some("aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa"),
            "hash is lowercased"
        );
        assert_eq!(
            parse_sha256sums(sums, "yt-dlp").as_deref(),
            Some("0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef")
        );
        assert_eq!(parse_sha256sums(sums, "yt-dlp_macos"), None);
        // Malformed lines never match.
        assert_eq!(
            parse_sha256sums("nothex  yt-dlp_linux", "yt-dlp_linux"),
            None
        );
        assert_eq!(parse_sha256sums("", "yt-dlp_linux"), None);
    }

    #[test]
    fn pinned_public_key_is_yt_dlp_signing_key() {
        assert!(YTDLP_PUBLIC_KEY.contains("BEGIN PGP PUBLIC KEY BLOCK"));
        assert!(YTDLP_PUBLIC_KEY.contains("tDdTaW1vbiBTYXdpY2tp"));
        assert!(YTDLP_PUBLIC_KEY.contains("dWI0ay54eXo+"));
    }

    #[test]
    fn signature_verifier_rejects_invalid_signature_when_gpg_is_available() {
        let Ok(tools) = resolve_gpg_tools() else {
            return;
        };
        let dir = signature_work_dir().unwrap();

        let err =
            verify_sha256sums_signature_in(&tools, b"not a sums file\n", b"not a signature", &dir)
                .expect_err("invalid detached signature must be rejected");

        assert!(
            err.contains("verify yt-dlp checksum signature"),
            "unexpected error: {err}"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[cfg(unix)]
    #[test]
    fn install_file_sets_exec_bit_and_replaces_atomically() {
        use std::os::unix::fs::PermissionsExt;
        let dir = std::env::temp_dir().join(format!("ytt-install-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let dest = dir.join("yt-dlp");

        // First install.
        let tmp = dir.join(".yt-dlp.tmp-1");
        std::fs::write(&tmp, b"#!/bin/sh\necho one\n").unwrap();
        install_file(&tmp, &dest).unwrap();
        assert!(!tmp.exists(), "temp is consumed by the rename");
        let mode = std::fs::metadata(&dest).unwrap().permissions().mode();
        assert_eq!(mode & 0o111, 0o111, "executable for u/g/o");

        // Overwrite while a reader holds the old file open (mpv/yt-dlp mid-exec):
        // the open handle keeps reading the old inode; the path serves the new one.
        let held = std::fs::File::open(&dest).unwrap();
        let tmp2 = dir.join(".yt-dlp.tmp-2");
        std::fs::write(&tmp2, b"#!/bin/sh\necho two\n").unwrap();
        install_file(&tmp2, &dest).unwrap();
        assert!(
            std::fs::read_to_string(&dest).unwrap().contains("two"),
            "path now serves the new binary"
        );
        drop(held);

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn update_lock_is_exclusive() {
        let dir = std::env::temp_dir().join(format!("ytt-lock-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let first = UpdateLock::try_acquire(&dir);
        assert!(first.is_some());
        assert!(
            UpdateLock::try_acquire(&dir).is_none(),
            "second acquire fails while the first is held"
        );
        drop(first);
        assert!(UpdateLock::try_acquire(&dir).is_some(), "released on drop");
        let _ = std::fs::remove_dir_all(&dir);
    }
}
