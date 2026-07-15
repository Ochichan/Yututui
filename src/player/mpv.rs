//! Spawning the mpv child process.
//!
//! mpv runs headless (`--no-video --no-terminal --idle=yes`) as a pure audio engine
//! and exposes a JSON IPC endpoint we drive from [`super::ipc`]. `--no-config` keeps
//! startup predictable and the footprint small. stdio is sent to null so mpv can never
//! write into our terminal.

use std::collections::HashMap;
use std::path::Path;
use std::sync::Mutex;

use anyhow::{Context, Result, bail};

use crate::config::MpvAudioRuntimeConfig;
#[cfg(unix)]
use crate::util::runtime;

/// Whether the installed mpv accepts `flag` — the version-independence gate for any
/// option newer than a particular build. POSIX playback requires mpv 0.33+ for the native
/// guardian lease, but version-independent probes still avoid brittle version-string parsing.
/// Unsupported mpv must never see an unknown flag
/// (they are fatal at startup), and version parsing breaks on git builds, so this
/// runs a capability probe instead:
///
/// ```text
/// <mpv> --no-config <flag> --version
/// ```
///
/// The flag must come BEFORE `--version`: mpv validates only the options parsed
/// before `--version` short-circuits, so the reversed order reports success for
/// any flag. Probed once per (process, flag) — the daemon respawns mpv on every
/// stop→play cycle, and the answer can't change mid-run.
pub fn flag_supported(flag: &str) -> bool {
    static CACHE: Mutex<Option<HashMap<(String, String), bool>>> = Mutex::new(None);
    // Recover the guard on poison instead of panicking: a probe is a pure, cached bool, so a
    // prior panic while holding the lock can't leave it in a bad state worth propagating.
    let mut cache = CACHE.lock().unwrap_or_else(|e| e.into_inner());
    let cache = cache.get_or_insert_with(HashMap::new);
    let program = crate::tools::mpv_program();
    let key = (program.clone(), flag.to_owned());
    if let Some(&supported) = cache.get(&key) {
        return supported;
    }
    if !crate::deps::on_path(&program) {
        cache.insert(key, false);
        return false;
    }
    // Route through the timeout-bounded runner so a wedged/hung mpv binary can't block this
    // synchronous startup probe forever; a timeout (or any error) is treated as "unsupported".
    let supported = super::guardian::probe(
        &program,
        vec![
            "--no-config".to_owned(),
            flag.to_owned(),
            "--version".to_owned(),
        ],
        std::time::Duration::from_secs(5),
        64 * 1024,
    )
    .map(|out| out.success)
    .unwrap_or(false);
    cache.insert(key, supported);
    supported
}

/// Whether this platform can enforce the guardian-to-mpv hard-death lease.
///
/// POSIX uses mpv's `--input-ipc-client=fd://N` (introduced in 0.33). Windows instead uses
/// parent-only outer and guardian-only inner kill-on-close Job Objects, so no mpv option gate is
/// needed there.
pub fn lifeline_supported() -> bool {
    #[cfg(unix)]
    {
        static CACHE: Mutex<Option<(String, bool)>> = Mutex::new(None);
        let program = crate::tools::mpv_program();
        let mut cached = CACHE.lock().unwrap_or_else(|error| error.into_inner());
        if let Some((cached_program, supported)) = cached.as_ref()
            && cached_program == &program
        {
            return *supported;
        }
        // The guardian injects its own valid fd:// client immediately before --version. An old mpv
        // rejects that option, while a supported build proves the exact lease path playback uses.
        let supported = crate::deps::on_path(&program)
            && super::guardian::probe(
                &program,
                vec!["--version".to_owned()],
                std::time::Duration::from_secs(5),
                64 * 1024,
            )
            .is_ok_and(|output| output.success);
        *cached = Some((program, supported));
        supported
    }
    #[cfg(windows)]
    {
        static CACHE: Mutex<Option<(String, bool)>> = Mutex::new(None);
        let program = crate::tools::mpv_program();
        let mut cached = CACHE.lock().unwrap_or_else(|error| error.into_inner());
        if let Some((cached_program, supported)) = cached.as_ref()
            && cached_program == &program
        {
            return *supported;
        }
        let supported = crate::deps::on_path(&program)
            && super::guardian::probe(
                &program,
                vec!["--version".to_owned()],
                std::time::Duration::from_secs(5),
                64 * 1024,
            )
            .is_ok_and(|output| output.success);
        *cached = Some((program, supported));
        supported
    }
    #[cfg(not(any(unix, windows)))]
    {
        false
    }
}

/// Fail closed before playback when the required process-lifetime primitive is unavailable.
pub fn ensure_lifeline_supported() -> Result<()> {
    let program = crate::tools::mpv_program();
    if !crate::deps::on_path(&program) {
        bail!("mpv is not installed or not executable: {program}");
    }
    if !lifeline_supported() {
        #[cfg(unix)]
        bail!(
            "mpv lifetime protection unavailable: POSIX playback requires mpv 0.33 or newer \
             with --input-ipc-client=fd:// support"
        );
        #[cfg(windows)]
        bail!(
            "mpv lifetime protection unavailable: Windows outer/inner Job Object guardian \
             setup or the guarded mpv probe failed"
        );
        #[cfg(not(any(unix, windows)))]
        bail!("mpv lifetime protection unavailable on this platform");
    }
    Ok(())
}

/// First `mpv --version` line, executed under the same native-lease guardian as every other mpv
/// probe. An old POSIX mpv without `fd://` client support intentionally yields no version rather
/// than creating a lease-free process.
pub fn version_line(program: &str) -> Option<String> {
    if !crate::deps::on_path(program) {
        return None;
    }
    super::guardian::probe(
        program,
        vec!["--version".to_owned()],
        std::time::Duration::from_secs(5),
        64 * 1024,
    )
    .ok()
    .filter(|out| out.success)
    .and_then(|out| String::from_utf8(out.stdout).ok())
    .and_then(|stdout| stdout.lines().next().map(str::to_owned))
}

/// Whether mpv accepts `--media-controls` (added in mpv 0.39).
///
/// mpv ≥ 0.39 registers its **own** OS media session by default — Windows SMTC
/// and macOS Now Playing — even in headless/audio-only mode, which would sit in
/// the flyout next to the session `crate::media` publishes as a duplicate entry
/// with raw stream metadata. Both our spawns pass `--media-controls=no` when
/// supported (see [`flag_supported`]).
pub fn media_controls_flag_supported() -> bool {
    flag_supported("--media-controls=no")
}

/// Whether mpv supports the `stream-record` property used by the radio recorder (a startup
/// option since mpv ~0.31, so present on any mpv this app targets — probed anyway so an
/// ancient build silently disables recording instead of erroring).
pub fn stream_record_supported() -> bool {
    flag_supported("--stream-record=")
}

/// A per-process IPC endpoint: a Unix socket path on macOS/Linux, a named pipe on
/// Windows. Unique per pid so concurrent instances never collide.
pub fn ipc_path() -> Result<String> {
    let pid = std::process::id();
    #[cfg(windows)]
    {
        Ok(format!(r"\\.\pipe\yututui-mpv-{pid}"))
    }
    #[cfg(unix)]
    {
        Ok(runtime::app_runtime_dir()
            .context("prepare mpv IPC runtime dir")?
            .join(format!("yututui-mpv-{pid}.sock"))
            .to_string_lossy()
            .into_owned())
    }
}

/// IPC endpoint for the *video overlay* mpv ([`crate::player::video`]), unique per
/// (pid, spawn generation) so a `Shift+V` respawn never races the previous window's
/// socket/pipe.
pub fn video_ipc_path(generation: u64) -> Result<String> {
    let pid = std::process::id();
    #[cfg(windows)]
    {
        Ok(format!(r"\\.\pipe\yututui-mpv-video-{pid}-{generation}"))
    }
    #[cfg(unix)]
    {
        Ok(runtime::app_runtime_dir()
            .context("prepare mpv IPC runtime dir")?
            .join(format!("yututui-mpv-video-{pid}-{generation}.sock"))
            .to_string_lossy()
            .into_owned())
    }
}

/// Spawn mpv listening on `ipc_path` behind the same-binary guardian. The guardian owns the
/// process tree and a heartbeat/native IPC lease; [`super::Mpv`] owns and boundedly reaps that
/// guardian. Signal, panic, terminal-loss, and disk-recovery backstops live in
/// [`super::lifetime`].
///
/// `cookies_file`, when present, is handed to mpv's bundled yt-dlp so authenticated
/// streams resolve (age-gated / library tracks). yt-dlp owns stream resolution — PO
/// tokens, signature deciphering, throttling — so we never touch those ourselves.
///
/// The yt-dlp that ytdl_hook runs is pinned to [`crate::tools`]' exact selection via
/// `--script-opts=ytdl_hook-ytdl_path=…`; even a system selection is passed as an
/// absolute path so mpv does not redo PATH lookup differently. ytdl_hook reads that
/// option once at spawn, so the TUI's
/// long-lived mpv keeps its spawn-time binary until restart — the playback self-heal
/// therefore feeds mpv *resolved* CDN URLs (via the resolver) instead of watch URLs
/// when a fresher yt-dlp lands mid-session; the daemon simply respawns its player.
pub(crate) fn spawn(
    ipc_path: &str,
    cookies_file: Option<&Path>,
    gapless: bool,
    audio: &MpvAudioRuntimeConfig,
    managed_cache_args: &[String],
) -> Result<super::guardian::GuardedSpawn> {
    super::lifetime::ensure_media_start_allowed()?;
    ensure_lifeline_supported()?;
    let mut args = vec![
        "--no-video".to_owned(),
        "--no-terminal".to_owned(),
        "--idle=yes".to_owned(),
        // Retained legacy protocol robustness: old JSON end-file events omitted a reason.
        // Keeping a naturally ended file open
        // gives the eof-reached observer one ordered loop turn before the app loads the next
        // track; load failures still enter the explicit idle event used by the IPC actor.
        "--keep-open=yes".to_owned(),
        "--no-config".to_owned(),
        // Don't decode embedded cover art into a video track — pure audio engine.
        "--audio-display=no".to_owned(),
        if gapless {
            "--gapless-audio=yes".to_owned()
        } else {
            "--gapless-audio=no".to_owned()
        },
        // Bound the streaming cache: mpv's default forward demuxer cache is 150 MiB,
        // which would balloon RAM on long tracks. ~40 MiB total is plenty of audio
        // buffering while keeping us honest on priority #1 (low RAM).
        "--cache=yes".to_owned(),
        format!("--input-ipc-server={ipc_path}"),
    ];
    args.extend(structured_audio_args(audio));

    // Capability-selected current-media cache lifecycle options. They precede both raw user
    // layers so the existing last-option-wins escape hatch remains authoritative.
    for arg in managed_cache_args {
        args.push(arg.clone());
    }

    for arg in crate::tools::mpv_ytdl_raw_option_args(cookies_file) {
        args.push(arg);
    }

    // Pin ytdl_hook to the selected yt-dlp. Ancient option, no probe needed;
    // `--no-config` above means there are no user script-opts to clobber. Before
    // YTM_MPV_EXTRA so last-option-wins keeps the user lever.
    if let Some(sel) = crate::tools::ytdlp_selection()
        && let Some(pin) = sel.pin_for_mpv()
    {
        let pin = pin.canonicalize().unwrap_or_else(|_| pin.to_path_buf());
        tracing::info!(
            source = sel.source.label(),
            path = %pin.display(),
            "mpv ytdl_hook pinned"
        );
        // `-append` (not `--script-opts=`) so a pin path containing a comma can't be misread as
        // an option separator by mpv's script-opts parser. Under `--no-config` this is the only
        // script-opt, so append is functionally identical in the normal case — just comma-safe.
        args.push(format!(
            "--script-opts-append=ytdl_hook-ytdl_path={}",
            pin.display()
        ));
    }

    // The OS media session is ours (`crate::media`), not mpv's — see the probe
    // docs. Placed before YTM_MPV_EXTRA so mpv's last-option-wins rule keeps
    // `YTM_MPV_EXTRA=--media-controls=yes` working as a user override.
    if media_controls_flag_supported() {
        args.push("--media-controls=no".to_owned());
    }

    for arg in &audio.extra_args {
        args.push(arg.clone());
    }

    // Escape hatch for tests/debugging, e.g. `YTM_MPV_EXTRA="--ao=null --volume=0"`.
    // Quote-aware so a value with spaces (`--x="/My Music/y"`) survives as one arg; simple
    // space-separated flags behave exactly as the previous `split_whitespace`.
    if let Ok(extra) = std::env::var("YTM_MPV_EXTRA") {
        for a in split_shell_like(&extra) {
            args.push(a);
        }
    }

    super::guardian::spawn(&crate::tools::mpv_program(), args, false)
        .context("failed to spawn protected mpv")
}

pub(crate) fn structured_audio_args(audio: &MpvAudioRuntimeConfig) -> Vec<String> {
    let mut args = vec!["--audio-fallback-to-null=yes".to_owned()];
    if let Some(device) = &audio.device {
        args.push(format!("--audio-device={device}"));
    } else if let Some(output) = &audio.output {
        args.push(format!("--ao={output}"));
    }
    args.push(format!("--demuxer-max-bytes={}", audio.cache_forward));
    args.push(format!("--demuxer-max-back-bytes={}", audio.cache_back));
    args
}

/// Split a `YTM_MPV_EXTRA`-style string into args, honoring single/double quotes so a value
/// with spaces (`--ao-null-device="/My Music/x"`) survives as ONE arg. Unquoted runs split on
/// whitespace exactly like the previous `split_whitespace`, so simple debug values
/// (`--ao=null --volume=0`) are unaffected. Not a full shell parser — no escapes or expansion.
fn split_shell_like(s: &str) -> Vec<String> {
    let mut args = Vec::new();
    let mut cur = String::new();
    let mut in_arg = false;
    let mut quote: Option<char> = None;
    for ch in s.chars() {
        match quote {
            Some(q) => {
                if ch == q {
                    quote = None;
                } else {
                    cur.push(ch);
                }
            }
            None if ch == '\'' || ch == '"' => {
                quote = Some(ch);
                in_arg = true;
            }
            None if ch.is_whitespace() => {
                if in_arg {
                    args.push(std::mem::take(&mut cur));
                    in_arg = false;
                }
            }
            None => {
                cur.push(ch);
                in_arg = true;
            }
        }
    }
    if in_arg || quote.is_some() {
        args.push(cur);
    }
    args
}

#[cfg(test)]
mod tests {
    use super::{split_shell_like, structured_audio_args};
    use crate::config::MpvAudioRuntimeConfig;

    #[test]
    fn split_shell_like_matches_whitespace_for_simple_values() {
        assert_eq!(
            split_shell_like("--ao=null --volume=0"),
            ["--ao=null", "--volume=0"]
        );
        assert_eq!(split_shell_like("   a   b  "), ["a", "b"]);
        assert!(split_shell_like("").is_empty());
    }

    #[test]
    fn split_shell_like_keeps_quoted_spaces_together() {
        assert_eq!(
            split_shell_like(r#"--ao-null-device="/My Music/x" --volume=0"#),
            ["--ao-null-device=/My Music/x", "--volume=0"]
        );
        assert_eq!(split_shell_like("--x='a b c'"), ["--x=a b c"]);
    }

    #[test]
    fn structured_audio_args_prefer_device_and_enable_safe_fallback() {
        let audio = MpvAudioRuntimeConfig {
            output: Some("pipewire".to_owned()),
            device: Some("pipewire/42".to_owned()),
            cache_forward: "64MiB".to_owned(),
            cache_back: "16MiB".to_owned(),
            long_form_seek_optimization: crate::config::LongFormSeekOptimization::Off,
            extra_args: vec!["--ao=null".to_owned()],
        };

        assert_eq!(
            structured_audio_args(&audio),
            [
                "--audio-fallback-to-null=yes",
                "--audio-device=pipewire/42",
                "--demuxer-max-bytes=64MiB",
                "--demuxer-max-back-bytes=16MiB",
            ]
        );
    }

    #[test]
    fn structured_audio_args_include_output_only_without_device() {
        let audio = MpvAudioRuntimeConfig {
            output: Some("coreaudio".to_owned()),
            device: None,
            cache_forward: "32MiB".to_owned(),
            cache_back: "8MiB".to_owned(),
            long_form_seek_optimization: crate::config::LongFormSeekOptimization::Off,
            extra_args: Vec::new(),
        };

        assert_eq!(
            structured_audio_args(&audio),
            [
                "--audio-fallback-to-null=yes",
                "--ao=coreaudio",
                "--demuxer-max-bytes=32MiB",
                "--demuxer-max-back-bytes=8MiB",
            ]
        );
    }
}
