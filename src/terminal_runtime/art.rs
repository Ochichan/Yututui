use std::hash::{Hash, Hasher};
use std::time::{Duration, Instant};

/// Build the terminal image picker, querying the terminal for its graphics protocol and
/// font size. Falls back to a halfblocks-only picker if the query fails (e.g. a terminal
/// that doesn't answer the control sequences), so album art still renders — just blocky.
pub fn build_art_picker() -> ratatui_image::picker::Picker {
    build_art_picker_with_access(&crate::persist::persistence_access())
}

/// Variant used by startup to carry its already-decided writer capability explicitly.
pub fn build_art_picker_with_access(
    persistence_access: &crate::persist::PersistenceAccess,
) -> ratatui_image::picker::Picker {
    build_art_picker_with_access_deadline(persistence_access, None)
}

/// Startup variant that shares one absolute terminal-negotiation deadline with keyboard and zoom
/// probes. An expired deadline falls back without writing a query.
pub fn build_art_picker_with_access_until(
    persistence_access: &crate::persist::PersistenceAccess,
    deadline: Instant,
) -> ratatui_image::picker::Picker {
    build_art_picker_with_access_deadline(persistence_access, Some(deadline))
}

#[cfg(unix)]
enum GraphicsProbeCleanupRequest {
    Restore(std::io::Error),
    Disarm,
}

/// Run the pre-TUI termios/query/restore transaction off the async owner and enforce the shared
/// startup deadline from outside the vendored call. On Unix, a stuck restore cannot pin signal
/// handling or let startup proceed while a detached worker still owns raw mode.
pub async fn build_art_picker_with_access_until_bounded(
    persistence_access: &crate::persist::PersistenceAccess,
    deadline: Instant,
    shutdown: crate::player::lifetime::ShutdownLatch,
    query_cancellation: ratatui_image::picker::QueryCancellation,
) -> std::io::Result<ratatui_image::picker::Picker> {
    #[cfg(unix)]
    {
        use std::os::fd::AsFd as _;

        let stdin = std::io::stdin();
        let initial_termios = stdin.as_fd().try_clone_to_owned().ok().and_then(|input| {
            rustix::termios::tcgetattr(&input)
                .ok()
                .map(|termios| (input, termios))
        });

        // Start the cleanup coordinator before the query worker can touch termios. Unlike a
        // timeout-time `spawn_blocking`, this cannot sit behind a saturated Tokio blocking pool.
        // If it is scheduled late, the original absolute deadline prevents late restoration.
        let (cleanup_request_tx, cleanup_request_rx) = std::sync::mpsc::sync_channel(1);
        let (cleanup_result_tx, mut cleanup_result_rx) = tokio::sync::oneshot::channel();
        let cleanup_cancellation = query_cancellation.clone();
        let cleanup_worker = std::thread::Builder::new()
            .name("ytt-terminal-graphics-cleanup".to_owned())
            .spawn(move || match cleanup_request_rx.recv() {
                Ok(GraphicsProbeCleanupRequest::Restore(primary)) => {
                    let result = restore_after_failed_probe_blocking(
                        initial_termios,
                        &cleanup_cancellation,
                        deadline,
                        primary,
                    );
                    let _ = cleanup_result_tx.send(result);
                }
                Ok(GraphicsProbeCleanupRequest::Disarm) | Err(_) => {}
            });
        let _cleanup_worker = match cleanup_worker {
            Ok(worker) => worker,
            Err(error) => {
                return Err(std::io::Error::new(
                    error.kind(),
                    format!("could not start terminal graphics cleanup coordinator: {error}"),
                ));
            }
        };

        let access = persistence_access.clone();
        let worker_cancellation = query_cancellation.clone();
        let (worker_tx, mut worker_rx) = tokio::sync::oneshot::channel();
        let worker = std::thread::Builder::new()
            .name("ytt-terminal-graphics-probe".to_owned())
            .spawn(move || {
                let result = try_build_art_picker_with_access_deadline_controlled(
                    &access,
                    Some(deadline),
                    &worker_cancellation,
                );
                let _ = worker_tx.send(result);
            });
        let _worker = match worker {
            Ok(worker) => worker,
            Err(error) => {
                return request_failed_probe_cleanup(
                    &cleanup_request_tx,
                    &mut cleanup_result_rx,
                    &query_cancellation,
                    deadline,
                    std::io::Error::new(
                        error.kind(),
                        format!("could not start terminal graphics probe worker: {error}"),
                    ),
                )
                .await;
            }
        };
        // Reserve enough of the original startup transaction for cancellation fencing and exact
        // termios restoration. The worker keeps the same absolute deadline, so none of these
        // phases can silently rebase another full budget after the caller's three-second bound.
        let cleanup_reserve = crate::terminal_policy::HARD_EXIT_QUERY_QUIESCE_TIMEOUT
            + crate::terminal_policy::EMERGENCY_RESTORE_TIMEOUT;
        let cleanup_at = deadline.checked_sub(cleanup_reserve).unwrap_or(deadline);
        let cleanup_timer = tokio::time::sleep_until(tokio::time::Instant::from_std(cleanup_at));
        tokio::pin!(cleanup_timer);

        tokio::select! {
            biased;
            _ = shutdown.wait() => {
                request_failed_probe_cleanup(
                    &cleanup_request_tx,
                    &mut cleanup_result_rx,
                    &query_cancellation,
                    deadline,
                    std::io::Error::new(
                        std::io::ErrorKind::Interrupted,
                        "terminal graphics probe was cancelled by process shutdown",
                    ),
                ).await
            }
            result = &mut worker_rx => match result {
                Ok(Ok(picker)) => {
                    let _ = cleanup_request_tx.try_send(GraphicsProbeCleanupRequest::Disarm);
                    Ok(picker)
                }
                Ok(Err(error)) => request_failed_probe_cleanup(
                    &cleanup_request_tx,
                    &mut cleanup_result_rx,
                    &query_cancellation,
                    deadline,
                    error,
                ).await,
                Err(error) => request_failed_probe_cleanup(
                    &cleanup_request_tx,
                    &mut cleanup_result_rx,
                    &query_cancellation,
                    deadline,
                    std::io::Error::other(format!("terminal graphics probe worker stopped: {error}")),
                ).await,
            },
            _ = &mut cleanup_timer => {
                request_failed_probe_cleanup(
                    &cleanup_request_tx,
                    &mut cleanup_result_rx,
                    &query_cancellation,
                    deadline,
                    std::io::Error::new(
                        std::io::ErrorKind::TimedOut,
                        "terminal graphics probe exceeded the shared startup deadline",
                    ),
                ).await
            }
        }
    }

    #[cfg(not(unix))]
    {
        let _ = shutdown;
        try_build_art_picker_with_access_deadline_controlled(
            persistence_access,
            Some(deadline),
            &query_cancellation,
        )
    }
}

#[cfg(unix)]
async fn request_failed_probe_cleanup(
    cleanup_request: &std::sync::mpsc::SyncSender<GraphicsProbeCleanupRequest>,
    cleanup_result: &mut tokio::sync::oneshot::Receiver<
        std::io::Result<ratatui_image::picker::Picker>,
    >,
    query_cancellation: &ratatui_image::picker::QueryCancellation,
    cleanup_deadline: Instant,
    primary: std::io::Error,
) -> std::io::Result<ratatui_image::picker::Picker> {
    let kind = primary.kind();
    let primary_message = primary.to_string();
    query_cancellation.cancel();
    if let Err(error) = cleanup_request.try_send(GraphicsProbeCleanupRequest::Restore(primary)) {
        return Err(std::io::Error::new(
            kind,
            format!(
                "{primary_message}; terminal graphics probe cleanup coordinator was unavailable: {error}"
            ),
        ));
    }
    match tokio::time::timeout_at(
        tokio::time::Instant::from_std(cleanup_deadline),
        &mut *cleanup_result,
    )
    .await
    {
        Ok(Ok(result)) => result,
        Ok(Err(error)) => Err(std::io::Error::new(
            kind,
            format!("{primary_message}; terminal graphics probe cleanup worker stopped: {error}"),
        )),
        Err(_) => Err(std::io::Error::new(
            kind,
            format!(
                "{primary_message}; terminal graphics probe cleanup exceeded the shared startup deadline"
            ),
        )),
    }
}

#[cfg(unix)]
fn restore_after_failed_probe_blocking(
    initial_termios: Option<(rustix::fd::OwnedFd, rustix::termios::Termios)>,
    query_cancellation: &ratatui_image::picker::QueryCancellation,
    cleanup_deadline: Instant,
    primary: std::io::Error,
) -> std::io::Result<ratatui_image::picker::Picker> {
    let kind = primary.kind();
    let now = Instant::now();
    let restore_reserve = crate::terminal_policy::EMERGENCY_RESTORE_TIMEOUT;
    let latest_quiesce_deadline = cleanup_deadline
        .checked_sub(restore_reserve)
        .unwrap_or(cleanup_deadline);
    let policy_quiesce_deadline = now
        .checked_add(crate::terminal_policy::HARD_EXIT_QUERY_QUIESCE_TIMEOUT)
        .unwrap_or(latest_quiesce_deadline);
    let fence_deadline = policy_quiesce_deadline.min(latest_quiesce_deadline);
    let restored = query_cancellation.run_cancelled_exclusive_until(fence_deadline, || {
        let Some((input, termios)) = initial_termios else {
            return Err(std::io::Error::new(
                std::io::ErrorKind::NotConnected,
                "the inherited terminal mode was unavailable for exact restoration",
            ));
        };
        let remaining = cleanup_deadline.saturating_duration_since(Instant::now());
        let budget = restore_reserve.min(remaining);
        if budget.is_zero() {
            return Err(std::io::Error::new(
                std::io::ErrorKind::TimedOut,
                "terminal graphics probe had no startup budget left for exact termios restoration",
            ));
        }
        let restore_started = Instant::now();
        let restore_deadline = restore_started
            .checked_add(budget)
            .unwrap_or(cleanup_deadline)
            .min(cleanup_deadline);
        crate::tui::bounded_termios_restore_until(
            "terminal graphics probe emergency raw-mode restore",
            restore_deadline,
            budget,
            input,
            termios,
        )
    });
    match restored {
        Some(Ok(())) => Err(primary),
        None => Err(std::io::Error::new(
            kind,
            format!(
                "{primary}; terminal graphics probe did not quiesce, so unsafe unfenced restoration was skipped"
            ),
        )),
        Some(Err(restore_error)) => Err(std::io::Error::new(
            kind,
            format!("{primary}; emergency termios restore also failed: {restore_error}"),
        )),
    }
}

fn build_art_picker_with_access_deadline(
    persistence_access: &crate::persist::PersistenceAccess,
    deadline: Option<Instant>,
) -> ratatui_image::picker::Picker {
    try_build_art_picker_with_access_deadline(persistence_access, deadline).unwrap_or_else(
        |error| {
            tracing::error!(error = %error, "terminal graphics probe teardown failed");
            ratatui_image::picker::Picker::halfblocks()
        },
    )
}

fn try_build_art_picker_with_access_deadline(
    persistence_access: &crate::persist::PersistenceAccess,
    deadline: Option<Instant>,
) -> std::io::Result<ratatui_image::picker::Picker> {
    let cancellation = ratatui_image::picker::QueryCancellation::new();
    try_build_art_picker_with_access_deadline_controlled(
        persistence_access,
        deadline,
        &cancellation,
    )
}

fn try_build_art_picker_with_access_deadline_controlled(
    persistence_access: &crate::persist::PersistenceAccess,
    deadline: Option<Instant>,
    cancellation: &ratatui_image::picker::QueryCancellation,
) -> std::io::Result<ratatui_image::picker::Picker> {
    use ratatui_image::picker::{Picker, ProtocolType, cap_parser::QueryStdioOptions};
    use std::io::IsTerminal;
    let override_protocol = image_protocol_override();
    let override_is_set = std::env::var_os("YTM_TUI_IMAGE_PROTOCOL").is_some();
    if cancellation.is_cancelled() {
        return Err(std::io::Error::new(
            std::io::ErrorKind::Interrupted,
            "terminal graphics probe was cancelled before it started",
        ));
    }
    if deadline.is_some_and(|deadline| Instant::now() >= deadline) {
        return Ok(Picker::halfblocks());
    }
    // Now that this runs for every launch (not just album-art-on), skip the probe entirely when
    // stdout isn't a terminal (`ytt > file`, a pipe, CI): the query bytes would otherwise be
    // written into the redirect target and the poll would stall the full timeout waiting for a
    // reply that can't come. Stdin-not-a-tty is already handled inside the probe (its
    // `tcgetattr` errors out to halfblocks); this closes the stdout-redirected-but-stdin-tty gap.
    if !std::io::stdout().is_terminal() {
        return Ok(Picker::halfblocks());
    }
    if override_protocol == Some(ProtocolType::Halfblocks) {
        return Ok(Picker::halfblocks());
    }
    if let Some(picker) = cached_halfblocks_art_picker() {
        return Ok(picker);
    }

    let has_native_hint = terminal_has_native_image_hint();
    let probe_started = Instant::now();
    let Some(probe_timeout) = terminal_probe_timeout_until(
        terminal_image_probe_timeout(has_native_hint),
        deadline,
        probe_started,
    ) else {
        return Ok(Picker::halfblocks());
    };
    let query_deadline = probe_started + probe_timeout;
    let mut picker = match Picker::from_query_stdio_with_options_until(
        QueryStdioOptions {
            timeout: probe_timeout,
            ..QueryStdioOptions::default()
        },
        query_deadline,
        cancellation,
    ) {
        Ok(picker) => picker,
        Err(ratatui_image::errors::Errors::TerminalRestore(error)) => {
            return Err(std::io::Error::other(error.to_string()));
        }
        Err(e) => {
            tracing::warn!(error = %e, "terminal graphics probe failed; album art falls back to halfblocks");
            Picker::halfblocks()
        }
    };
    if let Some(protocol) = override_protocol {
        picker.set_protocol_type(protocol);
    } else if !override_is_set
        && picker.protocol_type() == ProtocolType::Halfblocks
        && let Some(protocol) = trusted_protocol_hint()
    {
        tracing::warn!(
            ?protocol,
            "terminal probe returned halfblocks despite strong native image hint; using hinted protocol"
        );
        picker.set_protocol_type(protocol);
    }
    if picker.protocol_type() == ProtocolType::Halfblocks {
        store_halfblocks_art_picker_cache(persistence_access);
    }
    Ok(picker)
}

fn cached_halfblocks_art_picker() -> Option<ratatui_image::picker::Picker> {
    if terminal_has_native_image_hint() {
        return None;
    }
    let path = art_picker_cache_path()?;
    // No-symlink, size-bounded read (the cache is one short line); bypasses raw std::fs.
    let bytes = crate::util::safe_fs::read_no_symlink_limited(&path, 64 * 1024).ok()?;
    let contents = String::from_utf8(bytes).ok()?;
    let mut parts = contents.trim().split('\t');
    let version = parts.next()?;
    let key = parts.next()?;
    let protocol = parts.next()?;
    let stored_at = parts.next()?.parse::<u64>().ok()?;
    if version != "v1" || key != terminal_probe_cache_key().to_string() || protocol != "halfblocks"
    {
        return None;
    }
    let now = unix_seconds()?;
    if now.saturating_sub(stored_at) > Duration::from_secs(24 * 60 * 60).as_secs() {
        return None;
    }
    Some(ratatui_image::picker::Picker::halfblocks())
}

fn store_halfblocks_art_picker_cache(persistence_access: &crate::persist::PersistenceAccess) {
    if persistence_access.is_read_only() || terminal_has_native_image_hint() {
        return;
    }
    let Some(path) = art_picker_cache_path() else {
        return;
    };
    let Some(now) = unix_seconds() else {
        return;
    };
    let contents = format!("v1\t{}\thalfblocks\t{now}\n", terminal_probe_cache_key());
    // Atomic, private (0600), no-symlink write; also creates the cache dir (0700) if needed.
    let _ = crate::util::safe_fs::write_private_atomic(&path, contents.as_bytes());
}

fn art_picker_cache_path() -> Option<std::path::PathBuf> {
    crate::paths::cache_dir().map(|cache_dir| cache_dir.join("art-picker.cache"))
}

fn terminal_probe_cache_key() -> u64 {
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    for name in [
        "TERM",
        "TERM_PROGRAM",
        "WT_SESSION",
        "TMUX",
        "KITTY_WINDOW_ID",
        "WEZTERM_EXECUTABLE",
        "KONSOLE_VERSION",
        "YTM_TUI_IMAGE_PROTOCOL",
    ] {
        name.hash(&mut hasher);
        std::env::var_os(name)
            .map(|value| value.to_string_lossy().into_owned())
            .hash(&mut hasher);
    }
    hasher.finish()
}

fn terminal_has_native_image_hint() -> bool {
    let term = std::env::var("TERM")
        .unwrap_or_default()
        .to_ascii_lowercase();
    let term_program = std::env::var("TERM_PROGRAM")
        .unwrap_or_default()
        .to_ascii_lowercase();
    env_nonempty("KITTY_WINDOW_ID")
        || env_nonempty("WEZTERM_EXECUTABLE")
        || env_nonempty("KONSOLE_VERSION")
        || env_nonempty("WT_SESSION")
        || term_program == "iterm.app"
        || term_program.contains("wezterm")
        || term_program.contains("ghostty")
        || [
            "kitty", "ghostty", "wezterm", "foot", "konsole", "mlterm", "mintty", "rio", "contour",
        ]
        .iter()
        .any(|hint| term.contains(hint))
}

fn env_nonempty(name: &str) -> bool {
    std::env::var_os(name).is_some_and(|value| !value.is_empty())
}

fn terminal_image_probe_timeout(has_native_hint: bool) -> Duration {
    Duration::from_millis(if has_native_hint { 700 } else { 250 })
}

fn terminal_probe_timeout_until(
    policy_timeout: Duration,
    deadline: Option<Instant>,
    now: Instant,
) -> Option<Duration> {
    let remaining = deadline
        .map(|deadline| deadline.saturating_duration_since(now))
        .unwrap_or(policy_timeout);
    let timeout = policy_timeout.min(remaining);
    (!timeout.is_zero()).then_some(timeout)
}

fn trusted_protocol_hint() -> Option<ratatui_image::picker::ProtocolType> {
    use ratatui_image::picker::ProtocolType;

    let term = std::env::var("TERM")
        .unwrap_or_default()
        .to_ascii_lowercase();
    let term_program = std::env::var("TERM_PROGRAM")
        .unwrap_or_default()
        .to_ascii_lowercase();

    if env_nonempty("KITTY_WINDOW_ID")
        || term.contains("xterm-kitty")
        || term.contains("kitty")
        || term.contains("ghostty")
        || term_program.contains("ghostty")
    {
        return Some(ProtocolType::Kitty);
    }

    if term_program == "iterm.app" {
        return Some(ProtocolType::Iterm2);
    }

    #[cfg(windows)]
    {
        if env_nonempty("WEZTERM_EXECUTABLE") {
            return Some(ProtocolType::Iterm2);
        }
    }

    None
}

fn unix_seconds() -> Option<u64> {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .ok()
        .map(|duration| duration.as_secs())
}

fn image_protocol_override() -> Option<ratatui_image::picker::ProtocolType> {
    let value = std::env::var("YTM_TUI_IMAGE_PROTOCOL").ok()?;
    parse_image_protocol_override(&value)
}

fn parse_image_protocol_override(value: &str) -> Option<ratatui_image::picker::ProtocolType> {
    use ratatui_image::picker::ProtocolType;
    match value.trim().to_ascii_lowercase().as_str() {
        "halfblocks" | "halfblock" | "blocks" | "block" => Some(ProtocolType::Halfblocks),
        "sixel" => Some(ProtocolType::Sixel),
        "kitty" => Some(ProtocolType::Kitty),
        "iterm2" | "iterm" => Some(ProtocolType::Iterm2),
        _ => None,
    }
}

pub(crate) fn log_art_picker(picker: Option<&ratatui_image::picker::Picker>) {
    let override_value = std::env::var("YTM_TUI_IMAGE_PROTOCOL").ok();
    let term = std::env::var("TERM").ok();
    let term_program = std::env::var("TERM_PROGRAM").ok();
    let wt_session = std::env::var("WT_SESSION").ok();
    if let Some(value) = override_value.as_deref()
        && parse_image_protocol_override(value).is_none()
    {
        tracing::warn!(
            image_protocol_override = value,
            "ignored unsupported terminal image protocol override"
        );
    }
    let Some(picker) = picker else {
        tracing::info!(
            image_protocol_override = override_value.as_deref().unwrap_or(""),
            term = term.as_deref().unwrap_or(""),
            term_program = term_program.as_deref().unwrap_or(""),
            wt_session = wt_session.as_deref().unwrap_or(""),
            "terminal graphics disabled"
        );
        return;
    };
    let font = picker.font_size();
    tracing::info!(
        protocol = ?picker.protocol_type(),
        cell_width = font.width,
        cell_height = font.height,
        capabilities = ?picker.capabilities(),
        image_protocol_override = override_value.as_deref().unwrap_or(""),
        term = term.as_deref().unwrap_or(""),
        term_program = term_program.as_deref().unwrap_or(""),
        wt_session = wt_session.as_deref().unwrap_or(""),
        "terminal graphics picker"
    );
}

#[cfg(test)]
mod tests {
    use crate::test_util::env::{with_var, with_vars};
    use ratatui_image::picker::ProtocolType;

    use super::*;

    const TERMINAL_ENV: &[(&str, Option<&str>)] = &[
        ("KITTY_WINDOW_ID", None),
        ("WEZTERM_EXECUTABLE", None),
        ("KONSOLE_VERSION", None),
        ("WT_SESSION", None),
        ("TERM", None),
        ("TERM_PROGRAM", None),
    ];

    fn with_terminal_env<T>(vars: &[(&str, Option<&str>)], f: impl FnOnce() -> T) -> T {
        let mut scoped = TERMINAL_ENV.to_vec();
        scoped.extend_from_slice(vars);
        with_vars(&scoped, f)
    }

    #[test]
    fn art_cache_uses_the_override_and_read_only_access_never_creates_it() {
        let mut suffix = [0_u8; 8];
        getrandom::fill(&mut suffix).unwrap();
        let suffix = suffix
            .into_iter()
            .map(|byte| format!("{byte:02x}"))
            .collect::<String>();
        let cache =
            std::env::temp_dir().join(format!("yututui-art-cache-{}-{suffix}", std::process::id()));
        let cache_env = cache.to_string_lossy().into_owned();

        with_terminal_env(&[("YTM_CACHE_DIR", Some(cache_env.as_str()))], || {
            assert_eq!(
                art_picker_cache_path(),
                Some(cache.join("art-picker.cache"))
            );
            store_halfblocks_art_picker_cache(&crate::persist::PersistenceAccess::ReadOnly {
                reason: std::sync::Arc::from("test observational owner"),
            });
            assert!(
                !cache.exists(),
                "read-only art probing must not create the overridden cache root"
            );

            store_halfblocks_art_picker_cache(&crate::persist::PersistenceAccess::Writable);
            assert!(cache.join("art-picker.cache").is_file());
        });
        let _ = std::fs::remove_dir_all(cache);
    }

    #[test]
    fn image_protocol_override_accepts_supported_protocol_names() {
        assert_eq!(
            parse_image_protocol_override("halfblocks"),
            Some(ProtocolType::Halfblocks)
        );
        assert_eq!(
            parse_image_protocol_override("sixel"),
            Some(ProtocolType::Sixel)
        );
        assert_eq!(
            parse_image_protocol_override("kitty"),
            Some(ProtocolType::Kitty)
        );
        assert_eq!(
            parse_image_protocol_override("iterm2"),
            Some(ProtocolType::Iterm2)
        );
        assert_eq!(
            parse_image_protocol_override("  SIXEL  "),
            Some(ProtocolType::Sixel)
        );
        assert_eq!(parse_image_protocol_override("bad"), None);
    }

    #[test]
    fn image_protocol_override_reads_environment_exactly() {
        with_var("YTM_TUI_IMAGE_PROTOCOL", Some("kitty"), || {
            assert_eq!(image_protocol_override(), Some(ProtocolType::Kitty));
        });
        with_var("YTM_TUI_IMAGE_PROTOCOL", Some("unsupported"), || {
            assert_eq!(image_protocol_override(), None);
        });
        with_var("YTM_TUI_IMAGE_PROTOCOL", None, || {
            assert_eq!(image_protocol_override(), None);
        });
    }

    #[test]
    fn terminal_native_image_hints_follow_known_env_vars() {
        with_terminal_env(&[], || assert!(!terminal_has_native_image_hint()));

        with_terminal_env(&[("KITTY_WINDOW_ID", Some("1"))], || {
            assert!(terminal_has_native_image_hint());
        });
        with_terminal_env(&[("TERM", Some("xterm-kitty"))], || {
            assert!(terminal_has_native_image_hint());
        });
        with_terminal_env(&[("WT_SESSION", Some("1"))], || {
            assert!(terminal_has_native_image_hint());
        });
        with_terminal_env(&[("TERM", Some("foot"))], || {
            assert!(terminal_has_native_image_hint());
        });
        with_terminal_env(&[("TERM", Some("mintty"))], || {
            assert!(terminal_has_native_image_hint());
        });
        with_terminal_env(&[("TERM", Some("mlterm"))], || {
            assert!(terminal_has_native_image_hint());
        });
        with_terminal_env(&[("TERM", Some("rio"))], || {
            assert!(terminal_has_native_image_hint());
        });
        with_terminal_env(&[("TERM", Some("contour"))], || {
            assert!(terminal_has_native_image_hint());
        });
        with_terminal_env(&[("TERM_PROGRAM", Some("WezTerm"))], || {
            assert!(terminal_has_native_image_hint());
        });
        with_terminal_env(&[("TERM_PROGRAM", Some("iTerm.app"))], || {
            assert!(terminal_has_native_image_hint());
        });
        with_terminal_env(&[("TERM_PROGRAM", Some("plain-terminal"))], || {
            assert!(!terminal_has_native_image_hint());
        });
    }

    #[test]
    fn terminal_image_probe_timeout_only_extends_for_native_hints() {
        assert_eq!(
            terminal_image_probe_timeout(false),
            Duration::from_millis(250)
        );
        assert_eq!(
            terminal_image_probe_timeout(true),
            Duration::from_millis(700)
        );
    }

    #[test]
    fn image_probe_timeout_is_clamped_to_the_shared_startup_deadline() {
        let now = Instant::now();
        assert_eq!(
            terminal_probe_timeout_until(
                Duration::from_millis(700),
                Some(now + Duration::from_millis(125)),
                now,
            ),
            Some(Duration::from_millis(125))
        );
        assert_eq!(
            terminal_probe_timeout_until(Duration::from_millis(250), Some(now), now),
            None
        );
        assert_eq!(
            terminal_probe_timeout_until(Duration::from_millis(250), None, now),
            Some(Duration::from_millis(250))
        );
    }

    #[test]
    fn trusted_protocol_hint_uses_only_strong_native_hints() {
        with_terminal_env(&[], || assert_eq!(trusted_protocol_hint(), None));

        with_terminal_env(&[("KITTY_WINDOW_ID", Some("1"))], || {
            assert_eq!(trusted_protocol_hint(), Some(ProtocolType::Kitty));
        });
        with_terminal_env(&[("TERM", Some("xterm-kitty"))], || {
            assert_eq!(trusted_protocol_hint(), Some(ProtocolType::Kitty));
        });
        with_terminal_env(&[("TERM", Some("ghostty"))], || {
            assert_eq!(trusted_protocol_hint(), Some(ProtocolType::Kitty));
        });
        with_terminal_env(&[("TERM_PROGRAM", Some("iTerm.app"))], || {
            assert_eq!(trusted_protocol_hint(), Some(ProtocolType::Iterm2));
        });
        with_terminal_env(&[("WT_SESSION", Some("1"))], || {
            assert_eq!(trusted_protocol_hint(), None);
        });
        with_terminal_env(&[("TERM", Some("foot"))], || {
            assert_eq!(trusted_protocol_hint(), None);
        });
    }

    #[test]
    fn terminal_probe_cache_key_changes_with_relevant_environment() {
        let base = with_var("YTM_TUI_IMAGE_PROTOCOL", None, terminal_probe_cache_key);
        with_var("YTM_TUI_IMAGE_PROTOCOL", Some("sixel"), || {
            assert_ne!(terminal_probe_cache_key(), base);
        });
    }

    #[test]
    fn env_nonempty_distinguishes_absent_empty_and_present() {
        with_var("YTM_TUI_TEST_ENV_NONEMPTY", None, || {
            assert!(!env_nonempty("YTM_TUI_TEST_ENV_NONEMPTY"));
        });
        with_var("YTM_TUI_TEST_ENV_NONEMPTY", Some(""), || {
            assert!(!env_nonempty("YTM_TUI_TEST_ENV_NONEMPTY"));
        });
        with_var("YTM_TUI_TEST_ENV_NONEMPTY", Some("1"), || {
            assert!(env_nonempty("YTM_TUI_TEST_ENV_NONEMPTY"));
        });
    }
}
