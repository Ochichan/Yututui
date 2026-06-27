//! Persistent configuration: cross-platform paths, atomic save, and a one-time import
//! of the old `~/.youtube-music-cli/config.json`.
//!
//! Auth: either an inline `cookie` (raw `Cookie:` header) or a `cookies_file` pointing
//! at a Netscape `cookies.txt`. If no file is configured, `~/Music/ytm-tui/cookies.txt`
//! (the platform music folder) is tried. The header form feeds ytmapi-rs; the file is
//! also handed to mpv/yt-dlp so they own stream resolution (PO tokens, throttling).

use std::fs;
use std::path::PathBuf;

use serde::{Deserialize, Serialize};

use crate::ai::GeminiModel;
use crate::eq::{self, EqPreset};

/// Clamp range for playback speed (matches the `>`/`<` controls and the settings slider).
pub const SPEED_MIN: f64 = 0.5;
pub const SPEED_MAX: f64 = 2.0;

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct Config {
    /// Raw `Cookie:` header for music.youtube.com (takes precedence over the file).
    pub cookie: Option<String>,
    /// Path to a Netscape `cookies.txt` exported from the browser.
    /// `None` -> `<user music dir>/ytm-tui/cookies.txt`.
    pub cookies_file: Option<PathBuf>,
    /// Startup volume, 0-100.
    pub volume: i64,
    /// Where downloads are saved. `None` -> `<user music dir>/ytm-tui`.
    pub download_dir: Option<PathBuf>,
    /// Capture the mouse for buttons and click-to-seek. `None` → enabled.
    pub mouse: Option<bool>,

    // Playback / EQ -----------------------------------------------------------
    /// Selected equalizer preset.
    pub eq_preset: EqPreset,
    /// Hand-tuned band gains (dB). `None` → use the preset's gains.
    pub eq_bands: Option<[f64; eq::BANDS]>,
    /// Loudness normalization (`dynaudnorm`). `None` → off.
    pub normalize: Option<bool>,
    /// Playback speed multiplier. `None` → 1.0×.
    pub speed: Option<f64>,
    /// Gapless playback. `None` → on. Takes effect at the next launch (an mpv flag).
    pub gapless: Option<bool>,
    /// Auto-extend the queue with related tracks when it runs low. `None` → off.
    pub autoplay_radio: Option<bool>,

    // AI assistant ------------------------------------------------------------
    /// Google Gemini API key. The `GEMINI_API_KEY` env var overrides this when set.
    pub gemini_api_key: Option<String>,
    /// Which Gemini model the assistant uses.
    pub gemini_model: GeminiModel,

    // Keybindings -------------------------------------------------------------
    /// User keybinding overrides, keyed `"<context>.<action>"` → chord string (e.g.
    /// `"player.toggle_pause" -> "space"`). Only entries that differ from the built-in
    /// defaults are stored; everything else falls back to [`crate::keymap`]'s defaults.
    pub keybindings: std::collections::BTreeMap<String, String>,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            cookie: None,
            cookies_file: None,
            volume: 100,
            download_dir: None,
            mouse: None,
            eq_preset: EqPreset::default(),
            eq_bands: None,
            normalize: None,
            speed: None,
            gapless: None,
            autoplay_radio: None,
            gemini_api_key: None,
            gemini_model: GeminiModel::default(),
            keybindings: std::collections::BTreeMap::new(),
        }
    }
}

impl Config {
    /// Load config, importing from the old app on first run. Never fails: a missing or
    /// corrupt file falls back to defaults (+ migration).
    pub fn load() -> Self {
        if let Some(path) = config_path()
            && let Ok(text) = fs::read_to_string(&path)
            && let Ok(cfg) = serde_json::from_str::<Config>(&text)
        {
            return cfg;
        }
        // First run (or unreadable): migrate from the old app, then persist.
        let mut cfg = Config::default();
        if let Some(old) = old_config_path() {
            import_old_from(&old, &mut cfg);
        }
        let _ = cfg.save();
        cfg
    }

    /// Persist atomically (write a temp file, then rename over the target).
    pub fn save(&self) -> std::io::Result<()> {
        let Some(path) = config_path() else {
            return Ok(()); // no config dir on this platform; nothing to do
        };
        if let Some(dir) = path.parent() {
            fs::create_dir_all(dir)?;
        }
        let json = serde_json::to_string_pretty(self)?;
        let tmp = path.with_extension("json.tmp");
        fs::write(&tmp, json)?;
        fs::rename(&tmp, &path)
    }

    /// The `Cookie:` header to authenticate ytmapi-rs, from the inline value or by
    /// parsing the configured/default `cookies.txt`. `None` if neither yields cookies.
    pub fn effective_cookie(&self) -> Option<String> {
        if let Some(c) = &self.cookie
            && !c.trim().is_empty()
        {
            return Some(c.clone());
        }
        if let Some(file) = self.effective_cookies_file()
            && let Ok(content) = fs::read_to_string(file)
        {
            let header = parse_netscape_cookies(&content);
            if !header.is_empty() {
                return Some(header);
            }
        }
        None
    }

    /// The cookies file to try. An explicit setting wins; otherwise the cross-platform
    /// default is `<user music dir>/ytm-tui/cookies.txt`.
    pub fn effective_cookies_file(&self) -> Option<PathBuf> {
        self.cookies_file.clone().or_else(default_cookies_file)
    }

    /// The concrete directory downloads are saved to. Precedence: `YTM_DOWNLOAD_DIR`
    /// env override, then the configured `download_dir`, then `<user music dir>/ytm-tui`.
    /// The music folder resolves per-OS (`~/Music` on macOS, the Music known-folder on
    /// Windows) via the `directories` crate.
    pub fn effective_download_dir(&self) -> PathBuf {
        if let Ok(env) = std::env::var("YTM_DOWNLOAD_DIR")
            && !env.is_empty()
        {
            return PathBuf::from(env);
        }
        if let Some(dir) = &self.download_dir {
            return dir.clone();
        }
        default_download_dir()
    }

    /// Whether to capture the mouse (buttons and click-to-seek). Enabled unless set to `false`.
    pub fn effective_mouse(&self) -> bool {
        self.mouse.unwrap_or(true)
    }

    /// The ten band gains to apply: the hand-tuned array if set, else the preset's.
    pub fn effective_eq_bands(&self) -> [f64; eq::BANDS] {
        self.eq_bands.unwrap_or_else(|| self.eq_preset.gains())
    }

    /// Whether loudness normalization is on (default off).
    pub fn effective_normalize(&self) -> bool {
        self.normalize.unwrap_or(false)
    }

    /// Playback speed, clamped to the supported range (default 1.0×).
    pub fn effective_speed(&self) -> f64 {
        self.speed.unwrap_or(1.0).clamp(SPEED_MIN, SPEED_MAX)
    }

    /// Whether gapless playback is on (default on).
    pub fn effective_gapless(&self) -> bool {
        self.gapless.unwrap_or(true)
    }

    /// Whether queue auto-extend (radio) is on (default off).
    pub fn effective_autoplay_radio(&self) -> bool {
        self.autoplay_radio.unwrap_or(false)
    }

    /// The Gemini API key to use. The `GEMINI_API_KEY` env var wins over the config
    /// value; whitespace is trimmed and an empty result is treated as unset (`None`).
    pub fn effective_gemini_api_key(&self) -> Option<String> {
        if let Ok(env) = std::env::var("GEMINI_API_KEY") {
            let trimmed = env.trim();
            if !trimmed.is_empty() {
                return Some(trimmed.to_owned());
            }
        }
        self.gemini_api_key
            .as_ref()
            .map(|k| k.trim().to_owned())
            .filter(|k| !k.is_empty())
    }

    /// The Gemini model the assistant uses.
    pub fn effective_gemini_model(&self) -> GeminiModel {
        self.gemini_model
    }
}

/// Default location for an optional exported Netscape cookies file.
///
/// macOS: `~/Music/ytm-tui/cookies.txt`
/// Windows: `%USERPROFILE%\Music\ytm-tui\cookies.txt`
pub fn default_cookies_file() -> Option<PathBuf> {
    default_ytm_dir().map(|dir| dir.join("cookies.txt"))
}

/// Default directory for downloaded tracks.
///
/// macOS: `~/Music/ytm-tui`
/// Windows: `%USERPROFILE%\Music\ytm-tui`
pub fn default_download_dir() -> PathBuf {
    default_ytm_dir().unwrap_or_else(|| PathBuf::from("ytm-tui"))
}

fn default_ytm_dir() -> Option<PathBuf> {
    directories::UserDirs::new()
        .and_then(|u| u.audio_dir().map(std::path::Path::to_path_buf))
        .map(ytm_dir_under_audio_dir)
}

fn ytm_dir_under_audio_dir(audio_dir: PathBuf) -> PathBuf {
    audio_dir.join("ytm-tui")
}

fn config_path() -> Option<PathBuf> {
    directories::ProjectDirs::from("", "", "ytm-tui")
        .map(|d| d.config_dir().join("config.json"))
}

fn old_config_path() -> Option<PathBuf> {
    directories::BaseDirs::new().map(|d| d.home_dir().join(".youtube-music-cli/config.json"))
}

/// Pull whatever we can reuse out of the old TypeScript app's config. Favorites,
/// history and playlists are migrated later (M5) when the library view consumes them.
fn import_old_from(path: &std::path::Path, cfg: &mut Config) {
    let Ok(text) = fs::read_to_string(path) else {
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
    if let Some(d) = json.get("downloadDirectory").and_then(serde_json::Value::as_str)
        && !d.is_empty()
    {
        cfg.download_dir = Some(PathBuf::from(d));
    }
}

/// Turn a Netscape `cookies.txt` body into a `name=value; ...` header, keeping only
/// youtube.com cookies (where the YTM auth cookies — SAPISID etc. — live).
fn parse_netscape_cookies(content: &str) -> String {
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
        if !fields[0].contains("youtube.com") {
            continue;
        }
        let (name, value) = (fields[5].trim(), fields[6].trim());
        if !name.is_empty() {
            pairs.push(format!("{name}={value}"));
        }
    }
    pairs.join("; ")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn defaults_have_full_volume() {
        let c = Config::default();
        assert_eq!(c.volume, 100);
        assert!(c.cookie.is_none());
    }

    #[test]
    fn json_round_trips() {
        let c = Config {
            cookie: Some("SID=abc".to_owned()),
            cookies_file: Some(PathBuf::from("/tmp/cookies.txt")),
            volume: 70,
            download_dir: Some(PathBuf::from("/tmp/dl")),
            mouse: Some(false),
            eq_preset: EqPreset::BassBoost,
            eq_bands: Some([1.0; eq::BANDS]),
            normalize: Some(true),
            speed: Some(1.5),
            gapless: Some(false),
            autoplay_radio: Some(true),
            gemini_api_key: Some("AIzaSecret".to_owned()),
            gemini_model: GeminiModel::Latest,
            keybindings: std::collections::BTreeMap::new(),
        };
        let s = serde_json::to_string(&c).unwrap();
        let back: Config = serde_json::from_str(&s).unwrap();
        assert_eq!(back.volume, 70);
        assert_eq!(back.cookie.as_deref(), Some("SID=abc"));
        assert_eq!(back.download_dir, Some(PathBuf::from("/tmp/dl")));
        assert_eq!(back.mouse, Some(false));
        assert_eq!(back.eq_preset, EqPreset::BassBoost);
        assert_eq!(back.eq_bands, Some([1.0; eq::BANDS]));
        assert_eq!(back.normalize, Some(true));
        assert_eq!(back.speed, Some(1.5));
        assert_eq!(back.gapless, Some(false));
        assert_eq!(back.autoplay_radio, Some(true));
        assert_eq!(back.gemini_api_key.as_deref(), Some("AIzaSecret"));
        assert_eq!(back.gemini_model, GeminiModel::Latest);
    }

    #[test]
    fn keybindings_persist_through_config_json() {
        use crate::keymap::{Action, KeyContext, KeyMap, parse_chord};

        // Rebind a key, then capture it the way `close_settings` does on save.
        let mut km = KeyMap::default();
        km.rebind(KeyContext::Player, Action::TogglePause, parse_chord("P").unwrap()).unwrap();
        let cfg = Config { keybindings: km.to_overrides(), ..Config::default() };
        // Only the diff from defaults is persisted.
        assert_eq!(cfg.keybindings.get("player.toggle_pause").map(String::as_str), Some("P"));

        // Round-trip through the exact serde path `Config::save`/`load` use (write JSON,
        // read it back) — proving the override survives a restart.
        let json = serde_json::to_string_pretty(&cfg).unwrap();
        let back: Config = serde_json::from_str(&json).unwrap();

        // On next launch the persisted override rebuilds into the live keymap.
        let restored = KeyMap::from_config(&back);
        assert_eq!(restored.action(KeyContext::Player, parse_chord("P").unwrap()), Some(Action::TogglePause));
        assert_eq!(restored.action(KeyContext::Player, parse_chord("space").unwrap()), None);
    }

    #[test]
    fn gemini_key_env_overrides_config() {
        let cfg = Config { gemini_api_key: Some("from_config".to_owned()), ..Config::default() };
        // SAFETY: single-threaded test; set+unset around the calls.
        unsafe { std::env::set_var("GEMINI_API_KEY", "  from_env  ") };
        assert_eq!(cfg.effective_gemini_api_key().as_deref(), Some("from_env"));
        unsafe { std::env::remove_var("GEMINI_API_KEY") };
        assert_eq!(cfg.effective_gemini_api_key().as_deref(), Some("from_config"));

        // Empty/whitespace key reads as unset.
        let blank = Config { gemini_api_key: Some("   ".to_owned()), ..Config::default() };
        assert_eq!(blank.effective_gemini_api_key(), None);
    }

    #[test]
    fn playback_effective_defaults_and_overrides() {
        let d = Config::default();
        assert_eq!(d.effective_eq_bands(), [0.0; eq::BANDS]);
        assert!(!d.effective_normalize());
        assert_eq!(d.effective_speed(), 1.0);
        assert!(d.effective_gapless());
        assert!(!d.effective_autoplay_radio());

        // Preset gains feed through when no hand-tuned bands are set.
        let preset = Config { eq_preset: EqPreset::BassBoost, ..Config::default() };
        assert_eq!(preset.effective_eq_bands(), EqPreset::BassBoost.gains());

        // Speed is clamped to the supported range.
        let fast = Config { speed: Some(9.0), ..Config::default() };
        assert_eq!(fast.effective_speed(), SPEED_MAX);
    }

    #[test]
    fn mouse_enabled_by_default_and_overridable() {
        assert!(Config::default().effective_mouse());
        let off = Config { mouse: Some(false), ..Config::default() };
        assert!(!off.effective_mouse());
    }

    #[test]
    fn missing_fields_use_defaults() {
        let back: Config = serde_json::from_str("{}").unwrap();
        assert_eq!(back.volume, 100);
    }

    #[test]
    fn parses_netscape_cookies_youtube_only() {
        let txt = "# Netscape HTTP Cookie File\n\
                   .youtube.com\tTRUE\t/\tTRUE\t1999999999\tSAPISID\tsecret1\n\
                   #HttpOnly_.youtube.com\tTRUE\t/\tTRUE\t1999999999\tSID\tsecret2\n\
                   .example.com\tTRUE\t/\tFALSE\t1999999999\tIGNORED\tnope\n";
        let header = parse_netscape_cookies(txt);
        assert!(header.contains("SAPISID=secret1"));
        assert!(header.contains("SID=secret2"));
        assert!(!header.contains("IGNORED"));
    }

    #[test]
    fn default_cookies_file_lives_under_audio_dir() {
        assert_eq!(
            ytm_dir_under_audio_dir(PathBuf::from("/Users/alice/Music")).join("cookies.txt"),
            PathBuf::from("/Users/alice/Music/ytm-tui/cookies.txt")
        );
    }

    #[test]
    fn default_download_dir_lives_under_audio_dir() {
        assert_eq!(
            ytm_dir_under_audio_dir(PathBuf::from("/Users/alice/Music")),
            PathBuf::from("/Users/alice/Music/ytm-tui")
        );
    }

    #[test]
    fn configured_cookies_file_overrides_default() {
        let cfg = Config {
            cookies_file: Some(PathBuf::from("/custom/cookies.txt")),
            ..Config::default()
        };
        assert_eq!(
            cfg.effective_cookies_file(),
            Some(PathBuf::from("/custom/cookies.txt"))
        );
    }

    #[test]
    fn env_overrides_download_dir() {
        // SAFETY: single-threaded test; we set+unset around the call.
        unsafe { std::env::set_var("YTM_DOWNLOAD_DIR", "/tmp/ytm-dl-test") };
        let dir = Config::default().effective_download_dir();
        unsafe { std::env::remove_var("YTM_DOWNLOAD_DIR") };
        assert_eq!(dir, PathBuf::from("/tmp/ytm-dl-test"));
    }

    #[test]
    fn imports_old_download_directory() {
        let dir = std::env::temp_dir().join(format!("ytm-old-dl-{}", std::process::id()));
        fs::create_dir_all(&dir).unwrap();
        let p = dir.join("config.json");
        fs::write(&p, r#"{"downloadDirectory":"/music/dl"}"#).unwrap();
        let mut cfg = Config::default();
        import_old_from(&p, &mut cfg);
        assert_eq!(cfg.download_dir, Some(PathBuf::from("/music/dl")));
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn imports_old_volume_and_cookie() {
        let dir = std::env::temp_dir().join(format!("ytm-old-{}", std::process::id()));
        fs::create_dir_all(&dir).unwrap();
        let p = dir.join("config.json");
        fs::write(
            &p,
            r#"{"volume":42,"youtubeMusic":{"cookie":"SID=fromold"}}"#,
        )
        .unwrap();
        let mut cfg = Config::default();
        import_old_from(&p, &mut cfg);
        assert_eq!(cfg.volume, 42);
        assert_eq!(cfg.cookie.as_deref(), Some("SID=fromold"));
        let _ = fs::remove_dir_all(&dir);
    }
}
