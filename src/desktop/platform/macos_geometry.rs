//! AppKit-to-tao screen geometry conversion for the macOS desktop shell.

use crate::desktop::window_state::MonitorRect;

/// Physical work area of the macOS screen containing (or nearest to) a tao
/// physical-pixel point. AppKit reports global screen rectangles in Cocoa's
/// bottom-left, point-based coordinate space, while tao exposes a top-left
/// physical-pixel space. Keeping this conversion here gives both desktop
/// windows the same menu-bar/Dock exclusion without depending on private tao
/// implementation details.
///
/// This is called from tao's main event loop. `NSScreen` is main-thread-only,
/// so a call from any other thread safely declines to provide a result.
pub(crate) fn work_area_for_point(point: (f64, f64)) -> Option<MonitorRect> {
    use objc2::MainThreadMarker;
    use objc2_app_kit::NSScreen;

    let mtm = MainThreadMarker::new()?;
    let screens = NSScreen::screens(mtm);
    // Per AppKit, the first screen is the one containing the menu bar. Its
    // upper edge is the global axis needed to flip Cocoa Y into Quartz/tao Y;
    // `mainScreen` is intentionally not used because it follows keyboard focus.
    let primary_frame = screens.firstObject()?.frame();
    let desktop_top = primary_frame.origin.y + primary_frame.size.height;
    let candidates = screens
        .iter()
        .filter_map(|screen| {
            ScreenGeometry::from_cocoa(
                CocoaRect::from_ns(screen.frame()),
                CocoaRect::from_ns(screen.visibleFrame()),
                desktop_top,
                screen.backingScaleFactor(),
            )
        })
        .collect::<Vec<_>>();
    nearest_screen(point, &candidates).map(|screen| screen.work.to_monitor_rect())
}

#[derive(Debug, Clone, Copy, PartialEq)]
struct CocoaRect {
    x: f64,
    y: f64,
    w: f64,
    h: f64,
}

impl CocoaRect {
    fn from_ns(rect: objc2_foundation::NSRect) -> Self {
        Self {
            x: rect.origin.x,
            y: rect.origin.y,
            w: rect.size.width,
            h: rect.size.height,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq)]
struct PhysicalRect {
    x: f64,
    y: f64,
    w: f64,
    h: f64,
}

impl PhysicalRect {
    fn center_distance_squared(self, (x, y): (f64, f64)) -> f64 {
        let dx = x - (self.x + self.w / 2.0);
        let dy = y - (self.y + self.h / 2.0);
        dx * dx + dy * dy
    }

    fn to_monitor_rect(self) -> MonitorRect {
        MonitorRect {
            x: self.x.round() as i32,
            y: self.y.round() as i32,
            w: self.w.round().max(0.0) as u32,
            h: self.h.round().max(0.0) as u32,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq)]
struct ScreenGeometry {
    frame: PhysicalRect,
    work: PhysicalRect,
}

impl ScreenGeometry {
    fn from_cocoa(
        frame: CocoaRect,
        visible: CocoaRect,
        desktop_top: f64,
        scale: f64,
    ) -> Option<Self> {
        if !desktop_top.is_finite()
            || !scale.is_finite()
            || scale <= 0.0
            || !valid_cocoa_rect(frame)
            || !valid_cocoa_rect(visible)
        {
            return None;
        }
        Some(Self {
            frame: cocoa_to_tao(frame, desktop_top, scale),
            work: cocoa_to_tao(visible, desktop_top, scale),
        })
    }
}

fn valid_cocoa_rect(rect: CocoaRect) -> bool {
    rect.x.is_finite()
        && rect.y.is_finite()
        && rect.w.is_finite()
        && rect.h.is_finite()
        && rect.w > 0.0
        && rect.h > 0.0
}

/// Flip a global Cocoa rectangle around the menu-bar screen's upper edge, then
/// apply this screen's backing scale. Scaling each screen independently mirrors
/// tao's mixed-DPI monitor positions instead of assuming one desktop-wide DPI.
fn cocoa_to_tao(rect: CocoaRect, desktop_top: f64, scale: f64) -> PhysicalRect {
    PhysicalRect {
        x: rect.x * scale,
        y: (desktop_top - (rect.y + rect.h)) * scale,
        w: rect.w * scale,
        h: rect.h * scale,
    }
}

fn nearest_screen(point: (f64, f64), screens: &[ScreenGeometry]) -> Option<ScreenGeometry> {
    if !point.0.is_finite() || !point.1.is_finite() {
        return None;
    }
    screens.iter().copied().min_by(|left, right| {
        left.frame
            .center_distance_squared(point)
            .total_cmp(&right.frame.center_distance_squared(point))
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cocoa_geometry_flips_y_and_applies_per_screen_scale() {
        let screen = ScreenGeometry::from_cocoa(
            CocoaRect {
                x: 0.0,
                y: 0.0,
                w: 1440.0,
                h: 900.0,
            },
            // 24pt menu bar at the top and 50pt Dock at the bottom.
            CocoaRect {
                x: 0.0,
                y: 50.0,
                w: 1440.0,
                h: 826.0,
            },
            900.0,
            2.0,
        )
        .unwrap();
        assert_eq!(
            screen.frame,
            PhysicalRect {
                x: 0.0,
                y: 0.0,
                w: 2880.0,
                h: 1800.0,
            }
        );
        assert_eq!(
            screen.work,
            PhysicalRect {
                x: 0.0,
                y: 48.0,
                w: 2880.0,
                h: 1652.0,
            }
        );
    }

    #[test]
    fn cocoa_geometry_handles_negative_and_mixed_dpi_monitor_origins() {
        // A taller 1x screen is bottom-aligned to the right of a 900pt 2x
        // menu-bar screen. Cocoa therefore gives it a negative bottom origin;
        // its top maps to tao y=0 without borrowing the primary's 2x scale.
        let external = ScreenGeometry::from_cocoa(
            CocoaRect {
                x: 1440.0,
                y: -180.0,
                w: 1920.0,
                h: 1080.0,
            },
            CocoaRect {
                x: 1440.0,
                y: -180.0,
                w: 1920.0,
                h: 1040.0,
            },
            900.0,
            1.0,
        )
        .unwrap();
        assert_eq!(
            external.frame,
            PhysicalRect {
                x: 1440.0,
                y: 0.0,
                w: 1920.0,
                h: 1080.0,
            }
        );
        assert_eq!(external.work.y, 40.0);

        let above = ScreenGeometry::from_cocoa(
            CocoaRect {
                x: -1280.0,
                y: 900.0,
                w: 1280.0,
                h: 800.0,
            },
            CocoaRect {
                x: -1280.0,
                y: 900.0,
                w: 1280.0,
                h: 800.0,
            },
            900.0,
            1.5,
        )
        .unwrap();
        assert_eq!(above.frame.x, -1920.0);
        assert_eq!(above.frame.y, -1200.0);
    }

    #[test]
    fn screen_matching_uses_nearest_physical_center() {
        let left = ScreenGeometry {
            frame: PhysicalRect {
                x: -1000.0,
                y: 0.0,
                w: 1000.0,
                h: 800.0,
            },
            work: PhysicalRect {
                x: -1000.0,
                y: 20.0,
                w: 1000.0,
                h: 780.0,
            },
        };
        let right = ScreenGeometry {
            frame: PhysicalRect {
                x: 0.0,
                y: 0.0,
                w: 1200.0,
                h: 900.0,
            },
            work: PhysicalRect {
                x: 0.0,
                y: 24.0,
                w: 1200.0,
                h: 876.0,
            },
        };
        assert_eq!(nearest_screen((-500.0, 400.0), &[left, right]), Some(left));
        assert_eq!(nearest_screen((500.0, 400.0), &[left, right]), Some(right));
        // A point in a topology gap still gets a deterministic nearest screen.
        assert_eq!(
            nearest_screen((-100.0, 2_000.0), &[left, right]),
            Some(left)
        );
        assert_eq!(nearest_screen((f64::NAN, 0.0), &[left]), None);
    }

    #[test]
    fn center_matching_disambiguates_overlapping_mixed_dpi_spaces() {
        // tao applies each monitor's own scale to its global origin. A 2x
        // primary followed by a 1x external screen can therefore overlap in
        // tao's physical coordinate space. Their center points remain exact,
        // so center matching (rather than first-containing-frame matching)
        // selects the intended NSScreen.
        let primary = ScreenGeometry::from_cocoa(
            CocoaRect {
                x: 0.0,
                y: 0.0,
                w: 1440.0,
                h: 900.0,
            },
            CocoaRect {
                x: 0.0,
                y: 0.0,
                w: 1440.0,
                h: 900.0,
            },
            900.0,
            2.0,
        )
        .unwrap();
        let external = ScreenGeometry::from_cocoa(
            CocoaRect {
                x: 1440.0,
                y: 0.0,
                w: 1920.0,
                h: 1080.0,
            },
            CocoaRect {
                x: 1440.0,
                y: 0.0,
                w: 1920.0,
                h: 1040.0,
            },
            900.0,
            1.0,
        )
        .unwrap();

        let external_center = (
            external.frame.x + external.frame.w / 2.0,
            external.frame.y + external.frame.h / 2.0,
        );
        assert!(external_center.0 < primary.frame.x + primary.frame.w);
        assert_eq!(
            nearest_screen(external_center, &[primary, external]),
            Some(external)
        );
    }
}
