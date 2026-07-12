//! Desktop geometry persistence (docs/gui/03 §8): `desktop.json` in yututray's independent
//! config root. The playback owner's yututui configuration tree is compatibility-read-only.
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

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct Size {
    pub w: u32,
    pub h: u32,
}

/// Versioned, monitor-relative placement.  The legacy absolute `main`/`mini`
/// fields remain readable while this representation survives a monitor moving,
/// changing scale, or disappearing between launches.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct WindowPlacement {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub monitor_key: Option<String>,
    pub work_area: MonitorRect,
    pub origin: Point,
    pub size: Size,
    #[serde(default)]
    pub maximized: bool,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct PlacementV2 {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub main: Option<WindowPlacement>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub mini: Option<WindowPlacement>,
}

impl PlacementV2 {
    fn is_empty(&self) -> bool {
        self.main.is_none() && self.mini.is_none()
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DesktopState {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub main: Option<WindowRect>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub mini: Option<Point>,
    /// Explicit behavior, independent of the selected skin. Unpinned panels are
    /// transient popovers; pinned panels persist their position and stay on top.
    #[serde(default)]
    pub mini_pinned: bool,
    #[serde(default, skip_serializing_if = "PlacementV2::is_empty")]
    pub placement_v2: PlacementV2,
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
            mini_pinned: false,
            placement_v2: PlacementV2::default(),
            close_to_tray: true,
            keep_webview_alive: false,
            mini_theme: None,
        }
    }
}

/// A monitor's logical rectangle, for clamping.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct MonitorRect {
    pub x: i32,
    pub y: i32,
    pub w: u32,
    pub h: u32,
}

/// Live monitor information used to resolve a saved placement.  `key` is a
/// best-effort platform monitor name; the work-area signature is the fallback
/// when a driver changes the name without changing the display topology.
#[derive(Debug, Clone, PartialEq)]
pub struct MonitorDescriptor {
    pub key: Option<String>,
    /// Work area in device-independent pixels for persistence and clamping.
    pub work_area: MonitorRect,
    /// The same work area in the native global physical-pixel coordinate space.
    pub physical_work_area: MonitorRect,
    /// Native scale for this monitor. Kept with the descriptor so a hidden window that was
    /// initially created on a different-DPI monitor is never used as the conversion source.
    pub scale_factor: f64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ResolvedPlacement {
    pub rect: WindowRect,
    pub monitor_index: usize,
}

/// A live native-pixel rectangle reconciled against the currently attached displays.
/// Unlike [`ResolvedPlacement`], this type is intentionally physical: it is used while a
/// window already exists and the display topology or scale may have changed underneath it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ReconciledPhysicalRect {
    pub rect: WindowRect,
    pub monitor_index: usize,
}

impl WindowPlacement {
    pub fn capture(rect: WindowRect, monitor: &MonitorDescriptor) -> Self {
        Self {
            monitor_key: monitor.key.clone(),
            work_area: monitor.work_area,
            origin: Point {
                x: difference_i32(rect.x, monitor.work_area.x),
                y: difference_i32(rect.y, monitor.work_area.y),
            },
            size: Size {
                w: rect.w,
                h: rect.h,
            },
            maximized: rect.maximized,
        }
    }

    /// Resolve onto current work areas. Prefer the stable monitor key, then an exact work-area
    /// signature; if neither survives, center on the primary. The selected monitor index is
    /// retained so callers can convert with the *target* monitor's DPI rather than the hidden
    /// host window's current DPI.
    pub fn resolve(&self, monitors: &[MonitorDescriptor]) -> Option<ResolvedPlacement> {
        let primary = monitors.first()?;
        let host_index = self
            .monitor_key
            .as_ref()
            .and_then(|key| {
                monitors
                    .iter()
                    .position(|monitor| monitor.key.as_ref() == Some(key))
            })
            .or_else(|| {
                monitors
                    .iter()
                    .position(|monitor| monitor.work_area == self.work_area)
            });

        if let Some(monitor_index) = host_index {
            let host = &monitors[monitor_index];
            let rect = WindowRect {
                x: add_i32(host.work_area.x, self.origin.x),
                y: add_i32(host.work_area.y, self.origin.y),
                w: self.size.w,
                h: self.size.h,
                maximized: self.maximized,
            };
            Some(ResolvedPlacement {
                rect: clamp_to_work_area(
                    rect,
                    host.work_area,
                    overlap_area(rect, host.work_area) > 0,
                ),
                monitor_index,
            })
        } else {
            let rect = WindowRect {
                x: centered_axis(primary.work_area.x, primary.work_area.w, self.size.w),
                y: centered_axis(primary.work_area.y, primary.work_area.h, self.size.h),
                w: self.size.w,
                h: self.size.h,
                maximized: self.maximized,
            };
            Some(ResolvedPlacement {
                rect: clamp_to_work_area(rect, primary.work_area, false),
                monitor_index: 0,
            })
        }
    }

    pub fn restore(&self, monitors: &[MonitorDescriptor]) -> Option<WindowRect> {
        self.resolve(monitors).map(|resolved| resolved.rect)
    }
}

impl MonitorDescriptor {
    /// Convert a logical rect resolved onto this monitor into native global physical pixels.
    /// Conversion is monitor-relative so negative origins and mixed-DPI topologies do not borrow
    /// the scale of whichever monitor happened to host the hidden window at creation time.
    pub fn to_physical(&self, rect: WindowRect) -> WindowRect {
        let scale = valid_scale(self.scale_factor);
        let relative_x = i64::from(rect.x).saturating_sub(i64::from(self.work_area.x));
        let relative_y = i64::from(rect.y).saturating_sub(i64::from(self.work_area.y));
        let physical = WindowRect {
            x: clamp_i64_to_i32(
                i64::from(self.physical_work_area.x)
                    .saturating_add(scale_signed(relative_x, scale)),
            ),
            y: clamp_i64_to_i32(
                i64::from(self.physical_work_area.y)
                    .saturating_add(scale_signed(relative_y, scale)),
            ),
            w: scale_unsigned(rect.w, scale),
            h: scale_unsigned(rect.h, scale),
            maximized: rect.maximized,
        };
        clamp_to_work_area(
            physical,
            self.physical_work_area,
            overlap_area(physical, self.physical_work_area) > 0,
        )
    }

    /// Convert a native global rectangle into the monitor-relative logical coordinate space used
    /// by `placement_v2`. This inverse avoids dividing a mixed-DPI desktop-wide origin by one
    /// monitor's scale, which shifts negative/adjacent displays on every save.
    pub fn to_logical(&self, rect: WindowRect) -> WindowRect {
        let scale = valid_scale(self.scale_factor);
        let relative_x = i64::from(rect.x).saturating_sub(i64::from(self.physical_work_area.x));
        let relative_y = i64::from(rect.y).saturating_sub(i64::from(self.physical_work_area.y));
        WindowRect {
            x: add_i32(
                self.work_area.x,
                (relative_x as f64 / scale)
                    .round()
                    .clamp(i32::MIN as f64, i32::MAX as f64) as i32,
            ),
            y: add_i32(
                self.work_area.y,
                (relative_y as f64 / scale)
                    .round()
                    .clamp(i32::MIN as f64, i32::MAX as f64) as i32,
            ),
            w: (f64::from(rect.w) / scale)
                .round()
                .clamp(0.0, u32::MAX as f64) as u32,
            h: (f64::from(rect.h) / scale)
                .round()
                .clamp(0.0, u32::MAX as f64) as u32,
            maximized: rect.maximized,
        }
    }
}

/// Keep an existing native window on a live work area and size it with the target monitor's
/// DPI. The monitor with the greatest physical overlap wins; a window whose display vanished
/// is centered on the primary. `logical_size` is the host-owned normal/content size in DIP,
/// while `margin_dip` reserves an inset for frameless popovers.
pub fn reconcile_physical_rect(
    current: WindowRect,
    logical_size: Size,
    monitors: &[MonitorDescriptor],
    margin_dip: u32,
) -> Option<ReconciledPhysicalRect> {
    let primary = monitors.first()?;
    let monitor_index = best_monitor_index(
        current,
        &monitors
            .iter()
            .map(|monitor| monitor.physical_work_area)
            .collect::<Vec<_>>(),
    );
    let had_overlap = monitor_index.is_some();
    let monitor_index = monitor_index.unwrap_or(0);
    let host = monitors.get(monitor_index).unwrap_or(primary);
    let scale = valid_scale(host.scale_factor);
    let margin = scale_unsigned(margin_dip, scale)
        .min(host.physical_work_area.w.saturating_sub(1) / 2)
        .min(host.physical_work_area.h.saturating_sub(1) / 2);
    let area = inset_rect(host.physical_work_area, margin);
    let desired = WindowRect {
        x: current.x,
        y: current.y,
        w: scale_unsigned(logical_size.w, scale),
        h: scale_unsigned(logical_size.h, scale),
        maximized: current.maximized,
    };
    let rect = clamp_to_work_area(desired, area, had_overlap);
    Some(ReconciledPhysicalRect {
        rect,
        monitor_index,
    })
}

fn valid_scale(scale: f64) -> f64 {
    if scale.is_finite() && scale > 0.0 {
        scale
    } else {
        1.0
    }
}

fn inset_rect(rect: MonitorRect, margin: u32) -> MonitorRect {
    let double_margin = margin.saturating_mul(2);
    MonitorRect {
        x: add_i32(rect.x, margin.min(i32::MAX as u32) as i32),
        y: add_i32(rect.y, margin.min(i32::MAX as u32) as i32),
        w: rect.w.saturating_sub(double_margin),
        h: rect.h.saturating_sub(double_margin),
    }
}

/// Resolve a legacy absolute logical rect against live work areas, including removed-monitor
/// recovery, while retaining the target monitor required for a correct physical conversion.
pub fn resolve_legacy_rect(
    rect: WindowRect,
    monitors: &[MonitorDescriptor],
) -> Option<ResolvedPlacement> {
    monitors.first()?;
    let areas = monitors
        .iter()
        .map(|monitor| monitor.work_area)
        .collect::<Vec<_>>();
    let rect = clamp_to_monitors(rect, &areas);
    let monitor_index = best_monitor_index(rect, &areas).unwrap_or(0);
    Some(ResolvedPlacement {
        rect,
        monitor_index,
    })
}

impl DesktopState {
    /// `desktop.json` inside the tray companion's independent config root.
    pub fn path() -> Option<PathBuf> {
        crate::desktop::persistence::config_dir().map(|dir| dir.join("desktop.json"))
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
        if !path.exists()
            && let Some(legacy) =
                crate::desktop::persistence::legacy_config_dir().map(|dir| dir.join("desktop.json"))
            && legacy.exists()
        {
            // Compatibility read only: never migrate into or modify the playback owner's root.
            return Self::load_legacy_read_only(&legacy);
        }
        crate::util::safe_fs::load_json_or_default_limited(&path, MAX_STATE_BYTES)
    }

    /// Decode legacy tray state without invoking recovery that may rename a core-owned file.
    fn load_legacy_read_only(path: &std::path::Path) -> Self {
        let Ok(bytes) = crate::util::safe_fs::read_no_symlink_limited(path, MAX_STATE_BYTES) else {
            return Self::default();
        };
        if let Ok(state) = serde_json::from_slice::<Self>(&bytes) {
            return state;
        }
        serde_json::from_slice::<serde_json::Value>(&bytes)
            .map(crate::util::safe_fs::recover_lenient::<Self>)
            .unwrap_or_default()
    }

    /// Persist state (best-effort). Atomic temp-write + rename with private perms so a crash
    /// or a concurrent read never sees a half-written file. Output is byte-identical pretty
    /// JSON. Creates the config dir if needed.
    pub fn save(&self) {
        if let Err(error) = self.save_checked() {
            tracing::warn!(target: "ytt_desktop", %error, "could not persist desktop state");
        }
    }

    pub fn save_checked(&self) -> std::io::Result<()> {
        let path = Self::path().ok_or_else(|| {
            std::io::Error::new(
                std::io::ErrorKind::NotFound,
                "desktop state directory is unavailable",
            )
        })?;
        self.save_to(&path)
    }

    pub(crate) fn save_to(&self, path: &std::path::Path) -> std::io::Result<()> {
        crate::util::safe_fs::write_private_atomic_json(path, self)
    }
}

/// `desktop.json` holds only window geometry + a few flags; nothing legitimate approaches
/// this. Bounds a corrupt/hostile file before it's read into memory.
const MAX_STATE_BYTES: u64 = 1024 * 1024;

/// Clamp a saved rect onto the available monitors. Keep it on the monitor with the greatest
/// visible overlap (nudged fully on-screen); when it has no overlap at all, recenter onto the
/// primary (`monitors[0]`). A saved size larger than its host work area is reduced before the
/// position is clamped, so every returned edge is representable and on-screen.
pub fn clamp_to_monitors(rect: WindowRect, monitors: &[MonitorRect]) -> WindowRect {
    let Some(primary) = monitors.first() else {
        return rect;
    };
    let host = best_monitor_index(rect, monitors).map(|index| &monitors[index]);

    let (host, had_overlap) = if let Some(host) = host {
        (host, true)
    } else {
        (primary, false)
    };

    clamp_to_work_area(rect, *host, had_overlap)
}

fn best_monitor_index(rect: WindowRect, monitors: &[MonitorRect]) -> Option<usize> {
    monitors
        .iter()
        .enumerate()
        .filter_map(|monitor| {
            let area = overlap_area(rect, *monitor.1);
            (area > 0).then_some((monitor.0, area))
        })
        // Prefer the earlier monitor on ties; callers put the primary first.
        .reduce(|best, candidate| {
            if candidate.1 > best.1 {
                candidate
            } else {
                best
            }
        })
        .map(|(index, _)| index)
}

fn clamp_to_work_area(rect: WindowRect, host: MonitorRect, had_overlap: bool) -> WindowRect {
    let w = rect.w.min(host.w);
    let h = rect.h.min(host.h);
    let desired_x = if had_overlap {
        i64::from(rect.x)
    } else {
        i64::from(centered_axis(host.x, host.w, w))
    };
    let desired_y = if had_overlap {
        i64::from(rect.y)
    } else {
        i64::from(centered_axis(host.y, host.h, h))
    };
    let min_x = i64::from(host.x);
    let min_y = i64::from(host.y);
    let max_x = min_x.saturating_add(i64::from(host.w.saturating_sub(w)));
    let max_y = min_y.saturating_add(i64::from(host.h.saturating_sub(h)));

    WindowRect {
        x: clamp_i64_to_i32(desired_x.clamp(min_x, max_x)),
        y: clamp_i64_to_i32(desired_y.clamp(min_y, max_y)),
        w,
        h,
        maximized: rect.maximized,
    }
}

fn overlap_area(rect: WindowRect, monitor: MonitorRect) -> i64 {
    let rect_left = i64::from(rect.x);
    let rect_top = i64::from(rect.y);
    let rect_right = rect_left.saturating_add(i64::from(rect.w));
    let rect_bottom = rect_top.saturating_add(i64::from(rect.h));
    let monitor_left = i64::from(monitor.x);
    let monitor_top = i64::from(monitor.y);
    let monitor_right = monitor_left.saturating_add(i64::from(monitor.w));
    let monitor_bottom = monitor_top.saturating_add(i64::from(monitor.h));

    let width = rect_right
        .min(monitor_right)
        .saturating_sub(rect_left.max(monitor_left))
        .max(0);
    let height = rect_bottom
        .min(monitor_bottom)
        .saturating_sub(rect_top.max(monitor_top))
        .max(0);
    width.saturating_mul(height)
}

fn centered_axis(origin: i32, area_size: u32, item_size: u32) -> i32 {
    let offset = i64::from(area_size).saturating_sub(i64::from(item_size)) / 2;
    clamp_i64_to_i32(i64::from(origin).saturating_add(offset))
}

fn add_i32(left: i32, right: i32) -> i32 {
    clamp_i64_to_i32(i64::from(left).saturating_add(i64::from(right)))
}

fn difference_i32(left: i32, right: i32) -> i32 {
    clamp_i64_to_i32(i64::from(left).saturating_sub(i64::from(right)))
}

fn clamp_i64_to_i32(value: i64) -> i32 {
    value.clamp(i64::from(i32::MIN), i64::from(i32::MAX)) as i32
}

fn scale_signed(value: i64, scale: f64) -> i64 {
    let scaled = value as f64 * scale;
    if !scaled.is_finite() {
        return if scaled.is_sign_negative() {
            i64::MIN
        } else {
            i64::MAX
        };
    }
    scaled.round().clamp(i64::MIN as f64, i64::MAX as f64) as i64
}

fn scale_unsigned(value: u32, scale: f64) -> u32 {
    let scaled = value as f64 * scale;
    if !scaled.is_finite() {
        return u32::MAX;
    }
    scaled.round().clamp(0.0, u32::MAX as f64) as u32
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn corrupt_legacy_compatibility_state_is_read_without_mutating_core_storage() {
        let mut random = [0_u8; 8];
        getrandom::fill(&mut random).unwrap();
        let suffix = random
            .into_iter()
            .map(|byte| format!("{byte:02x}"))
            .collect::<String>();
        let directory = std::env::temp_dir().join(format!(
            "yututray-legacy-read-only-{}-{suffix}",
            std::process::id()
        ));
        std::fs::create_dir_all(&directory).unwrap();
        let path = directory.join("desktop.json");
        let corrupt = b"{not-valid-json-with-core-sentinel";
        std::fs::write(&path, corrupt).unwrap();

        assert_eq!(
            DesktopState::load_legacy_read_only(&path),
            DesktopState::default()
        );
        assert_eq!(std::fs::read(&path).unwrap(), corrupt);
        assert!(
            crate::util::safe_fs::recovery_backups(&path)
                .unwrap()
                .is_empty()
        );
        let _ = std::fs::remove_dir_all(directory);
    }

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
    const LEFT: MonitorRect = MonitorRect {
        x: -1600,
        y: -200,
        w: 1600,
        h: 900,
    };

    fn monitor(key: Option<&str>, work_area: MonitorRect) -> MonitorDescriptor {
        MonitorDescriptor {
            key: key.map(str::to_string),
            work_area,
            physical_work_area: work_area,
            scale_factor: 1.0,
        }
    }

    #[test]
    fn defaults_close_to_tray() {
        let s = DesktopState::default();
        assert!(s.close_to_tray);
        assert!(!s.keep_webview_alive);
        assert!(!s.mini_pinned);
        assert!(s.placement_v2.is_empty());
    }

    #[test]
    fn tray_state_save_uses_guarded_atomic_persistence() {
        let dir = std::env::temp_dir().join(format!(
            "yututray-window-state-{}-{}",
            std::process::id(),
            crate::signals::unix_now()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        let path = dir.join("desktop.json");
        let state = DesktopState {
            main: Some(WindowRect {
                x: 10,
                y: 20,
                w: 800,
                h: 600,
                maximized: false,
            }),
            mini: Some(Point { x: 30, y: 40 }),
            mini_pinned: true,
            placement_v2: PlacementV2::default(),
            close_to_tray: false,
            keep_webview_alive: true,
            mini_theme: Some("glass".to_owned()),
        };

        state.save_to(&path).unwrap();
        let restored: DesktopState =
            crate::util::safe_fs::load_json_or_default_limited(&path, MAX_STATE_BYTES);
        assert_eq!(restored, state);

        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn clamp_to_monitors_recovers_legacy_rects() {
        struct Case {
            name: &'static str,
            rect: WindowRect,
            monitors: Vec<MonitorRect>,
            expected: WindowRect,
        }

        let cases = vec![
            Case {
                name: "on screen",
                rect: WindowRect {
                    x: 100,
                    y: 80,
                    w: 1200,
                    h: 800,
                    maximized: false,
                },
                monitors: vec![PRIMARY, SECOND],
                expected: WindowRect {
                    x: 100,
                    y: 80,
                    w: 1200,
                    h: 800,
                    maximized: false,
                },
            },
            Case {
                name: "negative-coordinate monitor",
                rect: WindowRect {
                    x: -1500,
                    y: -100,
                    w: 1000,
                    h: 700,
                    maximized: false,
                },
                monitors: vec![PRIMARY, LEFT],
                expected: WindowRect {
                    x: -1500,
                    y: -100,
                    w: 1000,
                    h: 700,
                    maximized: false,
                },
            },
            Case {
                name: "removed monitor",
                rect: WindowRect {
                    x: 2200,
                    y: 200,
                    w: 1200,
                    h: 800,
                    maximized: false,
                },
                monitors: vec![PRIMARY],
                expected: WindowRect {
                    x: 360,
                    y: 140,
                    w: 1200,
                    h: 800,
                    maximized: false,
                },
            },
            Case {
                name: "partially offscreen",
                rect: WindowRect {
                    x: 1400,
                    y: 900,
                    w: 1200,
                    h: 800,
                    maximized: false,
                },
                monitors: vec![PRIMARY],
                expected: WindowRect {
                    x: 720,
                    y: 280,
                    w: 1200,
                    h: 800,
                    maximized: false,
                },
            },
            Case {
                name: "oversized",
                rect: WindowRect {
                    x: 100,
                    y: 50,
                    w: 3000,
                    h: 2000,
                    maximized: false,
                },
                monitors: vec![PRIMARY],
                expected: WindowRect {
                    x: 0,
                    y: 0,
                    w: 1920,
                    h: 1080,
                    maximized: false,
                },
            },
            Case {
                name: "corrupt integer extremes",
                rect: WindowRect {
                    x: i32::MAX,
                    y: i32::MIN,
                    w: u32::MAX,
                    h: u32::MAX,
                    maximized: true,
                },
                monitors: vec![PRIMARY],
                expected: WindowRect {
                    x: 0,
                    y: 0,
                    w: 1920,
                    h: 1080,
                    maximized: true,
                },
            },
        ];

        for case in cases {
            assert_eq!(
                clamp_to_monitors(case.rect, &case.monitors),
                case.expected,
                "{}",
                case.name
            );
        }
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
            mini_pinned: true,
            placement_v2: PlacementV2 {
                main: Some(WindowPlacement {
                    monitor_key: Some("primary".to_string()),
                    work_area: PRIMARY,
                    origin: Point { x: 1, y: 2 },
                    size: Size { w: 1200, h: 800 },
                    maximized: true,
                }),
                mini: None,
            },
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
        assert!(!s.mini_pinned);
        assert!(s.placement_v2.is_empty());
        // And an absent theme never appears on disk (keeps old builds parsing it).
        assert!(!serde_json::to_string(&s).unwrap().contains("mini_theme"));
    }

    #[test]
    fn placement_restore_recovers_persisted_extremes() {
        struct Case {
            name: &'static str,
            placement: WindowPlacement,
            monitors: Vec<MonitorDescriptor>,
            expected: WindowRect,
        }

        let primary = monitor(Some("primary"), PRIMARY);
        let cases = vec![
            Case {
                name: "matching monitor moved to negative coordinates",
                placement: WindowPlacement {
                    monitor_key: Some("external".into()),
                    work_area: SECOND,
                    origin: Point { x: 200, y: 120 },
                    size: Size { w: 1000, h: 700 },
                    maximized: false,
                },
                monitors: vec![
                    primary.clone(),
                    monitor(
                        Some("external"),
                        MonitorRect {
                            x: -2560,
                            y: 200,
                            w: 2560,
                            h: 1440,
                        },
                    ),
                ],
                expected: WindowRect {
                    x: -2360,
                    y: 320,
                    w: 1000,
                    h: 700,
                    maximized: false,
                },
            },
            Case {
                name: "removed monitor",
                placement: WindowPlacement {
                    monitor_key: Some("gone".into()),
                    work_area: SECOND,
                    origin: Point { x: 200, y: 100 },
                    size: Size { w: 1000, h: 700 },
                    maximized: false,
                },
                monitors: vec![primary.clone()],
                expected: WindowRect {
                    x: 460,
                    y: 190,
                    w: 1000,
                    h: 700,
                    maximized: false,
                },
            },
            Case {
                name: "oversized placement stays on its matching monitor",
                placement: WindowPlacement {
                    monitor_key: Some("external".into()),
                    work_area: SECOND,
                    origin: Point { x: -1920, y: 80 },
                    size: Size { w: 4000, h: 3000 },
                    maximized: true,
                },
                monitors: vec![primary.clone(), monitor(Some("external"), SECOND)],
                expected: WindowRect {
                    x: 1920,
                    y: 0,
                    w: 1920,
                    h: 1080,
                    maximized: true,
                },
            },
            Case {
                name: "corrupt origin and size",
                placement: WindowPlacement {
                    monitor_key: Some("primary".into()),
                    work_area: PRIMARY,
                    origin: Point {
                        x: i32::MAX,
                        y: i32::MIN,
                    },
                    size: Size {
                        w: u32::MAX,
                        h: u32::MAX,
                    },
                    maximized: false,
                },
                monitors: vec![primary],
                expected: WindowRect {
                    x: 0,
                    y: 0,
                    w: 1920,
                    h: 1080,
                    maximized: false,
                },
            },
        ];

        for case in cases {
            assert_eq!(
                case.placement.restore(&case.monitors),
                Some(case.expected),
                "{}",
                case.name
            );
        }
    }

    #[test]
    fn capture_saturates_relative_coordinates() {
        let placement = WindowPlacement::capture(
            WindowRect {
                x: i32::MAX,
                y: i32::MIN,
                w: 1200,
                h: 800,
                maximized: false,
            },
            &monitor(
                None,
                MonitorRect {
                    x: i32::MIN,
                    y: i32::MAX,
                    w: 1920,
                    h: 1080,
                },
            ),
        );

        assert_eq!(
            placement.origin,
            Point {
                x: i32::MAX,
                y: i32::MIN
            }
        );
    }

    #[test]
    fn resolved_placement_uses_target_monitor_scale_for_physical_pixels() {
        let monitors = vec![
            monitor(Some("primary"), PRIMARY),
            MonitorDescriptor {
                key: Some("retina".into()),
                work_area: MonitorRect {
                    x: 960,
                    y: -100,
                    w: 1280,
                    h: 720,
                },
                physical_work_area: MonitorRect {
                    x: 1920,
                    y: -200,
                    w: 2560,
                    h: 1440,
                },
                scale_factor: 2.0,
            },
        ];
        let placement = WindowPlacement {
            monitor_key: Some("retina".into()),
            work_area: SECOND,
            origin: Point { x: 100, y: 50 },
            size: Size { w: 398, h: 602 },
            maximized: false,
        };

        let resolved = placement.resolve(&monitors).unwrap();
        assert_eq!(resolved.monitor_index, 1);
        assert_eq!(resolved.rect.x, 1060);
        assert_eq!(resolved.rect.y, -50);
        let physical = monitors[resolved.monitor_index].to_physical(resolved.rect);
        assert_eq!(
            physical,
            WindowRect {
                x: 2120,
                y: -100,
                w: 796,
                h: 1204,
                maximized: false,
            }
        );
        assert_eq!(
            monitors[resolved.monitor_index].to_logical(physical),
            resolved.rect
        );
    }

    #[test]
    fn legacy_rect_on_removed_monitor_recovers_to_primary_with_physical_target() {
        let monitors = vec![MonitorDescriptor {
            key: Some("primary".into()),
            work_area: MonitorRect {
                x: 0,
                y: 0,
                w: 1280,
                h: 720,
            },
            physical_work_area: MonitorRect {
                x: 0,
                y: 0,
                w: 2560,
                h: 1440,
            },
            scale_factor: 2.0,
        }];
        let resolved = resolve_legacy_rect(
            WindowRect {
                x: 5000,
                y: 300,
                w: 398,
                h: 602,
                maximized: false,
            },
            &monitors,
        )
        .unwrap();

        assert_eq!(resolved.monitor_index, 0);
        assert_eq!(resolved.rect.x, 441);
        assert_eq!(resolved.rect.y, 59);
        assert_eq!(
            monitors[0].to_physical(resolved.rect),
            WindowRect {
                x: 882,
                y: 118,
                w: 796,
                h: 1204,
                maximized: false,
            }
        );
    }

    #[test]
    fn live_reconcile_uses_the_overlapping_monitors_scale_and_work_area() {
        let monitors = vec![
            monitor(Some("primary"), PRIMARY),
            MonitorDescriptor {
                key: Some("scaled-side".into()),
                work_area: MonitorRect {
                    x: 1280,
                    y: 40,
                    w: 1280,
                    h: 960,
                },
                // A left taskbar and 150% scaling make physical origins unsuitable for
                // desktop-wide logical conversion; this is the target-native rectangle.
                physical_work_area: MonitorRect {
                    x: 2000,
                    y: 60,
                    w: 1920,
                    h: 1440,
                },
                scale_factor: 1.5,
            },
        ];

        let reconciled = reconcile_physical_rect(
            WindowRect {
                x: 2150,
                y: 120,
                w: 300,
                h: 200,
                maximized: false,
            },
            Size { w: 398, h: 602 },
            &monitors,
            8,
        )
        .unwrap();

        assert_eq!(reconciled.monitor_index, 1);
        assert_eq!(
            reconciled.rect,
            WindowRect {
                x: 2150,
                y: 120,
                w: 597,
                h: 903,
                maximized: false,
            }
        );
    }

    #[test]
    fn live_reconcile_recenters_a_removed_display_on_the_primary() {
        let monitors = vec![MonitorDescriptor {
            key: Some("primary".into()),
            work_area: MonitorRect {
                x: 0,
                y: 0,
                w: 1280,
                h: 720,
            },
            physical_work_area: MonitorRect {
                x: 0,
                y: 0,
                w: 2560,
                h: 1440,
            },
            scale_factor: 2.0,
        }];

        let reconciled = reconcile_physical_rect(
            WindowRect {
                x: 5000,
                y: -900,
                w: 398,
                h: 602,
                maximized: false,
            },
            Size { w: 398, h: 602 },
            &monitors,
            8,
        )
        .unwrap();

        assert_eq!(reconciled.monitor_index, 0);
        assert_eq!(
            reconciled.rect,
            WindowRect {
                x: 882,
                y: 118,
                w: 796,
                h: 1204,
                maximized: false,
            }
        );
    }

    #[test]
    fn live_reconcile_clamps_to_top_and_side_work_area_insets() {
        let monitors = vec![MonitorDescriptor {
            key: None,
            work_area: MonitorRect {
                x: 40,
                y: 30,
                w: 960,
                h: 690,
            },
            physical_work_area: MonitorRect {
                x: 40,
                y: 30,
                w: 960,
                h: 690,
            },
            scale_factor: 1.0,
        }];

        let reconciled = reconcile_physical_rect(
            WindowRect {
                x: 35,
                y: 25,
                w: 306,
                h: 90,
                maximized: false,
            },
            Size { w: 306, h: 90 },
            &monitors,
            8,
        )
        .unwrap();

        assert_eq!(reconciled.rect.x, 48);
        assert_eq!(reconciled.rect.y, 38);
        assert_eq!(reconciled.rect.w, 306);
        assert_eq!(reconciled.rect.h, 90);
    }
}
