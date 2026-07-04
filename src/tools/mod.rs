//! External-tool selection: which `yt-dlp` (and `mpv`) binary this process runs.
//!
//! Why this exists: the app delegates *all* stream resolution to yt-dlp (mpv's
//! ytdl_hook, the prefetch resolver, search, downloads). YouTube churns weekly, so a
//! distro-frozen yt-dlp (Debian stable ships one that is over a year old) eventually
//! resolves **zero** audio formats and every track "fails to play" even though mpv
//! itself is fine. The fix is a *managed* yt-dlp: the app downloads the official
//! standalone binary into its data dir and keeps it fresh (see [`ytdlp`]), then runs
//! whichever candidate is newest.
//!
//! Selection policy (computed once at startup by [`init`], re-computed by
//! [`refresh_selection`] after a managed install):
//! 1. An explicit override — `YTM_YTDLP` env var, then the `tools.ytdlp_path` config —
//!    wins **unconditionally**. An override that could be out-voted by a version
//!    compare would not be an override.
//! 2. Otherwise the newest of {managed binary, system `yt-dlp` on PATH} by
//!    [`compare_versions`]; a tie prefers managed (it's the one we can update, and the
//!    one mpv's ytdl_hook gets pointed at). macOS exception: a usable system binary
//!    always wins, because the standalone build pays a ~10s malware scan per exec
//!    there (see [`select`]) — the managed copy is only the last resort.
//!
//! The winner is published process-wide (an `RwLock`, not a `OnceLock` — the
//! background maintainer re-selects after updating mid-session) and consumed through
//! [`ytdlp_command`] by every exec site, and through [`ytdlp_selection`] by the mpv
//! spawns (`--script-opts=ytdl_hook-ytdl_path=…`).

pub mod cli;
pub mod ytdlp;

use std::cmp::Ordering;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::RwLock;
use std::time::Duration;

use serde::{Deserialize, Serialize};

use crate::config::ToolsConfig;
use crate::util::process;

/// Which release channel the managed yt-dlp tracks. Nightly is the default on
/// upstream's own recommendation: YouTube extractor fixes land there within a day,
/// while stable releases can lag weeks behind a breakage.
#[derive(Debug, Clone, Copy, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum YtdlpChannel {
    #[default]
    Nightly,
    Stable,
}

impl YtdlpChannel {
    /// The GitHub repo whose releases this channel downloads from.
    pub fn repo(self) -> &'static str {
        match self {
            Self::Nightly => "yt-dlp/yt-dlp-nightly-builds",
            Self::Stable => "yt-dlp/yt-dlp",
        }
    }

    /// How stale a successful check may get before the background maintainer looks
    /// again. Nightly moves daily; stable every few weeks.
    pub fn check_ttl(self) -> Duration {
        match self {
            Self::Nightly => Duration::from_secs(48 * 60 * 60),
            Self::Stable => Duration::from_secs(7 * 24 * 60 * 60),
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            Self::Nightly => "nightly",
            Self::Stable => "stable",
        }
    }
}

/// Progress notices from the managed-yt-dlp downloader, surfaced on the TUI status
/// line (throttled to 10% steps) or the CLI/daemon log. The consumer decides display
/// policy — e.g. the TUI warns on `Failed` only when *no* usable binary exists.
#[derive(Debug, Clone)]
pub enum ToolsEvent {
    /// A download is running; `percent` is `None` until the size is known.
    Progress {
        channel: YtdlpChannel,
        percent: Option<u8>,
    },
    /// A new managed binary was installed, verified, and selected.
    Installed { version: String },
    /// The check/download failed (network, checksum, unsupported platform…).
    Failed { error: String },
}

/// Where the active yt-dlp came from (doctor/status display, and whether the mpv
/// spawns need an explicit `ytdl_path`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum YtdlpSource {
    /// `YTM_YTDLP` env var or `tools.ytdlp_path` config — used as-is.
    Override,
    /// The app-managed download in `<data dir>/tools/`.
    Managed,
    /// Whatever `yt-dlp` the system has on PATH.
    System,
}

impl YtdlpSource {
    pub fn label(self) -> &'static str {
        match self {
            Self::Override => "override",
            Self::Managed => "managed",
            Self::System => "system",
        }
    }
}

/// The yt-dlp binary this process resolved to run.
#[derive(Debug, Clone)]
pub struct YtdlpSelection {
    pub path: PathBuf,
    /// Probed `--version` output (e.g. `2026.06.09`, nightly `2026.07.03.234421`).
    /// `None` when the probe failed but the binary was chosen anyway (override, or a
    /// PATH fallback we never verified).
    pub version: Option<String>,
    pub source: YtdlpSource,
}

impl YtdlpSelection {
    /// Whether the mpv spawns should pass `--script-opts=ytdl_hook-ytdl_path=<path>`.
    /// A system selection keeps mpv's default PATH lookup (identical behavior to
    /// before this module existed); managed/override must be pinned explicitly or
    /// mpv would silently run the possibly-stale system binary instead.
    pub fn pin_for_mpv(&self) -> Option<&Path> {
        match self.source {
            YtdlpSource::System => None,
            YtdlpSource::Override | YtdlpSource::Managed => Some(&self.path),
        }
    }
}

/// Process-wide current selection. Written by [`init`]/[`refresh_selection`], read on
/// every yt-dlp exec and mpv spawn. `None` until `init` runs (or when no binary
/// exists anywhere), in which case exec sites fall back to the bare `"yt-dlp"`
/// program name so the pre-existing "is it installed?" error path is preserved.
static SELECTION: RwLock<Option<YtdlpSelection>> = RwLock::new(None);

/// The mpv program override (`YTM_MPV` env > `tools.mpv_path` config), captured at
/// [`init`] so the player/probe call sites don't need config threading. `None` → "mpv".
static MPV_PROGRAM: RwLock<Option<String>> = RwLock::new(None);

/// Resolve and publish the tool selection. Called once at startup (TUI `async_main`
/// and `daemon serve`) *before* the player spawns; cheap on steady state because
/// version probes are cached in the managed-state file keyed by (path, mtime, len).
pub async fn init(cfg: &ToolsConfig) {
    let mpv = cfg.mpv_program();
    *MPV_PROGRAM.write().expect("tools mpv lock poisoned") = (mpv != "mpv").then_some(mpv);
    refresh_selection(cfg).await;
}

/// Re-run the selection (after the maintainer installs/updates the managed binary).
pub async fn refresh_selection(cfg: &ToolsConfig) {
    let sel = select(cfg).await;
    if let Some(s) = &sel {
        tracing::info!(
            path = %s.path.display(),
            version = s.version.as_deref().unwrap_or("?"),
            source = s.source.label(),
            "yt-dlp selected"
        );
    } else {
        tracing::warn!("no usable yt-dlp found (managed, PATH, or override)");
    }
    *SELECTION.write().expect("tools selection lock poisoned") = sel;
}

/// The current selection, if any.
pub fn ytdlp_selection() -> Option<YtdlpSelection> {
    SELECTION
        .read()
        .expect("tools selection lock poisoned")
        .clone()
}

/// Program string for exec sites. Falls back to the bare `"yt-dlp"` name so that
/// with no selection the spawn error remains "is it installed and on PATH?".
pub fn ytdlp_program() -> String {
    ytdlp_selection()
        .map(|s| s.path.to_string_lossy().into_owned())
        .unwrap_or_else(|| "yt-dlp".to_owned())
}

/// The one exec chokepoint: every yt-dlp subprocess in the app builds here so the
/// selected binary (and the YtDlp env profile) applies uniformly.
pub fn ytdlp_command() -> tokio::process::Command {
    process::tokio_command(&ytdlp_program(), process::ProcessProfile::YtDlp)
}

/// The mpv program to spawn (`YTM_MPV` env > `tools.mpv_path` config > `"mpv"`).
/// Reads the value captured at [`init`]; before `init` (or in unit tests) it falls
/// back to the env var so the capability probe can never pick the wrong binary.
pub fn mpv_program() -> String {
    if let Some(p) = MPV_PROGRAM.read().expect("tools mpv lock poisoned").clone() {
        return p;
    }
    match std::env::var("YTM_MPV") {
        Ok(v) if !v.trim().is_empty() => v.trim().to_owned(),
        _ => "mpv".to_owned(),
    }
}

/// Compare two yt-dlp version strings (`2025.04.30`, nightly `2026.07.03.234421`) by
/// numeric segments. Missing segments count as 0, so a nightly built from a stable
/// tag (one extra segment) compares newer than the bare tag. Unparseable segments
/// count as 0 rather than failing — a garbage version loses to any real one.
pub fn compare_versions(a: &str, b: &str) -> Ordering {
    let seg = |s: &str| -> Vec<u64> {
        s.trim()
            .split('.')
            .map(|p| p.trim().parse::<u64>().unwrap_or(0))
            .collect()
    };
    let (va, vb) = (seg(a), seg(b));
    for i in 0..va.len().max(vb.len()) {
        let x = va.get(i).copied().unwrap_or(0);
        let y = vb.get(i).copied().unwrap_or(0);
        match x.cmp(&y) {
            Ordering::Equal => {}
            other => return other,
        }
    }
    Ordering::Equal
}

/// Minimum spacing between playback-failure-triggered update checks (the TUI and
/// daemon self-heal). One broken video must not turn every skip into a network
/// round-trip; the startup maintainer already covers routine freshness.
pub const HEAL_COOLDOWN: Duration = Duration::from_secs(30 * 60);

/// Whether an mpv playback error means "stream resolution failed" (ytdl_hook's
/// yt-dlp call produced nothing, so mpv probed the raw watch page and gave up) as
/// opposed to a network/HTTP error. This is the signature of a stale yt-dlp and
/// gates the self-heal (update, then retry the same track once).
pub fn looks_like_extraction_failure(player_error: &str) -> bool {
    let e = player_error.to_ascii_lowercase();
    e.contains("unrecognized file format") || e.contains("failed to recognize file format")
}

/// Probe `<program> --version`. Returns the first stdout line when it looks like a
/// yt-dlp version (digits and dots only) — a shim that prints usage or an HTML error
/// page must not be mistaken for a working binary. The timeout is generous because a
/// PyInstaller onefile binary self-extracts on every run (seconds on slow disks, and
/// macOS adds a per-exec Gatekeeper/XProtect scan of ~10s); the probe cache makes
/// this a one-time cost per binary.
pub(crate) async fn probe_version(program: &str) -> Option<String> {
    let mut cmd = process::tokio_command(program, process::ProcessProfile::YtDlp);
    cmd.arg("--version")
        .stdin(Stdio::null())
        .stderr(Stdio::null());
    let out = process::tokio_output_limited(cmd, Duration::from_secs(30), 4096)
        .await
        .ok()?;
    if !out.status.success() {
        return None;
    }
    let stdout = String::from_utf8_lossy(&out.stdout);
    let line = stdout.lines().next()?.trim().to_owned();
    (!line.is_empty() && line.chars().all(|c| c.is_ascii_digit() || c == '.') && line.contains('.'))
        .then_some(line)
}

/// Compute the selection per the policy in the module docs.
async fn select(cfg: &ToolsConfig) -> Option<YtdlpSelection> {
    // 1. Explicit override: used even if the probe fails (the user asked for it;
    //    a broken override should fail loudly at exec, not silently fall back).
    if let Some(path) = cfg.ytdlp_override() {
        let version = ytdlp::cached_probe(&path).await;
        return Some(YtdlpSelection {
            path,
            version,
            source: YtdlpSource::Override,
        });
    }

    let managed = ytdlp::installed_managed_path();
    let system = crate::deps::resolve_on_path("yt-dlp");

    // macOS exception to newest-wins: the standalone PyInstaller binary pays a
    // ~10s Gatekeeper/XProtect scan on EVERY exec (measured; it re-extracts to a
    // fresh temp dir each run, so the verdict never caches), which would poison
    // every search/resolve/download call. brew keeps yt-dlp current there anyway,
    // so a *usable* system binary always wins and the managed copy is only the
    // nothing-else-works fallback. An explicit override (above) still wins.
    if cfg!(target_os = "macos")
        && let Some(path) = &system
        && let Some(version) = ytdlp::cached_probe(path).await
    {
        return Some(YtdlpSelection {
            path: path.clone(),
            version: Some(version),
            source: YtdlpSource::System,
        });
    }

    select_candidates(managed, system).await
}

/// Newest-wins between the managed and system candidates. A candidate whose version
/// probe fails is unusable and drops out (e.g. a managed glibc binary on a musl
/// host); if both probes fail there is no selection.
async fn select_candidates(
    managed: Option<PathBuf>,
    system: Option<PathBuf>,
) -> Option<YtdlpSelection> {
    let (managed_ver, system_ver) = tokio::join!(
        async {
            match &managed {
                Some(p) => ytdlp::cached_probe(p).await,
                None => None,
            }
        },
        async {
            match &system {
                Some(p) => ytdlp::cached_probe(p).await,
                None => None,
            }
        },
    );

    let managed = managed.zip(managed_ver);
    let system = system.zip(system_ver);
    match (managed, system) {
        (Some((mp, mv)), Some((sp, sv))) => {
            // Tie prefers managed: it's the copy we can keep fresh.
            if compare_versions(&sv, &mv) == Ordering::Greater {
                Some(YtdlpSelection {
                    path: sp,
                    version: Some(sv),
                    source: YtdlpSource::System,
                })
            } else {
                Some(YtdlpSelection {
                    path: mp,
                    version: Some(mv),
                    source: YtdlpSource::Managed,
                })
            }
        }
        (Some((mp, mv)), None) => Some(YtdlpSelection {
            path: mp,
            version: Some(mv),
            source: YtdlpSource::Managed,
        }),
        (None, Some((sp, sv))) => Some(YtdlpSelection {
            path: sp,
            version: Some(sv),
            source: YtdlpSource::System,
        }),
        (None, None) => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn version_compare_orders_numerically_not_lexically() {
        use Ordering::*;
        let table = [
            ("2025.04.30", "2026.06.09", Less),
            ("2026.06.09", "2026.06.09", Equal),
            ("2026.06.10", "2026.06.09", Greater),
            // A nightly (4 segments) is newer than the stable tag it was cut from.
            ("2026.06.09.233015", "2026.06.09", Greater),
            ("2026.06.09", "2026.06.09.233015", Less),
            // Numeric compare: .9 < .10 (lexicographic would say Greater).
            ("2026.06.9", "2026.06.10", Less),
            // Garbage loses to anything real.
            ("garbage", "2020.01.01", Less),
        ];
        for (a, b, want) in table {
            assert_eq!(compare_versions(a, b), want, "{a} vs {b}");
        }
    }

    #[test]
    fn extraction_failure_classification() {
        // What the reducer actually sees (ipc.rs wraps mpv's file_error).
        assert!(looks_like_extraction_failure(
            "mpv could not play this track (unrecognized file format)"
        ));
        assert!(looks_like_extraction_failure(
            "Failed to recognize file format."
        ));
        // Network/HTTP/permission failures must NOT trigger a yt-dlp update.
        assert!(!looks_like_extraction_failure(
            "mpv could not play this track (HTTP error 403 Forbidden)"
        ));
        assert!(!looks_like_extraction_failure(
            "mpv could not play this track"
        ));
        assert!(!looks_like_extraction_failure("connection refused"));
    }

    #[test]
    fn channel_defaults_and_repos() {
        assert_eq!(YtdlpChannel::default(), YtdlpChannel::Nightly);
        assert_eq!(YtdlpChannel::Nightly.repo(), "yt-dlp/yt-dlp-nightly-builds");
        assert_eq!(YtdlpChannel::Stable.repo(), "yt-dlp/yt-dlp");
        assert!(YtdlpChannel::Nightly.check_ttl() < YtdlpChannel::Stable.check_ttl());
    }

    #[cfg(unix)]
    mod selection {
        use super::super::*;

        /// Point the probe cache at a per-process temp dir exactly once — these tests
        /// must never write into the user's real `<data>/tools/ytdlp.json`. Shared by
        /// all selection tests (entries are keyed by absolute path, so no conflicts).
        fn isolate_tools_dir() {
            static ONCE: std::sync::OnceLock<()> = std::sync::OnceLock::new();
            ONCE.get_or_init(|| {
                let dir =
                    std::env::temp_dir().join(format!("ytt-tools-state-{}", std::process::id()));
                std::fs::create_dir_all(&dir).unwrap();
                // SAFETY: test-only; set once before any concurrent reader can care.
                unsafe { std::env::set_var("YTM_TOOLS_DIR", &dir) };
            });
        }

        /// A fake yt-dlp that prints `version` for `--version` (same trick as the
        /// resolver's tests).
        fn write_fake_ytdlp(dir: &std::path::Path, name: &str, version: &str) -> PathBuf {
            use std::os::unix::fs::PermissionsExt;
            let path = dir.join(name);
            std::fs::write(&path, format!("#!/bin/sh\necho '{version}'\n")).unwrap();
            std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
            path
        }

        fn tmp_dir(tag: &str) -> PathBuf {
            let dir = std::env::temp_dir().join(format!("ytt-tools-{tag}-{}", std::process::id()));
            std::fs::create_dir_all(&dir).unwrap();
            dir
        }

        #[tokio::test]
        async fn newest_wins_between_managed_and_system() {
            isolate_tools_dir();
            let dir = tmp_dir("newest");
            let managed = write_fake_ytdlp(&dir, "managed", "2026.07.01");
            let system = write_fake_ytdlp(&dir, "system", "2025.04.30");

            let sel = select_candidates(Some(managed.clone()), Some(system.clone()))
                .await
                .unwrap();
            assert_eq!(sel.source, YtdlpSource::Managed);
            assert_eq!(sel.path, managed);
            assert_eq!(sel.version.as_deref(), Some("2026.07.01"));

            // System newer (rolling distro / brew) → system wins, no forced managed.
            let newer_system = write_fake_ytdlp(&dir, "system2", "2027.01.01");
            let sel = select_candidates(Some(managed), Some(newer_system.clone()))
                .await
                .unwrap();
            assert_eq!(sel.source, YtdlpSource::System);
            assert_eq!(sel.path, newer_system);

            let _ = std::fs::remove_dir_all(&dir);
        }

        #[tokio::test]
        async fn equal_versions_prefer_managed() {
            isolate_tools_dir();
            let dir = tmp_dir("tie");
            let managed = write_fake_ytdlp(&dir, "managed", "2026.07.01");
            let system = write_fake_ytdlp(&dir, "system", "2026.07.01");
            let sel = select_candidates(Some(managed.clone()), Some(system))
                .await
                .unwrap();
            assert_eq!(sel.source, YtdlpSource::Managed);
            assert_eq!(sel.path, managed);
            let _ = std::fs::remove_dir_all(&dir);
        }

        #[tokio::test]
        async fn broken_candidate_drops_out() {
            isolate_tools_dir();
            use std::os::unix::fs::PermissionsExt;
            let dir = tmp_dir("broken");
            // A managed binary that can't run (exec format error stand-in: exits 1).
            let broken = dir.join("managed");
            std::fs::write(&broken, "#!/bin/sh\nexit 1\n").unwrap();
            std::fs::set_permissions(&broken, std::fs::Permissions::from_mode(0o755)).unwrap();
            let system = write_fake_ytdlp(&dir, "system", "2025.04.30");

            let sel = select_candidates(Some(broken), Some(system.clone()))
                .await
                .unwrap();
            assert_eq!(sel.source, YtdlpSource::System, "broken managed is skipped");
            assert_eq!(sel.path, system);

            // Nothing usable at all → no selection.
            assert!(select_candidates(None, None).await.is_none());
            let _ = std::fs::remove_dir_all(&dir);
        }

        #[tokio::test]
        async fn version_probe_rejects_non_version_output() {
            isolate_tools_dir();
            let dir = tmp_dir("probe");
            let chatty = write_fake_ytdlp(&dir, "chatty", "usage: yt-dlp [OPTIONS]");
            assert_eq!(probe_version(&chatty.to_string_lossy()).await, None);
            let real = write_fake_ytdlp(&dir, "real", "2026.07.03.234421");
            assert_eq!(
                probe_version(&real.to_string_lossy()).await.as_deref(),
                Some("2026.07.03.234421")
            );
            let _ = std::fs::remove_dir_all(&dir);
        }

        #[test]
        fn pin_for_mpv_only_for_managed_and_override() {
            let sel = |source| YtdlpSelection {
                path: PathBuf::from("/x/yt-dlp"),
                version: None,
                source,
            };
            assert!(sel(YtdlpSource::Managed).pin_for_mpv().is_some());
            assert!(sel(YtdlpSource::Override).pin_for_mpv().is_some());
            assert!(sel(YtdlpSource::System).pin_for_mpv().is_none());
        }
    }
}
