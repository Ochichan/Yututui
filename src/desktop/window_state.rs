//! Main-window geometry persistence (docs/gui/03 §8): `desktop.json` next to `config.json`.
//!
//! Saved debounced on move/resize by the event loop; on restore the rect is clamped to the
//! union of currently-available monitors so a window saved on a now-disconnected display
//! lands on the primary instead of off-screen (mirrors `position_near`'s monitor logic).

use std::path::PathBuf;

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct WindowRect {
    pub x: i32,
    pub y: i32,
    pub w: u32,
    pub h: u32,
    #[serde(default)]
    pub maximized: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct Point {
    pub x: i32,
    pub y: i32,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DesktopState {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub main: Option<WindowRect>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub mini: Option<Point>,
    #[serde(default = "default_true")]
    pub close_to_tray: bool,
    #[serde(default)]
    pub keep_webview_alive: bool,
    /// Mini player skin id (`PanelTheme::id`). Stored as a plain string so an id
    /// written by a newer build degrades to the default theme instead of failing
    /// the whole file; `None` means the default theme.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub mini_theme: Option<String>,
}

fn default_true() -> bool {
    true
}

impl Default for DesktopState {
    fn default() -> Self {
        DesktopState {
            main: None,
            mini: None,
            close_to_tray: true,
            keep_webview_alive: false,
            mini_theme: None,
        }
    }
}

/// A monitor's logical rectangle, for clamping.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct MonitorRect {
    pub x: i32,
    pub y: i32,
    pub w: u32,
    pub h: u32,
}

impl DesktopState {
    /// `desktop.json` beside the persistent config (`config.json`).
    pub fn path() -> Option<PathBuf> {
        directories::ProjectDirs::from("", "", "ytm-tui")
            .map(|d| d.config_dir().join("desktop.json"))
    }

    /// Load persisted state, or defaults when absent/corrupt (never fails a launch).
    ///
    /// Routes through `safe_fs` like every other persisted store: schema-drift recovery
    /// (a field a newer build wrote degrades that field, not the whole file), symlink
    /// rejection, and a size cap — a tiny geometry file has no business being large.
    pub fn load() -> Self {
        let Some(path) = Self::path() else {
            return Self::default();
        };
        crate::util::safe_fs::load_json_or_default_limited(&path, MAX_STATE_BYTES)
    }

    /// Persist state (best-effort). Atomic temp-write + rename with private perms so a crash
    /// or a concurrent read never sees a half-written file. Output is byte-identical pretty
    /// JSON. Creates the config dir if needed.
    pub fn save(&self) {
        let Some(path) = Self::path() else {
            return;
        };
        let _ = crate::util::safe_fs::write_private_atomic_json(&path, self);
    }
}

/// `desktop.json` holds only window geometry + a few flags; nothing legitimate approaches
/// this. Bounds a corrupt/hostile file before it's read into memory.
const MAX_STATE_BYTES: u64 = 1024 * 1024;

/// Clamp a saved rect onto the available monitors. If the rect's center sits inside a
/// monitor, keep it there (nudged fully on-screen); otherwise recenter onto the primary
/// (`monitors[0]`). Uses min-then-max (not `clamp`) so a monitor smaller than the window
/// degrades to the top-left corner instead of panicking on an inverted range.
pub fn clamp_to_monitors(rect: WindowRect, monitors: &[MonitorRect]) -> WindowRect {
    let Some(primary) = monitors.first() else {
        return rect;
    };
    let cx = rect.x + rect.w as i32 / 2;
    let cy = rect.y + rect.h as i32 / 2;
    let host = monitors
        .iter()
        .find(|m| cx >= m.x && cx < m.x + m.w as i32 && cy >= m.y && cy < m.y + m.h as i32)
        .unwrap_or(primary);

    let max_x = host.x + host.w as i32 - rect.w as i32;
    let max_y = host.y + host.h as i32 - rect.h as i32;
    WindowRect {
        x: rect.x.min(max_x).max(host.x),
        y: rect.y.min(max_y).max(host.y),
        ..rect
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const PRIMARY: MonitorRect = MonitorRect {
        x: 0,
        y: 0,
        w: 1920,
        h: 1080,
    };
    const SECOND: MonitorRect = MonitorRect {
        x: 1920,
        y: 0,
        w: 1920,
        h: 1080,
    };

    #[test]
    fn defaults_close_to_tray() {
        let s = DesktopState::default();
        assert!(s.close_to_tray);
        assert!(!s.keep_webview_alive);
    }

    #[test]
    fn on_screen_rect_is_unchanged() {
        let rect = WindowRect {
            x: 100,
            y: 80,
            w: 1200,
            h: 800,
            maximized: false,
        };
        assert_eq!(clamp_to_monitors(rect, &[PRIMARY, SECOND]), rect);
    }

    #[test]
    fn rect_on_removed_monitor_recenters_onto_primary() {
        // Saved on the second monitor, which is now gone (only primary present).
        let rect = WindowRect {
            x: 2200,
            y: 200,
            w: 1200,
            h: 800,
            maximized: false,
        };
        let clamped = clamp_to_monitors(rect, &[PRIMARY]);
        assert!(
            clamped.x >= PRIMARY.x && clamped.x + clamped.w as i32 <= PRIMARY.x + PRIMARY.w as i32
        );
        assert!(clamped.y >= PRIMARY.y);
    }

    #[test]
    fn partially_offscreen_rect_is_nudged_fully_on() {
        let rect = WindowRect {
            x: 1400,
            y: 900,
            w: 1200,
            h: 800,
            maximized: false,
        };
        let clamped = clamp_to_monitors(rect, &[PRIMARY]);
        assert_eq!(clamped.x, 1920 - 1200);
        assert_eq!(clamped.y, 1080 - 800);
    }

    #[test]
    fn round_trips_through_json() {
        let s = DesktopState {
            main: Some(WindowRect {
                x: 1,
                y: 2,
                w: 1200,
                h: 800,
                maximized: true,
            }),
            mini: Some(Point { x: 10, y: 20 }),
            close_to_tray: false,
            keep_webview_alive: true,
            mini_theme: Some("minimal".to_string()),
        };
        let line = serde_json::to_string(&s).unwrap();
        assert_eq!(serde_json::from_str::<DesktopState>(&line).unwrap(), s);
    }

    #[test]
    fn state_without_mini_theme_still_parses() {
        // A desktop.json written before the theme field existed.
        let line = r#"{"main":{"x":1,"y":2,"w":1200,"h":800},"close_to_tray":true,"keep_webview_alive":false}"#;
        let s: DesktopState = serde_json::from_str(line).unwrap();
        assert_eq!(s.mini_theme, None);
        // And an absent theme never appears on disk (keeps old builds parsing it).
        assert!(!serde_json::to_string(&s).unwrap().contains("mini_theme"));
    }
}
