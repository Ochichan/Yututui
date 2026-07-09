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
//! 2. Otherwise the newest of {enabled managed binary, system `yt-dlp` on PATH} by
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
use std::time::{Duration, Instant};

use anyhow::{Context, bail};
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

/// Where the active yt-dlp came from (doctor/status display, and the source label
/// for the exact path pinned into mpv's `ytdl_path`).
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
    /// The exact yt-dlp path mpv spawns should receive through
    /// `--script-opts-append=ytdl_hook-ytdl_path=<path>`.
    ///
    /// Even a system selection is pinned. Leaving mpv to do its own PATH lookup can
    /// diverge from the path this process probed when Scoop shims, Python Scripts,
    /// or PATH ordering change between selection and player spawn.
    pub fn pin_for_mpv(&self) -> Option<&Path> {
        Some(&self.path)
    }
}

/// Process-wide current selection. Written by [`init`]/[`refresh_selection`], read on
/// every yt-dlp exec and mpv spawn. `None` until `init` runs (or when no binary
/// exists anywhere), in which case exec sites fall back to the bare `"yt-dlp"`
/// program name so the pre-existing "is it installed?" error path is preserved.
static SELECTION: RwLock<Option<YtdlpSelection>> = RwLock::new(None);
static SELECTION_ERROR: RwLock<Option<String>> = RwLock::new(None);

/// The mpv program override (`YTM_MPV` env > `tools.mpv_path` config), captured at
/// [`init`] so the player/probe call sites don't need config threading. `None` → "mpv".
static MPV_PROGRAM: RwLock<Option<String>> = RwLock::new(None);

/// Resolve and publish the tool selection. Called once at startup (TUI `async_main`
/// and `daemon serve`) *before* the player spawns; cheap on steady state because
/// version probes are cached in the managed-state file keyed by (path, mtime, len).
pub async fn init(cfg: &ToolsConfig) {
    *MPV_PROGRAM.write().expect("tools mpv lock poisoned") =
        cfg.mpv_override().and_then(normalize_mpv_override);
    refresh_selection(cfg).await;
}

/// Re-run the selection (after the maintainer installs/updates the managed binary).
pub async fn refresh_selection(cfg: &ToolsConfig) {
    let (sel, err) = match select(cfg).await {
        Ok(sel) => (sel, None),
        Err(e) => (None, Some(e)),
    };
    match (&sel, &err) {
        (Some(s), None) => {
            tracing::info!(
                path = %s.path.display(),
                version = s.version.as_deref().unwrap_or("?"),
                source = s.source.label(),
                "yt-dlp selected"
            );
        }
        (None, Some(e)) => {
            tracing::warn!(error = %e, "no usable yt-dlp selected");
        }
        (None, None) => {
            tracing::warn!("no usable yt-dlp found (managed, PATH, or override)");
        }
        (Some(_), Some(_)) => unreachable!("selection cannot have both value and error"),
    }
    *SELECTION.write().expect("tools selection lock poisoned") = sel;
    *SELECTION_ERROR
        .write()
        .expect("tools selection-error lock poisoned") = err;
}

/// The current selection, if any.
pub fn ytdlp_selection() -> Option<YtdlpSelection> {
    SELECTION
        .read()
        .expect("tools selection lock poisoned")
        .clone()
}

pub fn ytdlp_selection_error() -> Option<String> {
    SELECTION_ERROR
        .read()
        .expect("tools selection-error lock poisoned")
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
    ytdlp_command_for(&ytdlp_program())
}

/// Build a yt-dlp subprocess for a specific program path/name. Test seams and
/// direct exec paths use this so they receive the same environment and runtime
/// arguments as the process-wide selection.
pub(crate) fn ytdlp_command_for(program: &str) -> tokio::process::Command {
    let mut cmd = process::tokio_command(program, process::ProcessProfile::YtDlp);
    // Every yt-dlp invocation is transient (download / resolve / probe) and must never outlive
    // the future driving it: if that future is dropped on error or app shutdown, SIGKILL the
    // child so a stuck yt-dlp can't linger (mpv already does the same at its spawn site). This
    // reaps yt-dlp itself; a short-lived ffmpeg grandchild started for post-processing is not
    // in the same kill, but the download timeout path already accepts that same bound.
    cmd.kill_on_drop(true);
    // App-owned invocations are machine-parsed (`-g` first line, `--dump-single-json`,
    // `--print after_move:filepath`), so the user's ~/.config/yt-dlp/config — default format,
    // forced prints, plugins — must not leak into them. mpv's ytdl_hook path is unaffected
    // (playback keeps the user's yt-dlp behavior); `YTM_YTDLP_USER_CONFIG=1` opts back in.
    if !respect_user_ytdlp_config(std::env::var("YTM_YTDLP_USER_CONFIG").ok().as_deref()) {
        cmd.arg("--ignore-config");
    }
    append_ytdlp_js_runtime_args(&mut cmd);
    cmd
}

/// Pure seam for the `YTM_YTDLP_USER_CONFIG` opt-out so it is testable without toggling
/// process env (racy under the parallel test runner). Only an exact `1` (trimmed) opts in.
fn respect_user_ytdlp_config(env_val: Option<&str>) -> bool {
    env_val.map(str::trim) == Some("1")
}

pub(crate) fn classify_ytdlp_failure(stderr: &str) -> Option<&'static str> {
    let stderr = stderr.to_ascii_lowercase();
    for (needle, class) in [
        (
            "sign in to confirm",
            "YouTube bot check — sign-in cookies may be needed",
        ),
        (
            "not a bot",
            "YouTube bot check — sign-in cookies may be needed",
        ),
        ("sign in", "sign-in required"),
        ("login required", "sign-in required"),
        ("private video", "sign-in required"),
        ("members-only", "sign-in required"),
        ("confirm your age", "sign-in required"),
        ("not available in your country", "geo-blocked"),
        ("geo restrict", "geo-blocked"),
        (
            "requested format is not available",
            "no playable format (yt-dlp may be stale)",
        ),
        (
            "no video formats",
            "no playable format (yt-dlp may be stale)",
        ),
        ("js runtime", "JS runtime problem (install deno or node)"),
        (
            "javascript runtime",
            "JS runtime problem (install deno or node)",
        ),
        ("nsig", "JS runtime problem (install deno or node)"),
        // "error 429", not bare "429": video ids / byte counts embed the digits routinely.
        ("error 429", "rate-limited by YouTube"),
        ("too many requests", "rate-limited by YouTube"),
        ("urlopen error", "network error"),
        ("timed out", "network error"),
        ("connection re", "network error"),
        ("temporary failure in name resolution", "network error"),
        ("ffmpeg", "ffmpeg/post-processing failure"),
        ("postprocess", "ffmpeg/post-processing failure"),
    ] {
        if stderr.contains(needle) {
            return Some(class);
        }
    }
    None
}

/// Coarse class for playback errors reported by mpv. This is intentionally smaller than
/// yt-dlp stderr classification: by the time the reducer sees a `PlayerEvent::Error`,
/// it only has mpv's `file_error` text.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PlaybackFailureClass {
    /// ytdl_hook produced no playable stream and mpv tried to parse the watch page itself.
    Extraction,
    /// YouTube/CDN rejected the stream with HTTP 403.
    Http403,
    /// YouTube/CDN rate-limited the stream (HTTP 429 / too many requests).
    RateLimited,
    /// Transport/DNS/timeout style failure.
    Network,
    /// Anything else.
    Unknown,
}

/// Classify the mpv playback error string surfaced by `player::ipc`.
pub fn classify_playback_failure(player_error: &str) -> PlaybackFailureClass {
    let e = player_error.to_ascii_lowercase();
    if e.contains("http error 403") || e.contains("error 403") || e.contains("403 forbidden") {
        return PlaybackFailureClass::Http403;
    }
    if e.contains("http error 429")
        || e.contains("error 429")
        || e.contains("429 too many requests")
        || e.contains("too many requests")
        || e.contains("rate limit")
        || e.contains("rate-limit")
    {
        return PlaybackFailureClass::RateLimited;
    }
    if e.contains("unrecognized file format") || e.contains("failed to recognize file format") {
        return PlaybackFailureClass::Extraction;
    }
    if e.contains("connection refused")
        || e.contains("connection reset")
        || e.contains("connection timed out")
        || e.contains("timed out")
        || e.contains("timeout")
        || e.contains("temporary failure in name resolution")
        || e.contains("could not resolve")
        || e.contains("name or service not known")
        || e.contains("network is unreachable")
        || e.contains("no route to host")
        || e.contains("http error 500")
        || e.contains("http error 502")
        || e.contains("http error 503")
        || e.contains("http error 504")
    {
        return PlaybackFailureClass::Network;
    }
    PlaybackFailureClass::Unknown
}

pub fn playback_failure_actionable_error(
    class: PlaybackFailureClass,
    player_error: &str,
) -> String {
    match class {
        PlaybackFailureClass::Extraction => format!(
            "stream resolution failed; run `ytt tools update`, then `ytt doctor --verbose`: {player_error}"
        ),
        PlaybackFailureClass::Http403 | PlaybackFailureClass::RateLimited => format!(
            "YouTube rejected the stream; run `ytt doctor --verbose`, check cookies and JS runtime: {player_error}"
        ),
        PlaybackFailureClass::Network => {
            format!("network error while opening stream: {player_error}")
        }
        PlaybackFailureClass::Unknown => player_error.to_owned(),
    }
}

pub(crate) fn ytdlp_failure_detail(stderr_tail: &[u8]) -> String {
    let stderr = String::from_utf8_lossy(stderr_tail);
    let class = classify_ytdlp_failure(&stderr);
    let line = stderr
        .lines()
        .rev()
        .map(str::trim)
        .find(|line| !line.is_empty())
        .map(crate::util::sanitize::sanitize_error_text)
        .map(|line| truncate_chars(&line, 200))
        .filter(|line| !line.is_empty());

    match (class, line) {
        (Some(class), Some(line)) => format!(" ({class}; {line})"),
        (Some(class), None) => format!(" ({class})"),
        (None, Some(line)) => format!(" ({line})"),
        (None, None) => String::new(),
    }
}

fn truncate_chars(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        return s.to_owned();
    }
    s.chars().take(max).collect()
}

pub(crate) async fn run_ytdlp_json(
    mut cmd: tokio::process::Command,
    timeout: Duration,
    stdout_max: usize,
    what: &str,
) -> anyhow::Result<serde_json::Value> {
    cmd.stdin(Stdio::null());
    let out =
        process::tokio_output_limited(cmd, process::ProcessProfile::YtDlp, timeout, stdout_max)
            .await
            .with_context(|| {
                format!("failed to run yt-dlp {what} — is it installed and on PATH?")
            })?;
    if !out.status.success() {
        bail!(
            "yt-dlp {what} exited with status {}{}",
            out.status,
            ytdlp_failure_detail(&out.stderr_tail)
        );
    }
    serde_json::from_slice(&out.stdout)
        .with_context(|| format!("could not parse yt-dlp {what} output"))
}

/// The mpv program to spawn (`YTM_MPV` env > `tools.mpv_path` config > `"mpv"`).
/// Reads the value captured at [`init`]; before `init` (or in unit tests) it falls
/// back to the env var so the capability probe can never pick the wrong binary.
pub fn mpv_program() -> String {
    if let Some(p) = MPV_PROGRAM.read().expect("tools mpv lock poisoned").clone() {
        return p;
    }
    match std::env::var("YTM_MPV") {
        Ok(v) if !v.trim().is_empty() => {
            normalize_mpv_override(PathBuf::from(v.trim())).unwrap_or_else(|| "mpv".to_owned())
        }
        _ => "mpv".to_owned(),
    }
}

/// A JavaScript runtime yt-dlp can use for YouTube nsig/sig deciphering. Modern yt-dlp needs one
/// (Deno is the built-in default; the others must be named via `--js-runtimes`), or YouTube
/// extraction degrades toward failure. See <https://github.com/yt-dlp/yt-dlp/issues/15012>.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum JsRuntime {
    Deno,
    Node,
    Bun,
    QuickJs,
}

impl JsRuntime {
    /// The bare `--js-runtimes` token yt-dlp expects. `None` for Deno — it's auto-detected on
    /// `PATH` with no flag, so nothing needs to be passed.
    pub fn flag_value(self) -> Option<&'static str> {
        match self {
            JsRuntime::Deno => None,
            JsRuntime::Node => Some("node"),
            JsRuntime::Bun => Some("bun"),
            JsRuntime::QuickJs => Some("quickjs"),
        }
    }

    /// Human label for diagnostics.
    pub fn label(self) -> &'static str {
        match self {
            JsRuntime::Deno => "deno",
            JsRuntime::Node => "node",
            JsRuntime::Bun => "bun",
            JsRuntime::QuickJs => "quickjs",
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct JsRuntimeProbe {
    pub runtime: JsRuntime,
    pub path: PathBuf,
    pub version: Option<String>,
    pub supported: bool,
    pub reason: Option<&'static str>,
}

#[derive(Clone)]
struct JsRuntimeCache {
    checked: Instant,
    probes: Vec<JsRuntimeProbe>,
}

#[derive(Clone)]
struct JsRuntimeSelectionCache {
    checked: Instant,
    runtime: Option<JsRuntime>,
}

static JS_RUNTIME_CACHE: RwLock<Option<JsRuntimeCache>> = RwLock::new(None);
static JS_RUNTIME_SELECTION_CACHE: RwLock<Option<JsRuntimeSelectionCache>> = RwLock::new(None);
const JS_RUNTIME_CACHE_TTL: Duration = Duration::from_secs(60);
const JS_RUNTIME_PROBE_TIMEOUT: Duration = Duration::from_secs(2);
const JS_RUNTIME_PROBE_STDOUT_MAX: usize = 4096;

/// The best *supported* JS runtime for yt-dlp found on `PATH`, in yt-dlp's own preference order
/// (Deno first — it's the default and needs no flag). `None` when none is installed or only
/// unsupported versions are present. Version probes are cached briefly so frequent search/resolve
/// calls do not run `node --version`/`deno --version` for every yt-dlp subprocess.
pub fn detect_js_runtime() -> Option<JsRuntime> {
    if let Some(cache) = JS_RUNTIME_SELECTION_CACHE
        .read()
        .expect("js runtime selection cache lock poisoned")
        .clone()
        && cache.checked.elapsed() < JS_RUNTIME_CACHE_TTL
    {
        return cache.runtime;
    }

    if let Some(cache) = JS_RUNTIME_CACHE
        .read()
        .expect("js runtime cache lock poisoned")
        .clone()
        && cache.checked.elapsed() < JS_RUNTIME_CACHE_TTL
    {
        let runtime = select_supported_js_runtime(&cache.probes);
        *JS_RUNTIME_SELECTION_CACHE
            .write()
            .expect("js runtime selection cache lock poisoned") = Some(JsRuntimeSelectionCache {
            checked: Instant::now(),
            runtime,
        });
        return runtime;
    }

    let runtime = probe_supported_js_runtime_uncached();
    *JS_RUNTIME_SELECTION_CACHE
        .write()
        .expect("js runtime selection cache lock poisoned") = Some(JsRuntimeSelectionCache {
        checked: Instant::now(),
        runtime,
    });
    runtime
}

fn select_supported_js_runtime(probes: &[JsRuntimeProbe]) -> Option<JsRuntime> {
    probes
        .iter()
        .find(|probe| probe.supported)
        .map(|probe| probe.runtime)
}

pub fn js_runtime_diagnostics() -> Vec<JsRuntimeProbe> {
    if let Some(cache) = JS_RUNTIME_CACHE
        .read()
        .expect("js runtime cache lock poisoned")
        .clone()
        && cache.checked.elapsed() < JS_RUNTIME_CACHE_TTL
    {
        return cache.probes;
    }

    let probes = probe_js_runtimes_uncached();
    *JS_RUNTIME_CACHE
        .write()
        .expect("js runtime cache lock poisoned") = Some(JsRuntimeCache {
        checked: Instant::now(),
        probes: probes.clone(),
    });
    probes
}

fn js_runtime_candidates() -> [(&'static str, JsRuntime); 4] {
    [
        ("deno", JsRuntime::Deno),
        ("node", JsRuntime::Node),
        ("bun", JsRuntime::Bun),
        ("qjs", JsRuntime::QuickJs),
    ]
}

fn probe_js_runtimes_uncached() -> Vec<JsRuntimeProbe> {
    js_runtime_candidates()
        .into_iter()
        .filter_map(|(bin, runtime)| {
            crate::deps::resolve_on_path(bin).map(|path| probe_one_js_runtime(runtime, path))
        })
        .collect()
}

fn probe_supported_js_runtime_uncached() -> Option<JsRuntime> {
    for (bin, runtime) in js_runtime_candidates() {
        let Some(path) = crate::deps::resolve_on_path(bin) else {
            continue;
        };
        let probe = probe_one_js_runtime(runtime, path);
        if probe.supported {
            return Some(probe.runtime);
        }
    }
    None
}

fn probe_one_js_runtime(runtime: JsRuntime, path: PathBuf) -> JsRuntimeProbe {
    let path_str = path.to_string_lossy();
    let mut cmd = process::std_command(path_str.as_ref(), process::ProcessProfile::YtDlp);
    cmd.arg("--version");
    let stdout = process::std_output_limited(
        cmd,
        process::ProcessProfile::YtDlp,
        JS_RUNTIME_PROBE_TIMEOUT,
        JS_RUNTIME_PROBE_STDOUT_MAX,
    )
    .ok()
    .filter(|out| out.status.success())
    .map(|out| String::from_utf8_lossy(&out.stdout).into_owned())
    .unwrap_or_default();

    let version = parse_js_runtime_version(runtime, &stdout);
    let (supported, reason) = js_runtime_support(runtime, version.as_deref(), &stdout);
    JsRuntimeProbe {
        runtime,
        path,
        version,
        supported,
        reason,
    }
}

fn parse_js_runtime_version(_runtime: JsRuntime, output: &str) -> Option<String> {
    first_version_like(output)
}

fn first_version_like(text: &str) -> Option<String> {
    let chars: Vec<(usize, char)> = text.char_indices().collect();
    for (idx, ch) in chars {
        if !ch.is_ascii_digit() {
            continue;
        }
        let version: String = text[idx..]
            .chars()
            .take_while(|c| c.is_ascii_digit() || *c == '.' || *c == '-')
            .collect();
        if version.contains('.') || version.contains('-') {
            return Some(version.replace('-', "."));
        }
    }
    None
}

fn js_runtime_support(
    runtime: JsRuntime,
    version: Option<&str>,
    raw_output: &str,
) -> (bool, Option<&'static str>) {
    match runtime {
        JsRuntime::Deno => min_version_support(version, "2.3.0", "requires deno >= 2.3.0"),
        JsRuntime::Node => min_version_support(version, "22.0.0", "requires node >= 22.0.0"),
        JsRuntime::Bun => match version {
            Some(v) if compare_versions(v, "1.2.11") == Ordering::Less => {
                (false, Some("requires bun >= 1.2.11"))
            }
            Some(v) if compare_versions(v, "1.3.14") == Ordering::Greater => {
                (false, Some("requires bun <= 1.3.14"))
            }
            Some(_) => (true, None),
            None => (false, Some("could not read bun version")),
        },
        JsRuntime::QuickJs => {
            let lower = raw_output.to_ascii_lowercase();
            if lower.contains("quickjs-ng") || lower.contains("qjs-ng") {
                return (true, None);
            }
            match version {
                Some(v) if compare_versions(v, "2023.12.9") == Ordering::Less => {
                    (false, Some("requires quickjs >= 2023.12.9"))
                }
                Some(_) | None => (true, None),
            }
        }
    }
}

fn min_version_support(
    version: Option<&str>,
    minimum: &str,
    reason: &'static str,
) -> (bool, Option<&'static str>) {
    match version {
        Some(v) if compare_versions(v, minimum) != Ordering::Less => (true, None),
        Some(_) => (false, Some(reason)),
        None => (false, Some("could not read runtime version")),
    }
}

/// The `--js-runtimes` token to hand yt-dlp so it uses an installed *non-default* runtime for
/// YouTube nsig solving. `None` when Deno is present (auto-used, no flag) or nothing is found
/// (nothing to pass; yt-dlp warns/degrades on its own). Passed as a bare name — the yt-dlp
/// subprocess inherits our `PATH`, so no absolute path or quoting is needed.
pub fn js_runtimes_flag() -> Option<&'static str> {
    js_runtimes_flag_for(detect_js_runtime())
}

fn js_runtimes_flag_for(runtime: Option<JsRuntime>) -> Option<&'static str> {
    runtime.and_then(JsRuntime::flag_value)
}

fn ytdlp_js_runtime_args_for(runtime: Option<JsRuntime>) -> Option<[&'static str; 2]> {
    js_runtimes_flag_for(runtime).map(|rt| ["--js-runtimes", rt])
}

pub(crate) fn append_ytdlp_js_runtime_args(cmd: &mut tokio::process::Command) {
    if let Some(args) = ytdlp_js_runtime_args_for(detect_js_runtime()) {
        cmd.args(args);
    }
}

pub(crate) fn append_ytdlp_cookie_args(cmd: &mut tokio::process::Command, cookies: Option<&Path>) {
    if let Some(path) = cookies {
        cmd.arg("--cookies").arg(path);
    }
}

/// Player client used when minting YouTube stream URLs under cookie auth.
///
/// Cookie-authenticated yt-dlp defaults (notably `TVHTML5`) often return googlevideo
/// URLs that ffmpeg/mpv then open with HTTP 403 — so playlist streaming skips while
/// the same track still downloads (yt-dlp fetches the bytes itself). `web_safari`
/// keeps authenticated catalog access and yields CDN URLs mpv can open.
pub const YOUTUBE_STREAM_PLAYER_CLIENT: &str = "web_safari";

pub fn youtube_stream_extractor_args() -> String {
    format!("youtube:player_client={YOUTUBE_STREAM_PLAYER_CLIENT}")
}

/// Pin the stream player client on a direct yt-dlp invocation (prefetch resolver).
pub(crate) fn append_ytdlp_youtube_stream_extractor_args(cmd: &mut tokio::process::Command) {
    cmd.arg("--extractor-args")
        .arg(youtube_stream_extractor_args());
}

fn mpv_ytdl_js_runtime_arg_for(runtime: Option<JsRuntime>) -> Option<String> {
    js_runtimes_flag_for(runtime).map(|rt| format!("--ytdl-raw-options-append=js-runtimes={rt}"))
}

pub fn mpv_ytdl_js_runtime_arg() -> Option<String> {
    mpv_ytdl_js_runtime_arg_for(detect_js_runtime())
}

pub fn mpv_ytdl_raw_option_args(cookies: Option<&Path>) -> Vec<String> {
    let mut args = Vec::new();
    if let Some(path) = cookies {
        args.push(format!(
            "--ytdl-raw-options-append=cookies={}",
            path.display()
        ));
        // Only with cookies: anonymous playback keeps yt-dlp's default client chain.
        args.push(format!(
            "--ytdl-raw-options-append=extractor-args={}",
            youtube_stream_extractor_args()
        ));
    }
    if let Some(arg) = mpv_ytdl_js_runtime_arg() {
        args.push(arg);
    }
    args
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
    classify_playback_failure(player_error) == PlaybackFailureClass::Extraction
}

/// Probe `<program> --version`. Returns the first stdout line when it looks like a
/// yt-dlp version (digits and dots only) — a shim that prints usage or an HTML error
/// page must not be mistaken for a working binary. The timeout is generous because a
/// PyInstaller onefile binary self-extracts on every run (seconds on slow disks, and
/// macOS adds a per-exec Gatekeeper/XProtect scan of ~10s); the probe cache makes
/// this a one-time cost per binary.
pub(crate) async fn probe_version(program: &str) -> Option<String> {
    let mut cmd = process::tokio_command(program, process::ProcessProfile::YtDlp);
    cmd.arg("--ignore-config")
        .arg("--version")
        .stdin(Stdio::null());
    let out = process::tokio_output_limited(
        cmd,
        process::ProcessProfile::YtDlp,
        Duration::from_secs(30),
        4096,
    )
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
async fn select(cfg: &ToolsConfig) -> Result<Option<YtdlpSelection>, String> {
    // 1. Explicit override: wins over all candidates, but must be a real yt-dlp.
    //    A broken override fails loudly instead of silently falling back.
    if let Some(path) = cfg.ytdlp_override() {
        let path = normalize_program_override("yt-dlp", path)?;
        let version = ytdlp::cached_probe(&path)
            .await
            .ok_or_else(|| format!("yt-dlp override did not run: {}", path.display()))?;
        return Ok(Some(YtdlpSelection {
            path,
            version: Some(version),
            source: YtdlpSource::Override,
        }));
    }

    let managed = managed_candidate(cfg);
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
        return Ok(Some(YtdlpSelection {
            path: path.clone(),
            version: Some(version),
            source: YtdlpSource::System,
        }));
    }

    Ok(select_candidates(managed, system).await)
}

fn normalize_mpv_override(path: PathBuf) -> Option<String> {
    match normalize_program_override("mpv", path) {
        Ok(path) => Some(path.to_string_lossy().into_owned()),
        Err(e) => {
            tracing::warn!(error = %e, "mpv override ignored");
            None
        }
    }
}

fn normalize_program_override(program: &str, path: PathBuf) -> Result<PathBuf, String> {
    let raw = path.to_string_lossy();
    if raw.chars().any(|c| c == '\0' || c == '\n' || c == '\r') {
        return Err(format!(
            "{program} override contains a line break or NUL byte"
        ));
    }
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        return Err(format!("{program} override is empty"));
    }
    let unquoted = trimmed
        .strip_prefix('"')
        .and_then(|s| s.strip_suffix('"'))
        .or_else(|| {
            trimmed
                .strip_prefix('\'')
                .and_then(|s| s.strip_suffix('\''))
        })
        .unwrap_or(trimmed);
    let candidate = PathBuf::from(unquoted);
    let resolved = if candidate.is_absolute() || candidate.components().count() > 1 {
        candidate
    } else {
        crate::deps::resolve_on_path(unquoted)
            .ok_or_else(|| format!("{program} override `{unquoted}` was not found on PATH"))?
    };
    let resolved_str = resolved.to_string_lossy();
    if !crate::deps::on_path(resolved_str.as_ref()) {
        return Err(format!(
            "{program} override is not an executable file: {}",
            resolved.display()
        ));
    }
    Ok(resolved)
}

fn managed_candidate(cfg: &ToolsConfig) -> Option<PathBuf> {
    if cfg.managed_enabled() {
        ytdlp::installed_managed_path()
    } else {
        None
    }
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
    fn playback_failure_classification_splits_recovery_policies() {
        use PlaybackFailureClass as C;

        let cases = [
            (
                "mpv could not play this track (unrecognized file format)",
                C::Extraction,
            ),
            (
                "mpv could not play this track (HTTP error 403 Forbidden)",
                C::Http403,
            ),
            (
                "mpv could not play this track (HTTP Error 429: Too Many Requests)",
                C::RateLimited,
            ),
            (
                "mpv could not play this track (connection timed out)",
                C::Network,
            ),
            ("mpv could not play this track", C::Unknown),
        ];

        for (error, expected) in cases {
            assert_eq!(classify_playback_failure(error), expected, "{error}");
        }
    }

    #[test]
    fn ytdlp_user_config_opt_in_is_exact_trimmed_one() {
        assert!(!respect_user_ytdlp_config(None));
        assert!(respect_user_ytdlp_config(Some("1")));
        assert!(respect_user_ytdlp_config(Some(" 1 ")));
        assert!(!respect_user_ytdlp_config(Some("0")));
        assert!(!respect_user_ytdlp_config(Some("")));
        assert!(!respect_user_ytdlp_config(Some("true")));
    }

    #[test]
    fn ytdlp_failure_classification_matches_expected_classes() {
        let cases = [
            (
                "ERROR: [youtube] abc: Sign in to confirm you're not a bot",
                "YouTube bot check — sign-in cookies may be needed",
            ),
            (
                "ERROR: login required to watch this video",
                "sign-in required",
            ),
            (
                "ERROR: This video is not available in your country",
                "geo-blocked",
            ),
            (
                "ERROR: requested format is not available",
                "no playable format (yt-dlp may be stale)",
            ),
            (
                "WARNING: nsig extraction failed: JS runtime not found",
                "JS runtime problem (install deno or node)",
            ),
            (
                "ERROR: HTTP Error 429: Too Many Requests",
                "rate-limited by YouTube",
            ),
            ("ERROR: urlopen error timed out", "network error"),
            (
                "ERROR: ffmpeg postprocess failed while converting",
                "ffmpeg/post-processing failure",
            ),
        ];
        for (stderr, expected) in cases {
            assert_eq!(classify_ytdlp_failure(stderr), Some(expected), "{stderr}");
        }
        assert_eq!(classify_ytdlp_failure("ERROR: something surprising"), None);
    }

    #[test]
    fn ytdlp_failure_detail_formats_class_and_last_line() {
        let detail =
            ytdlp_failure_detail(b"WARNING: ignored\nERROR: HTTP Error 429: Too Many Requests\n");
        assert_eq!(
            detail,
            " (rate-limited by YouTube; ERROR: HTTP Error 429: Too Many Requests)"
        );

        assert_eq!(
            ytdlp_failure_detail(b"ERROR: completely new failure\n"),
            " (ERROR: completely new failure)"
        );
        assert_eq!(
            ytdlp_failure_detail(b"ERROR: Sign in to confirm you're not a bot\n"),
            " (YouTube bot check \u{2014} sign-in cookies may be needed; ERROR: Sign in to confirm you're not a bot)"
        );
        assert_eq!(ytdlp_failure_detail(b"\n \n"), "");

        let long = format!("ERROR: {}", "x".repeat(250));
        let detail = ytdlp_failure_detail(long.as_bytes());
        assert_eq!(detail.len(), " (".len() + 200 + ")".len());
    }

    #[test]
    fn channel_defaults_and_repos() {
        assert_eq!(YtdlpChannel::default(), YtdlpChannel::Nightly);
        assert_eq!(YtdlpChannel::Nightly.repo(), "yt-dlp/yt-dlp-nightly-builds");
        assert_eq!(YtdlpChannel::Stable.repo(), "yt-dlp/yt-dlp");
        assert!(YtdlpChannel::Nightly.check_ttl() < YtdlpChannel::Stable.check_ttl());
    }

    #[test]
    fn js_runtime_args_are_consistent_for_direct_ytdlp_and_mpv_hook() {
        assert_eq!(js_runtimes_flag_for(Some(JsRuntime::Deno)), None);
        assert_eq!(
            ytdlp_js_runtime_args_for(Some(JsRuntime::Node)),
            Some(["--js-runtimes", "node"])
        );
        assert_eq!(
            ytdlp_js_runtime_args_for(Some(JsRuntime::Bun)),
            Some(["--js-runtimes", "bun"])
        );
        assert_eq!(
            ytdlp_js_runtime_args_for(Some(JsRuntime::QuickJs)),
            Some(["--js-runtimes", "quickjs"])
        );
        assert_eq!(ytdlp_js_runtime_args_for(None), None);
        assert_eq!(
            mpv_ytdl_js_runtime_arg_for(Some(JsRuntime::Node)).as_deref(),
            Some("--ytdl-raw-options-append=js-runtimes=node")
        );
        assert_eq!(mpv_ytdl_js_runtime_arg_for(Some(JsRuntime::Deno)), None);
    }

    #[test]
    fn mpv_ytdl_raw_options_pin_web_safari_only_with_cookies() {
        let with_cookies = mpv_ytdl_raw_option_args(Some(Path::new("/tmp/cookies.txt")));
        assert!(
            with_cookies
                .iter()
                .any(|a| a.contains("cookies=/tmp/cookies.txt")),
            "{with_cookies:?}"
        );
        assert!(
            with_cookies.iter().any(|a| {
                a.contains("extractor-args=") && a.contains("player_client=web_safari")
            }),
            "cookie auth must pin web_safari to avoid TVHTML5 CDN 403s: {with_cookies:?}"
        );

        let anonymous = mpv_ytdl_raw_option_args(None);
        assert!(
            anonymous
                .iter()
                .all(|a| !a.contains("extractor-args=") && !a.contains("cookies=")),
            "anonymous playback must not force a player client: {anonymous:?}"
        );
    }

    #[test]
    fn js_runtime_versions_are_parsed_from_common_binaries() {
        assert_eq!(
            parse_js_runtime_version(JsRuntime::Deno, "deno 2.3.0\nv8 14.0\n").as_deref(),
            Some("2.3.0")
        );
        assert_eq!(
            parse_js_runtime_version(JsRuntime::Node, "v22.11.0\n").as_deref(),
            Some("22.11.0")
        );
        assert_eq!(
            parse_js_runtime_version(JsRuntime::Bun, "1.3.14\n").as_deref(),
            Some("1.3.14")
        );
        assert_eq!(
            parse_js_runtime_version(JsRuntime::QuickJs, "QuickJS version 2025-04-26\n").as_deref(),
            Some("2025.04.26")
        );
    }

    #[test]
    fn js_runtime_support_enforces_current_yt_dlp_bounds() {
        assert_eq!(
            js_runtime_support(JsRuntime::Deno, Some("2.2.9"), ""),
            (false, Some("requires deno >= 2.3.0"))
        );
        assert_eq!(
            js_runtime_support(JsRuntime::Deno, Some("2.3.0"), ""),
            (true, None)
        );
        assert_eq!(
            js_runtime_support(JsRuntime::Node, Some("20.19.4"), ""),
            (false, Some("requires node >= 22.0.0"))
        );
        assert_eq!(
            js_runtime_support(JsRuntime::Node, Some("22.0.0"), ""),
            (true, None)
        );
        assert_eq!(
            js_runtime_support(JsRuntime::Bun, Some("1.2.10"), ""),
            (false, Some("requires bun >= 1.2.11"))
        );
        assert_eq!(
            js_runtime_support(JsRuntime::Bun, Some("1.2.11"), ""),
            (true, None)
        );
        assert_eq!(
            js_runtime_support(JsRuntime::Bun, Some("1.3.15"), ""),
            (false, Some("requires bun <= 1.3.14"))
        );
        assert_eq!(
            js_runtime_support(JsRuntime::QuickJs, Some("2023.12.8"), ""),
            (false, Some("requires quickjs >= 2023.12.9"))
        );
        assert_eq!(
            js_runtime_support(JsRuntime::QuickJs, Some("2023.12.9"), ""),
            (true, None)
        );
        assert_eq!(
            js_runtime_support(JsRuntime::QuickJs, None, "quickjs-ng 0.10.0"),
            (true, None)
        );
    }

    #[test]
    fn js_runtime_selection_skips_unsupported_candidates() {
        let probes = vec![
            JsRuntimeProbe {
                runtime: JsRuntime::Deno,
                path: PathBuf::from("/bin/deno"),
                version: Some("2.2.9".to_owned()),
                supported: false,
                reason: Some("requires deno >= 2.3.0"),
            },
            JsRuntimeProbe {
                runtime: JsRuntime::Node,
                path: PathBuf::from("/bin/node"),
                version: Some("22.0.0".to_owned()),
                supported: true,
                reason: None,
            },
        ];
        assert_eq!(select_supported_js_runtime(&probes), Some(JsRuntime::Node));
    }

    #[test]
    fn managed_disabled_excludes_managed_selection_candidate() {
        let cfg = ToolsConfig {
            ytdlp_managed: Some(false),
            ..ToolsConfig::default()
        };
        assert_eq!(managed_candidate(&cfg), None);
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
                crate::test_util::env::set_var("YTM_TOOLS_DIR", &dir);
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
        fn override_path_rejects_line_breaks_before_path_lookup() {
            let err = normalize_program_override(
                "yt-dlp",
                PathBuf::from("C:\n\\Users\\zznn\\yt-dlp.exe"),
            )
            .expect_err("newline override must be rejected");
            assert!(err.contains("line break"));

            let err = normalize_program_override("mpv", PathBuf::from("/tmp/mpv\n"))
                .expect_err("newline mpv override must be rejected");
            assert!(err.contains("mpv override"));
        }

        #[test]
        fn override_path_rejects_directories_and_non_executable_files() {
            let dir = tmp_dir("override-bad");
            let file = dir.join("plain-file");
            std::fs::write(&file, "not executable").unwrap();

            let dir_err = normalize_program_override("mpv", dir.clone())
                .expect_err("directory must not be accepted as an executable override");
            assert!(dir_err.contains("not an executable"));

            let file_err = normalize_program_override("yt-dlp", file)
                .expect_err("non-executable file must not be accepted as an override");
            assert!(file_err.contains("not an executable"));

            let _ = std::fs::remove_dir_all(dir);
        }

        #[test]
        fn pin_for_mpv_uses_exact_selection_for_every_source() {
            let sel = |source| YtdlpSelection {
                path: PathBuf::from("/x/yt-dlp"),
                version: None,
                source,
            };
            assert!(sel(YtdlpSource::Managed).pin_for_mpv().is_some());
            assert!(sel(YtdlpSource::Override).pin_for_mpv().is_some());
            assert!(sel(YtdlpSource::System).pin_for_mpv().is_some());
        }
    }
}
