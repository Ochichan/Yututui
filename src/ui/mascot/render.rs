use ratatui::Frame;
use ratatui::layout::{Alignment, Rect};
use ratatui::text::Line;
use ratatui::widgets::Paragraph;

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
    let frame_data = &asset.frames[current_frame_index(app, asset)];
    let style = match frame_data.style {
        MascotStyle::Theme(role) => app.theme.style(role),
        MascotStyle::Accent => app.theme.style(R::Accent),
        MascotStyle::Muted => app.theme.style(R::TextMuted),
        MascotStyle::Thinking => app.theme.style(R::AiThinking),
        MascotStyle::Error => app.theme.style(R::AiError),
    };
    let lines = frame_data
        .lines
        .iter()
        .map(|line| Line::from(*line).style(style).alignment(Alignment::Center))
        .collect::<Vec<_>>();

    frame.render_widget(Paragraph::new(lines), rect);
    Some(rect)
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
    };

    static ONCE: MascotAsset = MascotAsset {
        name: "test_once",
        width: 1,
        height: 1,
        fps: 3,
        looped: false,
        frames: &TEST_FRAMES,
        fallback: None,
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
