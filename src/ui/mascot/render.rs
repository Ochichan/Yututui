use ratatui::Frame;
use ratatui::layout::Rect;
use ratatui::style::{Modifier, Style};

use crate::app::App;
use crate::theme::ThemeRole as R;

use super::asset::{MascotAsset, MascotStyle};

pub fn render(
    frame: &mut Frame,
    app: &App,
    area: Rect,
    asset: &'static MascotAsset,
) -> Option<Rect> {
    let asset = if app.retro_mode() {
        asset.fallback.unwrap_or(asset)
    } else {
        asset
    };

    if asset.frames.is_empty() || area.width < asset.width || area.height < asset.height {
        return None;
    }

    let rect = Rect {
        x: area.x + area.width.saturating_sub(asset.width) / 2,
        y: area.y + area.height.saturating_sub(asset.height) / 2,
        width: asset.width,
        height: asset.height,
    };
    // Rows are written straight into the buffer (no widget clipping), so refuse any rect
    // that hangs off the frame instead of indexing outside it.
    if rect.intersection(frame.area()) != rect {
        return None;
    }
    let frame_data = &asset.frames[current_frame_index(app, asset)];
    let base = resolve_style(app, frame_data.style);
    // Paint only each row's inked span: the margin cells stay untouched so a mascot placed
    // beside text never erases it. Interior blanks still paint, clearing whatever sits
    // under the art's body. Only ASCII spaces count as margin so byte and cell offsets agree;
    // column == char index is guaranteed by the single-width-glyph asset test.
    let buf = frame.buffer_mut();
    let has_regions = !asset.regions.is_empty();
    for (y, line) in frame_data.lines.iter().enumerate() {
        let trimmed = line.trim_end_matches(' ');
        let inked = trimmed.trim_start_matches(' ');
        if inked.is_empty() {
            continue;
        }
        let lead = (trimmed.len() - inked.len()) as u16;
        let row_y = rect.y + y as u16;
        let max_cols = usize::from(asset.width - lead);
        if !has_regions {
            buf.set_stringn(rect.x + lead, row_y, inked, max_cols, base);
            continue;
        }
        for (x, ch) in inked.chars().take(max_cols).enumerate() {
            let ax = lead + x as u16;
            let style = asset
                .regions
                .iter()
                .find(|region| region.contains(ax, y as u16))
                .map_or(base, |region| {
                    let s = resolve_style(app, region.style);
                    if region.bold {
                        s.add_modifier(Modifier::BOLD)
                    } else {
                        s
                    }
                });
            if let Some(cell) = buf.cell_mut((rect.x + ax, row_y)) {
                cell.set_char(ch);
                cell.set_style(style);
            }
        }
    }
    Some(rect)
}

fn resolve_style(app: &App, style: MascotStyle) -> Style {
    match style {
        MascotStyle::Theme(role) => app.theme.style(role),
        MascotStyle::Accent => app.theme.style(R::Accent),
        MascotStyle::Muted => app.theme.style(R::TextMuted),
        MascotStyle::Thinking => app.theme.style(R::AiThinking),
        MascotStyle::Error => app.theme.style(R::AiError),
    }
}

pub fn current_frame_index(app: &App, asset: &MascotAsset) -> usize {
    frame_index_for_tick(app.anim_frame(), app.animation_tick_fps(), asset)
}

pub fn frame_index_for_tick(anim_frame: u64, tick_fps: u16, asset: &MascotAsset) -> usize {
    if asset.frames.is_empty() {
        return 0;
    }

    let total_hold: u64 = asset
        .frames
        .iter()
        .map(|frame| u64::from(frame.hold.max(1)))
        .sum();
    if total_hold == 0 {
        return 0;
    }

    let tick_fps = u64::from(tick_fps.max(1));
    let asset_fps = u64::from(asset.fps.max(1));
    let mut t = anim_frame.saturating_mul(asset_fps) / tick_fps;
    if asset.looped {
        t %= total_hold;
    } else {
        t = t.min(total_hold.saturating_sub(1));
    }

    for (idx, frame) in asset.frames.iter().enumerate() {
        let hold = u64::from(frame.hold.max(1));
        if t < hold {
            return idx;
        }
        t = t.saturating_sub(hold);
    }
    asset.frames.len().saturating_sub(1)
}

#[cfg(test)]
mod tests {
    use ratatui::Terminal;
    use ratatui::backend::TestBackend;

    use super::*;
    use crate::app::App;
    use crate::ui::mascot::asset::MascotFrame;

    static TEST_FRAMES: [MascotFrame; 3] = [
        MascotFrame {
            hold: 1,
            lines: &["."],
            style: MascotStyle::Accent,
        },
        MascotFrame {
            hold: 2,
            lines: &["+"],
            style: MascotStyle::Accent,
        },
        MascotFrame {
            hold: 1,
            lines: &["#"],
            style: MascotStyle::Accent,
        },
    ];

    static LOOPED: MascotAsset = MascotAsset {
        name: "test_looped",
        width: 1,
        height: 1,
        fps: 3,
        looped: true,
        frames: &TEST_FRAMES,
        fallback: None,
        regions: &[],
    };

    static ONCE: MascotAsset = MascotAsset {
        name: "test_once",
        width: 1,
        height: 1,
        fps: 3,
        looped: false,
        frames: &TEST_FRAMES,
        fallback: None,
        regions: &[],
    };

    #[test]
    fn frame_index_respects_hold() {
        assert_eq!(frame_index_for_tick(0, 3, &LOOPED), 0);
        assert_eq!(frame_index_for_tick(1, 3, &LOOPED), 1);
        assert_eq!(frame_index_for_tick(2, 3, &LOOPED), 1);
        assert_eq!(frame_index_for_tick(3, 3, &LOOPED), 2);
        assert_eq!(frame_index_for_tick(4, 3, &LOOPED), 0);
    }

    #[test]
    fn frame_index_respects_30fps_app_tick_for_3fps_assets() {
        assert_eq!(frame_index_for_tick(0, 30, &LOOPED), 0);
        assert_eq!(frame_index_for_tick(9, 30, &LOOPED), 0);
        assert_eq!(frame_index_for_tick(10, 30, &LOOPED), 1);
        assert_eq!(frame_index_for_tick(29, 30, &LOOPED), 1);
        assert_eq!(frame_index_for_tick(30, 30, &LOOPED), 2);
        assert_eq!(frame_index_for_tick(40, 30, &LOOPED), 0);
    }

    #[test]
    fn frame_index_respects_looped_false() {
        assert_eq!(frame_index_for_tick(99, 3, &ONCE), 2);
    }

    #[test]
    fn regions_render_with_their_own_colors() {
        use crate::ui::mascot::generated::cat_laptop::CAT_LAPTOP_IDLE;

        let mut app = App::new(100);
        let backend = TestBackend::new(30, 20);
        let mut terminal = Terminal::new(backend).unwrap();
        let area = Rect {
            x: 0,
            y: 0,
            width: 30,
            height: 20,
        };
        terminal
            .draw(|frame| {
                assert!(render(frame, &app, area, &CAT_LAPTOP_IDLE).is_some());
            })
            .unwrap();

        // The asset centers in the 30x20 area: origin (3, 2). A cell inside the cat-head
        // region (asset 15,4 -> buffer 18,6) must differ in style from one inside the
        // laptop region (asset 10,9 -> buffer 13,11) — proof the span splitting works.
        let buffer = terminal.backend().buffer();
        let cat = buffer[(18u16, 6u16)].style();
        let laptop = buffer[(13u16, 11u16)].style();
        assert_ne!(cat, laptop, "region colors should differ");
        app.dirty = false;
    }

    static MARGIN_FRAMES: [MascotFrame; 1] = [MascotFrame {
        hold: 1,
        lines: &["    ", " ab ", "    "],
        style: MascotStyle::Accent,
    }];

    static MARGIN: MascotAsset = MascotAsset {
        name: "test_margin",
        width: 4,
        height: 3,
        fps: 3,
        looped: true,
        frames: &MARGIN_FRAMES,
        fallback: None,
        regions: &[],
    };

    #[test]
    fn blank_margins_leave_underlying_cells_alone() {
        let mut app = App::new(100);
        let backend = TestBackend::new(4, 3);
        let mut terminal = Terminal::new(backend).unwrap();
        let area = Rect {
            x: 0,
            y: 0,
            width: 4,
            height: 3,
        };
        terminal
            .draw(|frame| {
                let buf = frame.buffer_mut();
                for y in 0..3 {
                    for x in 0..4 {
                        buf[(x, y)].set_symbol("z");
                    }
                }
                assert!(render(frame, &app, area, &MARGIN).is_some());
            })
            .unwrap();
        let buffer = terminal.backend().buffer();
        let row = |y: u16| -> String { (0..4).map(|x| buffer[(x, y)].symbol()).collect() };
        assert_eq!(row(0), "zzzz", "all-blank rows must not paint");
        assert_eq!(row(1), "zabz", "only the inked span paints");
        assert_eq!(row(2), "zzzz");
        app.dirty = false;
    }

    #[test]
    fn rect_outside_the_frame_is_refused() {
        let mut app = App::new(100);
        let backend = TestBackend::new(4, 3);
        let mut terminal = Terminal::new(backend).unwrap();
        terminal
            .draw(|frame| {
                let area = Rect {
                    x: 0,
                    y: 2,
                    width: 4,
                    height: 3,
                };
                assert_eq!(render(frame, &app, area, &MARGIN), None);
            })
            .unwrap();
        app.dirty = false;
    }

    #[test]
    fn small_area_does_not_render_or_panic() {
        let mut app = App::new(100);
        let backend = TestBackend::new(2, 2);
        let mut terminal = Terminal::new(backend).unwrap();
        terminal
            .draw(|frame| {
                let area = Rect {
                    x: 0,
                    y: 0,
                    width: 0,
                    height: 0,
                };
                assert_eq!(render(frame, &app, area, &LOOPED), None);
            })
            .unwrap();
        app.dirty = false;
    }
}
