use std::hash::{Hash, Hasher};
use std::time::Duration;

/// Build the terminal image picker, querying the terminal for its graphics protocol and
/// font size. Falls back to a halfblocks-only picker if the query fails (e.g. a terminal
/// that doesn't answer the control sequences), so album art still renders — just blocky.
pub fn build_art_picker() -> ratatui_image::picker::Picker {
    use ratatui_image::picker::{Picker, ProtocolType, cap_parser::QueryStdioOptions};
    use std::io::IsTerminal;
    let override_protocol = image_protocol_override();
    let override_is_set = std::env::var_os("YTM_TUI_IMAGE_PROTOCOL").is_some();
    // Now that this runs for every launch (not just album-art-on), skip the probe entirely when
    // stdout isn't a terminal (`ytt > file`, a pipe, CI): the query bytes would otherwise be
    // written into the redirect target and the poll would stall the full timeout waiting for a
    // reply that can't come. Stdin-not-a-tty is already handled inside the probe (its
    // `tcgetattr` errors out to halfblocks); this closes the stdout-redirected-but-stdin-tty gap.
    if !std::io::stdout().is_terminal() {
        return Picker::halfblocks();
    }
    if override_protocol == Some(ProtocolType::Halfblocks) {
        return Picker::halfblocks();
    }
    if let Some(picker) = cached_halfblocks_art_picker() {
        return picker;
    }

    let has_native_hint = terminal_has_native_image_hint();
    let mut picker = match Picker::from_query_stdio_with_options(QueryStdioOptions {
        timeout: terminal_image_probe_timeout(has_native_hint),
        ..QueryStdioOptions::default()
    }) {
        Ok(picker) => picker,
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
        store_halfblocks_art_picker_cache();
    }
    picker
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

fn store_halfblocks_art_picker_cache() {
    if terminal_has_native_image_hint() {
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
    directories::ProjectDirs::from("", "", "yututui")
        .map(|dirs| dirs.cache_dir().join("art-picker.cache"))
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
