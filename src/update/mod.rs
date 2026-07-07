//! App self-update *awareness* (not self-install): detect when the running build is
//! behind the latest GitHub release and tell the user how to upgrade for their install
//! method.
//!
//! Flow (see [`spawn_update_check`]): a background task resolves the latest stable release
//! tag for `Ochichan/Yututui` via [`crate::util::github::latest_release_tag`] (the
//! `releases/latest` redirect — no API rate limit), compares it to
//! `env!("CARGO_PKG_VERSION")`, and emits an [`UpdateEvent::Checked`] the reducer turns
//! into an About-window notice + brand dot + one-time toast. State (last check, last seen
//! tag, last toasted tag) persists to `<data>/update.json`, mirroring the yt-dlp
//! [`crate::tools::ytdlp::ManagedState`] pattern, so we check at most once a day and toast a
//! given new version only once.
//!
//! Nothing here ever downloads or replaces the executable — [`update_instructions`] only
//! reports the correct command for the detected channel. Development builds and the
//! `update_check_enabled = false` opt-out skip the network entirely.

pub mod cli;

use std::path::{Path, PathBuf};
use std::time::Duration;

use serde::{Deserialize, Serialize};

use crate::t;
use crate::util::safe_fs;

/// Canonical releases page — the click target and CLI hint for every install method.
pub const RELEASES_URL: &str = "https://github.com/Ochichan/Yututui/releases/latest";

/// The repo whose releases we track.
pub const REPO: &str = "Ochichan/Yututui";

/// How stale a successful check may get before we look again. Stable releases land weeks
/// apart, so a day is plenty and keeps launches quiet.
const CHECK_TTL: Duration = Duration::from_secs(24 * 60 * 60);
/// After a failed attempt, wait this long before retrying (dead-network backoff).
const ATTEMPT_BACKOFF: Duration = Duration::from_secs(60 * 60);
/// Small head start so the check never competes with startup or the first paint.
const POST_START_DELAY: Duration = Duration::from_secs(4);

/// How the running binary reached this machine — determines the upgrade instructions.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum InstallMethod {
    Homebrew,
    Scoop,
    Aur,
    Nix,
    Cargo,
    /// `install.sh` (macOS/Linux → `~/.local/bin`).
    InstallerUnix,
    /// `install.ps1` (Windows → `%LOCALAPPDATA%\Programs\ytt`).
    InstallerWindows,
    /// A macOS `.app` bundle (from the release tarball / dragged to /Applications).
    MacAppBundle,
    Winget,
    /// Running from `target/{debug,release}` — never nagged.
    Development,
    Unknown,
}

impl InstallMethod {
    /// A short, stable identifier for CLI/doctor output.
    pub fn label(self) -> &'static str {
        match self {
            Self::Homebrew => "Homebrew",
            Self::Scoop => "Scoop",
            Self::Aur => "AUR",
            Self::Nix => "Nix",
            Self::Cargo => "cargo",
            Self::InstallerUnix => "install.sh",
            Self::InstallerWindows => "install.ps1",
            Self::MacAppBundle => "macOS .app",
            Self::Winget => "winget",
            Self::Development => "development build",
            Self::Unknown => "unknown",
        }
    }

    /// Parse a `label()` (case-insensitive) — used only by the debug `YTM_INSTALL_METHOD`
    /// override so a dev build can preview any channel's notice.
    fn from_label(raw: &str) -> Option<Self> {
        Some(match raw.trim().to_ascii_lowercase().as_str() {
            "homebrew" | "brew" => Self::Homebrew,
            "scoop" => Self::Scoop,
            "aur" => Self::Aur,
            "nix" => Self::Nix,
            "cargo" => Self::Cargo,
            "install.sh" | "installer-unix" | "unix" => Self::InstallerUnix,
            "install.ps1" | "installer-windows" | "windows" => Self::InstallerWindows,
            "macapp" | "app" | "macos-app" => Self::MacAppBundle,
            "winget" => Self::Winget,
            "development" | "dev" => Self::Development,
            "unknown" => Self::Unknown,
            _ => return None,
        })
    }
}

/// The three host families whose install-path conventions differ.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Os {
    Macos,
    Windows,
    Linux,
}

fn current_os() -> Os {
    if cfg!(windows) {
        Os::Windows
    } else if cfg!(target_os = "macos") {
        Os::Macos
    } else {
        Os::Linux
    }
}

/// The upgrade guidance for a method: a copy-pasteable `command` when the channel has a
/// stable self-serve one, plus a short human `note` (both bilingual via [`t!`]).
pub struct UpdateInstructions {
    pub command: Option<&'static str>,
    pub note: &'static str,
}

/// Detect the install method of the running binary. Best-effort: resolves symlinks
/// (Homebrew `bin`→`Cellar`) and matches well-known install-dir signatures. Unknown when
/// nothing matches — the notice then just points at the Releases page.
pub fn detect_install_method() -> InstallMethod {
    let exe = std::env::current_exe()
        .ok()
        .map(|p| p.canonicalize().unwrap_or(p));
    detect_from(exe.as_deref(), current_os())
}

/// The pure core of [`detect_install_method`], with the exe path and OS injected so it is
/// unit-testable. Signatures mirror the per-OS candidate dirs probed in
/// `desktop/launch.rs`. First match wins; the haystack is lowercased and `/`-normalized so
/// the same needles work on Windows.
fn detect_from(exe: Option<&Path>, os: Os) -> InstallMethod {
    let Some(path) = exe else {
        return InstallMethod::Unknown;
    };
    let hay = path
        .to_string_lossy()
        .replace('\\', "/")
        .to_ascii_lowercase();
    let has = |needle: &str| hay.contains(needle);

    // Dev build first — running from the build tree should never nag.
    if has("/target/debug/") || has("/target/release/") {
        return InstallMethod::Development;
    }
    if has("/nix/store/") {
        return InstallMethod::Nix;
    }
    // Homebrew: the `bin` symlink resolves into `Cellar`; works on Linux too.
    if has("/cellar/") || has("/opt/homebrew/") || has("/linuxbrew/") {
        return InstallMethod::Homebrew;
    }
    if os == Os::Macos && has("/usr/local/") {
        return InstallMethod::Homebrew; // Intel Homebrew prefix
    }
    if has("/scoop/apps/") || has("/scoop/shims/") {
        return InstallMethod::Scoop;
    }
    if os == Os::Windows && has("/microsoft/winget/") {
        return InstallMethod::Winget;
    }
    if has(".app/contents/macos/") {
        return InstallMethod::MacAppBundle;
    }
    if has("/.cargo/bin/") {
        return InstallMethod::Cargo;
    }
    if has("/.local/bin/") {
        return InstallMethod::InstallerUnix;
    }
    if os == Os::Windows && has("/programs/ytt/") {
        return InstallMethod::InstallerWindows;
    }
    if os == Os::Linux && has("/usr/bin/") {
        return InstallMethod::Aur;
    }
    InstallMethod::Unknown
}

/// The upgrade command + note for an install method (localized).
pub fn update_instructions(method: InstallMethod) -> UpdateInstructions {
    let (command, note) = match method {
        InstallMethod::Homebrew => (
            Some("brew update && brew upgrade yututui"),
            t!("Update with Homebrew.", "Homebrew로 업데이트하세요."),
        ),
        InstallMethod::Scoop => (
            Some("scoop update yututui"),
            t!("Update with Scoop.", "Scoop으로 업데이트하세요."),
        ),
        InstallMethod::Aur => (
            Some("yay -Syu yututui-bin"),
            t!("Update with your AUR helper.", "AUR 헬퍼로 업데이트하세요."),
        ),
        InstallMethod::Nix => (
            Some("nix profile upgrade yututui"),
            t!("Update with Nix.", "Nix로 업데이트하세요."),
        ),
        InstallMethod::Cargo => (
            Some("cargo install yututui --force"),
            t!("Reinstall with cargo.", "cargo로 재설치하세요."),
        ),
        InstallMethod::InstallerUnix => (
            None,
            t!(
                "Re-run the install script to update.",
                "설치 스크립트를 다시 실행해 업데이트하세요."
            ),
        ),
        InstallMethod::InstallerWindows => (
            Some("scoop update yututui"),
            t!(
                "Re-run install.ps1, or update with Scoop.",
                "install.ps1을 다시 실행하거나 Scoop으로 업데이트하세요."
            ),
        ),
        InstallMethod::MacAppBundle => (
            None,
            t!(
                "Download the latest macOS build from Releases.",
                "Releases에서 최신 macOS 빌드를 받으세요."
            ),
        ),
        InstallMethod::Winget => (
            Some("winget upgrade yututui"),
            t!("Update with winget.", "winget으로 업데이트하세요."),
        ),
        InstallMethod::Development => (None, t!("Development build.", "개발 빌드입니다.")),
        InstallMethod::Unknown => (
            None,
            t!(
                "Get the latest build from Releases.",
                "Releases에서 최신 빌드를 받으세요."
            ),
        ),
    };
    UpdateInstructions { command, note }
}

/// The result of a check, handed to the reducer.
#[derive(Debug, Clone)]
pub struct UpdateStatus {
    /// The running version (`env!("CARGO_PKG_VERSION")`).
    pub current: String,
    /// The latest release tag resolved from GitHub (as published, may carry a leading `v`).
    pub latest: String,
    /// Whether `latest` is strictly newer than `current`.
    pub available: bool,
    /// True only on the first sighting of this newer tag — gates the one-time toast.
    pub first_seen: bool,
    /// How this binary was installed.
    pub method: InstallMethod,
}

impl UpdateStatus {
    /// `latest` without a leading `v`/`V` — for display next to `current`.
    pub fn latest_display(&self) -> &str {
        self.latest.trim_start_matches(['v', 'V'])
    }
}

#[derive(Debug, Clone)]
pub enum UpdateEvent {
    Checked(UpdateStatus),
}

/// Persisted check bookkeeping. Every field `#[serde(default)]` so the file
/// forward-migrates and a corrupt one loads as default (worst case: one extra check).
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default)]
struct UpdateState {
    /// Unix seconds of the last *successful* check.
    last_check_unix: Option<u64>,
    /// Unix seconds of the last check *attempt* (dead-network backoff).
    last_attempt_unix: Option<u64>,
    /// The latest tag last resolved from GitHub.
    latest_tag: Option<String>,
    /// The newest tag we have already shown a toast for (one toast per version).
    toasted_tag: Option<String>,
}

fn state_path() -> Option<PathBuf> {
    crate::paths::data_dir().map(|d| d.join("update.json"))
}

fn load_state() -> UpdateState {
    match state_path() {
        Some(p) => safe_fs::load_json_or_default(&p),
        None => UpdateState::default(),
    }
}

fn save_state(state: &UpdateState) {
    let Some(path) = state_path() else { return };
    if let Some(dir) = path.parent() {
        let _ = safe_fs::ensure_private_dir(dir);
    }
    if let Err(e) = safe_fs::write_private_atomic_json(&path, state) {
        tracing::warn!(error = %e, "failed to persist update-check state");
    }
}

fn now_unix() -> u64 {
    crate::tools::ytdlp::now_unix()
}

/// `MAJOR.MINOR.PATCH` from a release tag: strips a leading `v`/`V`, drops any
/// `-prerelease`/`+build` suffix, and parses the first three dotted numbers (missing
/// minor/patch default to 0). `None` on anything non-numeric.
fn parse_semver(tag: &str) -> Option<(u64, u64, u64)> {
    let core = tag.trim().trim_start_matches(['v', 'V']);
    let core = core.split(['-', '+']).next().unwrap_or(core);
    let mut it = core.split('.');
    let major = it.next()?.parse().ok()?;
    let minor = it.next().unwrap_or("0").parse().ok()?;
    let patch = it.next().unwrap_or("0").parse().ok()?;
    Some((major, minor, patch))
}

/// True when `latest` is a strictly newer version than `current`. Anything unparseable →
/// false, so we never nag on a garbage tag.
pub fn is_newer(latest: &str, current: &str) -> bool {
    match (parse_semver(latest), parse_semver(current)) {
        (Some(l), Some(c)) => l > c,
        _ => false,
    }
}

/// Debug-only latest-tag override for visual verification without a real newer release.
fn forced_latest_tag() -> Option<String> {
    if !cfg!(debug_assertions) {
        return None;
    }
    std::env::var("YTM_UPDATE_FORCE")
        .ok()
        .map(|s| s.trim().to_owned())
        .filter(|s| !s.is_empty())
}

/// Debug-only install-method override so a dev build can preview any channel's notice.
fn method_override(detected: InstallMethod) -> InstallMethod {
    if !cfg!(debug_assertions) {
        return detected;
    }
    std::env::var("YTM_INSTALL_METHOD")
        .ok()
        .and_then(|s| InstallMethod::from_label(&s))
        .unwrap_or(detected)
}

/// The install method to act on: [`detect_install_method`] plus the debug override. The
/// About card, `ytt update`, and the background check all resolve through here so they agree.
pub fn resolved_install_method() -> InstallMethod {
    method_override(detect_install_method())
}

/// Resolve the latest release for `REPO` once — the exact source `ytt update` and the
/// background check share. Honors the debug force override.
pub async fn resolve_latest() -> Result<String, String> {
    if let Some(forced) = forced_latest_tag() {
        return Ok(forced);
    }
    crate::util::github::latest_release_tag(REPO).await
}

/// The last-known-newer release tag from persisted state, without any network — `Some(tag)`
/// only when a cached latest is strictly newer than the running build. `ytt doctor` uses this
/// to report update availability offline.
pub fn cached_newer_tag() -> Option<String> {
    let latest = load_state().latest_tag?;
    is_newer(&latest, env!("CARGO_PKG_VERSION")).then_some(latest)
}

/// Spawn the background update check. No-ops (and makes zero network calls) when
/// `enabled` is false or this is a development build. Never blocks startup: it sleeps
/// briefly, honors a 24h success TTL and 1h failure backoff, persists what it learns, and
/// emits [`UpdateEvent::Checked`] with the outcome.
pub fn spawn_update_check<F>(current: &'static str, enabled: bool, emit: F)
where
    F: Fn(UpdateEvent) + Send + Sync + 'static,
{
    if !enabled {
        return;
    }
    let method = resolved_install_method();
    // Development builds don't nag — unless a debug override is forcing a preview.
    if method == InstallMethod::Development && forced_latest_tag().is_none() {
        return;
    }

    tokio::spawn(async move {
        tokio::time::sleep(POST_START_DELAY).await;
        let mut state = load_state();
        let now = now_unix();

        let within_ttl = state
            .last_check_unix
            .is_some_and(|c| now.saturating_sub(c) < CHECK_TTL.as_secs());
        let backoff_active = state.last_attempt_unix.is_some_and(|a| {
            now.saturating_sub(a) < ATTEMPT_BACKOFF.as_secs()
                && state.last_check_unix.is_none_or(|c| c < a)
        });

        let latest = if within_ttl {
            // Fresh enough — reuse the cached tag (still surfaces a known update).
            match state.latest_tag.clone() {
                Some(t) => t,
                None => return,
            }
        } else if backoff_active {
            // A recent attempt failed — show any previously-known update, don't re-hit net.
            match state.latest_tag.clone() {
                Some(t) => t,
                None => return,
            }
        } else {
            state.last_attempt_unix = Some(now);
            match resolve_latest().await {
                Ok(tag) => {
                    state.last_check_unix = Some(now);
                    state.latest_tag = Some(tag.clone());
                    tag
                }
                Err(e) => {
                    tracing::warn!(error = %e, "app update check failed");
                    save_state(&state);
                    return;
                }
            }
        };

        let available = is_newer(&latest, current);
        let first_seen = available && state.toasted_tag.as_deref() != Some(latest.as_str());
        if first_seen {
            state.toasted_tag = Some(latest.clone());
        }
        save_state(&state);

        emit(UpdateEvent::Checked(UpdateStatus {
            current: current.to_owned(),
            latest,
            available,
            first_seen,
            method,
        }));
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn semver_parses_and_orders() {
        assert_eq!(parse_semver("v1.6.1"), Some((1, 6, 1)));
        assert_eq!(parse_semver("1.6.1"), Some((1, 6, 1)));
        assert_eq!(parse_semver("v2"), Some((2, 0, 0)));
        assert_eq!(parse_semver("1.7"), Some((1, 7, 0)));
        assert_eq!(parse_semver("v1.7.0-rc1"), Some((1, 7, 0)));
        assert_eq!(parse_semver("v1.7.0+build.5"), Some((1, 7, 0)));
        assert_eq!(parse_semver("nightly"), None);
        assert_eq!(parse_semver(""), None);
    }

    #[test]
    fn is_newer_only_when_strictly_ahead() {
        assert!(is_newer("v1.7.0", "1.6.1"));
        assert!(is_newer("1.6.2", "1.6.1"));
        assert!(is_newer("v2.0.0", "1.9.9"));
        assert!(!is_newer("v1.6.1", "1.6.1"));
        assert!(!is_newer("v1.6.0", "1.6.1"));
        assert!(!is_newer("garbage", "1.6.1"));
        assert!(!is_newer("v1.7.0", "garbage"));
    }

    #[test]
    fn detect_maps_representative_paths() {
        use InstallMethod::*;
        let cases = [
            ("/home/u/proj/target/debug/ytt", Os::Linux, Development),
            ("/home/u/proj/target/release/ytt", Os::Linux, Development),
            ("/nix/store/abc-yututui-1.6.1/bin/ytt", Os::Linux, Nix),
            (
                "/opt/homebrew/Cellar/yututui/1.6.1/bin/ytt",
                Os::Macos,
                Homebrew,
            ),
            ("/usr/local/bin/ytt", Os::Macos, Homebrew),
            ("/home/linuxbrew/.linuxbrew/bin/ytt", Os::Linux, Homebrew),
            (
                "C:/Users/u/scoop/apps/yututui/current/ytt.exe",
                Os::Windows,
                Scoop,
            ),
            ("C:/Users/u/scoop/shims/ytt.exe", Os::Windows, Scoop),
            (
                "C:/Users/u/AppData/Local/Microsoft/WinGet/Packages/x/ytt.exe",
                Os::Windows,
                Winget,
            ),
            (
                "/Applications/YuTuTui!.app/Contents/MacOS/ytt",
                Os::Macos,
                MacAppBundle,
            ),
            ("/home/u/.cargo/bin/ytt", Os::Linux, Cargo),
            ("/home/u/.local/bin/ytt", Os::Linux, InstallerUnix),
            (
                "C:/Users/u/AppData/Local/Programs/ytt/ytt.exe",
                Os::Windows,
                InstallerWindows,
            ),
            ("/usr/bin/ytt", Os::Linux, Aur),
            ("/some/weird/place/ytt", Os::Linux, Unknown),
        ];
        for (path, os, want) in cases {
            assert_eq!(
                detect_from(Some(Path::new(path)), os),
                want,
                "path {path} on {os:?}"
            );
        }
        assert_eq!(detect_from(None, Os::Linux), Unknown);
    }

    #[test]
    fn instructions_cover_every_method() {
        use InstallMethod::*;
        let _g = crate::i18n::lock_for_test();
        for m in [
            Homebrew,
            Scoop,
            Aur,
            Nix,
            Cargo,
            InstallerUnix,
            InstallerWindows,
            MacAppBundle,
            Winget,
            Development,
            Unknown,
        ] {
            let ins = update_instructions(m);
            assert!(!ins.note.is_empty(), "{}", m.label());
        }
        // Package-manager methods carry a runnable command; download-only ones don't.
        assert_eq!(
            update_instructions(Homebrew).command,
            Some("brew update && brew upgrade yututui")
        );
        assert_eq!(update_instructions(MacAppBundle).command, None);
        assert_eq!(update_instructions(InstallerUnix).command, None);
    }

    #[test]
    fn state_round_trips_and_forward_migrates() {
        let s = UpdateState {
            last_check_unix: Some(100),
            latest_tag: Some("v1.7.0".to_owned()),
            toasted_tag: Some("v1.7.0".to_owned()),
            ..Default::default()
        };
        let json = serde_json::to_string(&s).unwrap();
        let back: UpdateState = serde_json::from_str(&json).unwrap();
        assert_eq!(back.latest_tag.as_deref(), Some("v1.7.0"));
        // Missing fields default (forward-migration from an older/smaller file).
        let sparse: UpdateState = serde_json::from_str("{}").unwrap();
        assert_eq!(sparse.latest_tag, None);
        assert_eq!(sparse.last_check_unix, None);
    }
}
