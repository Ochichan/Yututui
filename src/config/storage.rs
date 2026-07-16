use super::*;

impl Config {
    /// Defaults written only for a genuinely new profile. Keep this separate from [`Default`]:
    /// serde recovery, legacy configs, tests, and non-TUI reset paths all rely on conservative
    /// defaults that do not opt an existing user into a walkthrough.
    pub(crate) fn fresh_install() -> Self {
        Self {
            beginner_mode: true,
            beginner_tutorial: BeginnerTutorialProgress::start(),
            ..Self::default()
        }
    }

    pub(crate) fn preflight_persistence_recovery()
    -> Result<(), crate::persist::StartupRecoveryError> {
        let Some(path) = config_path() else {
            return Ok(());
        };
        crate::persist::preflight_journal_recovery::<Self>(
            crate::persist::StoreKind::Config,
            &path,
            MAX_CONFIG_BYTES,
        )
    }

    /// Load config, importing from the old app on first run. Never fails: a missing or
    /// corrupt file falls back to defaults (+ migration).
    pub fn load() -> Self {
        let Some(path) = config_path() else {
            // No config dir on this platform: migrate from the old app but don't persist.
            let old = old_config_path();
            let mut cfg = config_for_missing_profile(old.as_deref());
            if let Some(old) = old {
                import_old_from(&old, &mut cfg);
            }
            return cfg;
        };
        Self::load_from(&path)
    }

    /// Load from an explicit path (also the test seam). `config.json` is the only
    /// secret-bearing store (YTM cookie, Gemini key, Last.fm/ListenBrainz tokens), so a
    /// present-but-unloadable file is set aside *before* the defaults `save()` below can
    /// overwrite it — matching `safe_fs::load_json_or_default_limited`'s backup-aside policy.
    pub(super) fn load_from(path: &std::path::Path) -> Self {
        recovery::load_from_path(path)
    }

    /// Persist atomically (write a temp file, then rename over the target).
    pub fn save(&self) -> std::io::Result<()> {
        crate::persist::ensure_persistence_writes_allowed()?;
        let Some(path) = config_path() else {
            return Ok(()); // no config dir on this platform; nothing to do
        };
        self.save_to(&path)
    }

    pub(super) fn save_to(&self, path: &std::path::Path) -> std::io::Result<()> {
        crate::persist::write_config_json(path, self)
    }
}

/// Default location for an optional exported Netscape cookies file.
///
/// macOS: `~/Music/yututui/cookies.txt`
/// Windows: `%USERPROFILE%\Music\yututui\cookies.txt`
pub fn default_cookies_file() -> Option<PathBuf> {
    default_ytm_dir().map(|dir| dir.join("cookies.txt"))
}

pub(super) fn validate_external_cookies_file(path: &Path) -> std::io::Result<()> {
    let meta = fs::symlink_metadata(path)?;
    if meta.file_type().is_symlink() {
        return Err(std::io::Error::new(
            std::io::ErrorKind::PermissionDenied,
            "cookies file must not be a symlink",
        ));
    }
    #[cfg(windows)]
    {
        use std::os::windows::fs::MetadataExt;
        const FILE_ATTRIBUTE_REPARSE_POINT: u32 = 0x0000_0400;
        if meta.file_attributes() & FILE_ATTRIBUTE_REPARSE_POINT != 0 {
            return Err(std::io::Error::new(
                std::io::ErrorKind::PermissionDenied,
                "cookies file must not be a reparse point",
            ));
        }
    }
    if !meta.is_file() {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            "cookies path is not a regular file",
        ));
    }
    if meta.len() > MAX_COOKIE_BYTES {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            "cookies file is too large",
        ));
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        if meta.permissions().mode() & 0o077 != 0 {
            tracing::warn!(
                "cookies file is readable outside the owner; importing a private external-tool copy"
            );
        }
    }
    Ok(())
}

pub(super) fn import_external_cookies_file(
    source: &Path,
    data_dir: &Path,
) -> std::io::Result<PathBuf> {
    validate_external_cookies_file(source)?;
    let bytes = safe_fs::read_no_symlink_limited(source, MAX_COOKIE_BYTES)?;
    let target = data_dir.join(EXTERNAL_COOKIES_COPY);
    safe_fs::write_private_atomic(&target, &bytes)?;
    Ok(target)
}

pub(super) fn external_cookies_warning(error: &std::io::Error) -> String {
    let reason = match error.kind() {
        std::io::ErrorKind::NotFound => "file was not found",
        std::io::ErrorKind::PermissionDenied => "permission or symlink check failed",
        std::io::ErrorKind::InvalidInput => "path is not a regular file",
        std::io::ErrorKind::InvalidData => "file is too large or invalid",
        _ => "file could not be read or imported",
    };
    format!(
        "Cookies file not used for mpv/yt-dlp: {reason}. Use a real non-symlink cookies.txt under 4 MiB."
    )
}

/// Default directory for downloaded tracks.
///
/// macOS: `~/Music/yututui`
/// Windows: `%USERPROFILE%\Music\yututui`
pub fn default_download_dir() -> PathBuf {
    default_ytm_dir().unwrap_or_else(|| PathBuf::from("yututui"))
}

/// Normalize a user-supplied directory (env override or stored config): trim surrounding
/// whitespace, treat a whitespace-only value as "unset" (→ fall through to the default), and
/// expand a leading `~` / `~/` to the home directory. Without this a value like `~/Music` was
/// stored/used literally (creating a dir named `~`), and `"   "` created a spaces-named dir.
pub(super) fn normalize_user_dir(raw: &str) -> Option<PathBuf> {
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        return None;
    }
    if trimmed == "~" {
        return directories::BaseDirs::new().map(|b| b.home_dir().to_path_buf());
    }
    if let Some(rest) = trimmed.strip_prefix("~/")
        && let Some(base) = directories::BaseDirs::new()
    {
        return Some(base.home_dir().join(rest));
    }
    Some(PathBuf::from(trimmed))
}

/// Default directory for saved radio recordings: `<music>/yututui/recordings`.
pub fn default_recording_dir() -> PathBuf {
    default_ytm_dir()
        .map(|d| d.join("recordings"))
        .unwrap_or_else(|| PathBuf::from("yututui/recordings"))
}

fn default_ytm_dir() -> Option<PathBuf> {
    directories::UserDirs::new()
        .and_then(|u| u.audio_dir().map(std::path::Path::to_path_buf))
        .map(ytm_dir_under_audio_dir)
}

pub(super) fn ytm_dir_under_audio_dir(audio_dir: PathBuf) -> PathBuf {
    audio_dir.join("yututui")
}

pub(crate) fn config_path() -> Option<PathBuf> {
    crate::paths::config_dir().map(|d| d.join("config.json"))
}

pub(super) fn old_config_path() -> Option<PathBuf> {
    // Never import the original app's config while a config-dir override is active (a non-blank
    // env override — mirroring `paths::config_dir`'s blank-is-unset rule — or the test sandbox):
    // tests must not read the developer's real `~/.youtube-music-cli`.
    let overridden = std::env::var("YTM_CONFIG_DIR").is_ok_and(|v| !v.trim().is_empty());
    if overridden || cfg!(test) {
        return None;
    }
    directories::BaseDirs::new().map(|d| d.home_dir().join(".youtube-music-cli/config.json"))
}

/// Pick the base for a missing current config. A legacy path that cannot be inspected counts as
/// present: surprising an existing user with onboarding is worse than conservatively leaving it
/// off, and the import itself will still use the hardened bounded/no-symlink reader below.
pub(super) fn config_for_missing_profile(old: Option<&Path>) -> Config {
    let legacy_present = old.is_some_and(|path| match fs::symlink_metadata(path) {
        Ok(_) => true,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => false,
        Err(_) => true,
    });
    if legacy_present {
        Config::default()
    } else {
        Config::fresh_install()
    }
}

/// Pull whatever we can reuse out of the old TypeScript app's config. Favorites,
/// history and playlists are migrated later (M5) when the library view consumes them.
pub(super) fn import_old_from(path: &std::path::Path, cfg: &mut Config) {
    // Legacy import: cap the read and refuse a symlink, like every other persisted-state read.
    let Ok(bytes) = crate::util::safe_fs::read_no_symlink_limited(path, MAX_CONFIG_BYTES) else {
        return;
    };
    let Ok(text) = String::from_utf8(bytes) else {
        return;
    };
    let Ok(json) = serde_json::from_str::<serde_json::Value>(&text) else {
        return;
    };
    if let Some(v) = json.get("volume").and_then(serde_json::Value::as_i64) {
        cfg.volume = v.clamp(0, 100);
    }
    if let Some(c) = json
        .pointer("/youtubeMusic/cookie")
        .and_then(serde_json::Value::as_str)
        && !c.is_empty()
    {
        cfg.cookie = Some(c.to_owned());
    }
    if let Some(d) = json
        .get("downloadDirectory")
        .and_then(serde_json::Value::as_str)
        && !d.is_empty()
    {
        cfg.download_dir = Some(PathBuf::from(d));
    }
}

/// Turn a Netscape `cookies.txt` body into a `name=value; ...` header, keeping only
/// youtube.com cookies (where the YTM auth cookies — SAPISID etc. — live).
pub(super) fn parse_netscape_cookies(content: &str) -> String {
    let mut pairs = Vec::new();
    for raw in content.lines() {
        // `#HttpOnly_` lines are real cookies; any other leading `#` is a comment.
        let line = raw.strip_prefix("#HttpOnly_").unwrap_or(raw);
        if line.starts_with('#') || line.trim().is_empty() {
            continue;
        }
        let fields: Vec<&str> = line.split('\t').collect();
        if fields.len() < 7 {
            continue;
        }
        // Suffix-boundary match, not `contains`: `evil-youtube.com` / `notyoutube.com` must not
        // slip through. A Netscape domain field may carry a leading dot (`.youtube.com`).
        let domain = fields[0].trim_start_matches('.');
        if !(domain == "youtube.com" || domain.ends_with(".youtube.com")) {
            continue;
        }
        let (name, value) = (fields[5].trim(), fields[6].trim());
        // Drop any pair carrying header-breaking characters. A genuine cookie value never
        // contains CR/LF/`;`; rejecting them keeps a crafted cookies.txt from injecting extra
        // header pairs into the `name=value; …` string.
        if name.is_empty() || name.contains(['\r', '\n', ';']) || value.contains(['\r', '\n', ';'])
        {
            continue;
        }
        pairs.push(format!("{name}={value}"));
    }
    pairs.join("; ")
}
