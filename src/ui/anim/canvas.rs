//! First-wave filler-canvas effects — matrix rain, the starfield, the faux visualizer, the
//! spinning donut and the bouncing logo — plus [`render_canvas`], the single dispatcher every
//! blank filler zone is painted through (back-to-front across this module, `canvas_ext` and
//! `canvas_sim`). Split out of the parent module verbatim when the second wave landed.

use std::cell::RefCell;
use std::sync::OnceLock;

use ratatui::Frame;
use ratatui::layout::Rect;
use ratatui::style::{Color, Modifier, Style};

use super::{bar_blocks, canvas_ext, canvas_sim, hash32, lerp_color, put_char, wave};
use crate::app::App;
use crate::theme::ThemeRole as R;

/// Draw all enabled canvas effects into a blank `zone`, back to front: full-field washes
/// first (plasma, Life, pipes), then weather and sky (waves, rain, snow, starfield,
/// fireflies, comets), then the scene pieces (aquarium, visualizer, fireworks, cube, donut)
/// and finally the bouncing logo on top. Called only for the blank filler region, so it never
/// overdraws album art or lyrics.
pub fn render_canvas(frame: &mut Frame, app: &App, zone: Rect) {
    let a = app.animations();
    if !a.master || zone.width < 4 || zone.height < 2 {
        return;
    }
    let f = app.anim_frame();
    if a.plasma {
        canvas_sim::plasma(frame, app, zone, f);
    }
    if a.life {
        canvas_sim::life(frame, app, zone, f);
    }
    if a.pipes {
        canvas_sim::pipes(frame, app, zone, f);
    }
    if a.waves {
        canvas_ext::waves(frame, app, zone, f);
    }
    if a.rain {
        rain(frame, app, zone, f);
    }
    if a.snow {
        canvas_ext::snow(frame, app, zone, f);
    }
    if a.starfield {
        starfield(frame, app, zone, f);
    }
    if a.fireflies {
        canvas_ext::fireflies(frame, app, zone, f);
    }
    if a.comets {
        canvas_ext::comets(frame, app, zone, f);
    }
    if a.aquarium {
        canvas_ext::aquarium(frame, app, zone, f);
    }
    if a.visualizer {
        visualizer(frame, app, zone, f);
    }
    if a.fireworks {
        canvas_sim::fireworks(frame, app, zone, f);
    }
    if a.cube {
        canvas_ext::cube(frame, app, zone, f);
    }
    if a.donut {
        donut(frame, app, zone, f);
    }
    if a.bounce {
        bounce(frame, app, zone, f);
    }
}

const RAIN_GLYPHS: [char; 22] = [
    '0', '1', '7', '9', '=', '+', '*', '/', '<', '>', ':', '.', '#', '$', '%', '&', '?', 'Z', 'X',
    'A', 'V', '|',
];

#[derive(Clone, Copy)]
struct RainColumn {
    col: u64,
    speed: u64,
    len: u64,
    period: u64,
    offset: u64,
}

#[derive(Default)]
struct RainScratch {
    width: u16,
    height: u16,
    columns: Vec<RainColumn>,
}

thread_local! {
    static RAIN_SCRATCH: RefCell<RainScratch> = RefCell::new(RainScratch::default());
}

/// Classic matrix digital rain: each column is an independently-falling head with a fading green
/// trail. Speed / length / phase are hashed from the column index so columns desync but stay
/// stable frame to frame.
fn rain(frame: &mut Frame, _app: &App, zone: Rect, f: u64) {
    let h = u64::from(zone.height);
    RAIN_SCRATCH.with(|scratch| {
        let mut scratch = scratch.borrow_mut();
        if scratch.width != zone.width || scratch.height != zone.height {
            scratch.width = zone.width;
            scratch.height = zone.height;
            scratch.columns.clear();
            scratch.columns.reserve(usize::from(zone.width));
            let h_mod = h.max(1) as u32;
            for col in 0..u64::from(zone.width) {
                let speed = 1 + u64::from(hash32(col) % 3);
                let len = 4 + u64::from(hash32(col.wrapping_mul(2_654_435_761)) % h_mod);
                let period = h + len + 3;
                let offset = u64::from(hash32(col ^ 0x9E37_79B9)) % period;
                scratch.columns.push(RainColumn {
                    col,
                    speed,
                    len,
                    period,
                    offset,
                });
            }
        }

        let buf = frame.buffer_mut();
        for (i, column) in scratch.columns.iter().enumerate() {
            let x = zone.left() + i as u16;
            let head = (f / column.speed + column.offset) % column.period;
            for k in 0..=column.len {
                if head < k {
                    continue;
                }
                let row = head - k;
                if row >= h {
                    continue;
                }
                let y = zone.top() + row as u16;
                let g = RAIN_GLYPHS[(hash32(
                    column
                        .col
                        .wrapping_mul(31)
                        .wrapping_add(row)
                        .wrapping_add(f / 6),
                ) as usize)
                    % RAIN_GLYPHS.len()];
                let color = if k == 0 {
                    Color::Rgb(200, 255, 200)
                } else {
                    let b = 1.0 - k as f64 / column.len as f64;
                    Color::Rgb((30.0 * b) as u8, (60.0 + 180.0 * b) as u8, (40.0 * b) as u8)
                };
                put_char(buf, x, y, g, Style::default().fg(color));
            }
        }
    });
}

const STAR_GLYPHS: [char; 8] = ['·', '✦', '✧', '*', '+', '.', '°', '⋆'];

#[derive(Clone, Copy)]
struct Star {
    x: u16,
    speed: u64,
    seed: u64,
}

#[derive(Default)]
struct StarScratch {
    width: u16,
    height: u16,
    count: u32,
    stars: Vec<Star>,
}

thread_local! {
    static STAR_SCRATCH: RefCell<StarScratch> = RefCell::new(StarScratch::default());
}

/// Drifting stars / musical sparkles rising slowly up the zone, each twinkling between a subtle and
/// an accent colour. Density scales with the zone area.
fn starfield(frame: &mut Frame, app: &App, zone: Rect, f: u64) {
    let w = u32::from(zone.width);
    let h = u64::from(zone.height);
    let count = ((w * u32::from(zone.height)) / 40).clamp(6, 60);
    let dim = app.theme.color(R::TextSubtle);
    let bright = app.theme.color(R::AccentAlt);
    let wave24: [f64; 24] = std::array::from_fn(|i| wave(i as u64, 24));
    STAR_SCRATCH.with(|scratch| {
        let mut scratch = scratch.borrow_mut();
        if scratch.width != zone.width || scratch.height != zone.height || scratch.count != count {
            scratch.width = zone.width;
            scratch.height = zone.height;
            scratch.count = count;
            scratch.stars.clear();
            scratch.stars.reserve(count as usize);
            for i in 0..u64::from(count) {
                let x = (hash32(i * 2 + 1) % w) as u16;
                let speed = 1 + u64::from(hash32(i * 3 + 2) % 4);
                let seed = u64::from(hash32(i * 5 + 3));
                scratch.stars.push(Star { x, speed, seed });
            }
        }

        let buf = frame.buffer_mut();
        for (i, star) in scratch.stars.iter().enumerate() {
            let i = i as u64;
            let yv = (f / star.speed + star.seed) % (h + 4);
            if yv >= h {
                continue; // off-screen pause → a twinkle gap
            }
            let y = zone.top() + (h - 1 - yv) as u16;
            let g = STAR_GLYPHS[(hash32(i * 7 + f / 20) as usize) % STAR_GLYPHS.len()];
            let color = lerp_color(dim, bright, wave24[((f + i * 5) % 24) as usize]);
            put_char(buf, zone.left() + star.x, y, g, Style::default().fg(color));
        }
    });
}

/// A decorative (non-audio-reactive) spectrum: bars rising from the bottom strip of the zone, each
/// mixing two waves, coloured from the gauge colour up to the accent.
fn visualizer(frame: &mut Frame, app: &App, zone: Rect, f: u64) {
    let blocks = bar_blocks(app);
    let strip = (zone.height / 3).clamp(2, 8);
    let baseline = i32::from(zone.bottom());
    let top = i32::from(zone.top());
    let filled = app.theme.color(R::GaugeFilled);
    let accent = app.theme.color(R::Accent);
    let wave14: [f64; 14] = std::array::from_fn(|i| wave(i as u64, 14));
    let wave9: [f64; 9] = std::array::from_fn(|i| wave(i as u64, 9));
    let row_styles: [Style; 8] = std::array::from_fn(|i| {
        let row = i as u16;
        if row < strip {
            Style::default().fg(lerp_color(
                filled,
                accent,
                f64::from(row + 1) / f64::from(strip),
            ))
        } else {
            Style::default()
        }
    });
    let buf = frame.buffer_mut();
    for bx in 0..zone.width {
        let x = zone.left() + bx;
        let phase = u64::from(bx) * 3;
        let t =
            wave14[((f + phase) % 14) as usize] * 0.6 + wave9[((f * 2 + phase) % 9) as usize] * 0.4;
        let cells = t * f64::from(strip);
        let full = cells.floor() as u16;
        let frac = cells - f64::from(full);
        for row in 0..strip {
            let yy = baseline - 1 - i32::from(row);
            if yy < top {
                break;
            }
            let y = yy as u16;
            let style = row_styles[row as usize];
            if row < full {
                put_char(buf, x, y, '█', style);
            } else if row == full && frac > 0.05 {
                let idx = (frac * (blocks.len() - 1) as f64).round() as usize;
                put_char(buf, x, y, blocks[idx.min(blocks.len() - 1)], style);
            }
        }
    }
}

const DONUT_LUM: &[u8; 12] = b".,-~:;=!*#$@";

#[derive(Default)]
struct DonutScratch {
    zbuf: Vec<f64>,
    out: Vec<u8>,
}

thread_local! {
    static DONUT_SCRATCH: RefCell<DonutScratch> = RefCell::new(DonutScratch::default());
}

fn donut_trig_steps() -> &'static [(f64, f64)] {
    static STEPS: OnceLock<Vec<(f64, f64)>> = OnceLock::new();
    STEPS.get_or_init(|| {
        let mut steps = Vec::new();
        let mut angle = 0.0f64;
        while angle < std::f64::consts::TAU {
            steps.push(angle.sin_cos());
            angle += 0.06;
        }
        steps
    })
}

/// The classic spinning ASCII torus (Andy Sloane's donut), z-buffered into the centre of the zone.
/// Two rotation angles advance with the frame counter. Luminance picks a glyph and a colour ramp
/// from accent to alt-accent.
fn donut(frame: &mut Frame, app: &App, zone: Rect, f: u64) {
    let w = i32::from(zone.width);
    let h = i32::from(zone.height);
    if w < 8 || h < 5 {
        return;
    }
    let (sa, ca) = (f as f64 * 0.04).sin_cos();
    let (sb, cb) = (f as f64 * 0.02).sin_cos();
    let cells = (w * h) as usize;
    let k1 = (f64::from(w) * 0.32).min(f64::from(h) * 0.62);
    let k2 = 5.0;
    let steps = donut_trig_steps();
    let base = app.theme.color(R::Accent);
    let bright = app.theme.color(R::AccentAlt);
    let styles: [Style; 12] = std::array::from_fn(|ci| {
        let t = ci as f64 / (DONUT_LUM.len() - 1) as f64;
        Style::default().fg(lerp_color(base, bright, t))
    });

    DONUT_SCRATCH.with(|scratch| {
        let mut scratch = scratch.borrow_mut();
        if scratch.zbuf.len() != cells {
            scratch.zbuf.resize(cells, 0.0);
            scratch.out.resize(cells, 0);
        } else {
            scratch.zbuf.fill(0.0);
            scratch.out.fill(0);
        }

        for &(st, ct) in steps {
            for &(sp, cp) in steps {
                let circle_x = 2.0 + ct; // R2 + R1*cos(theta), R1 = 1, R2 = 2
                let circle_y = st;
                let x = circle_x * (cb * cp + sa * sb * sp) - circle_y * ca * sb;
                let y = circle_x * (sb * cp - sa * cb * sp) + circle_y * ca * cb;
                let z = k2 + ca * circle_x * sp + circle_y * sa;
                let ooz = 1.0 / z;
                let xp = (f64::from(w) / 2.0 + k1 * ooz * x) as i32;
                let yp = (f64::from(h) / 2.0 - k1 * ooz * y * 0.5) as i32;
                if xp >= 0 && xp < w && yp >= 0 && yp < h {
                    let idx = (xp + yp * w) as usize;
                    if ooz > scratch.zbuf[idx] {
                        let lum =
                            cp * ct * sb - ca * ct * sp - sa * st + cb * (ca * st - ct * sa * sp);
                        scratch.zbuf[idx] = ooz;
                        let li = if lum > 0.0 { (lum * 8.0) as usize } else { 0 };
                        scratch.out[idx] = (li.min(DONUT_LUM.len() - 1) + 1) as u8;
                    }
                }
            }
        }

        let buf = frame.buffer_mut();
        for yy in 0..h {
            for xx in 0..w {
                let v = scratch.out[(xx + yy * w) as usize];
                if v == 0 {
                    continue;
                }
                let ci = (v as usize - 1).min(DONUT_LUM.len() - 1);
                put_char(
                    buf,
                    zone.left() + xx as u16,
                    zone.top() + yy as u16,
                    DONUT_LUM[ci] as char,
                    styles[ci],
                );
            }
        }
    });
}

/// DVD-style bouncing logo: an ASCII tag ricochets around the zone on a triangle-wave path, its
/// colour cycling each time it crosses a wall.
fn bounce(frame: &mut Frame, app: &App, zone: Rect, f: u64) {
    const LABEL: &str = "<yututui>";
    let tw = LABEL.chars().count() as i64;
    let w = i64::from(zone.width);
    let h = i64::from(zone.height);
    if w <= tw || h < 2 {
        return;
    }
    let span_x = (w - tw).max(1);
    let span_y = (h - 1).max(1);
    // Triangle wave in `0..=span`.
    let tri = |t: i64, span: i64| -> i64 {
        let period = span * 2;
        let m = t.rem_euclid(period);
        if m < span { m } else { period - m }
    };
    let t = f as i64;
    let x = i64::from(zone.left()) + tri(t / 2, span_x);
    let y = i64::from(zone.top()) + tri(t / 3, span_y);
    let cyc = (t / span_x.max(1) + t / span_y.max(1)) as usize;
    const PALETTE: [R; 5] = [R::Accent, R::AccentAlt, R::Success, R::Warning, R::Error];
    let color = app.theme.color(PALETTE[cyc % PALETTE.len()]);
    let right = i64::from(zone.right());
    let buf = frame.buffer_mut();
    for (i, ch) in LABEL.chars().enumerate() {
        let cx = x + i as i64;
        if cx < i64::from(zone.left()) || cx >= right {
            continue;
        }
        put_char(
            buf,
            cx as u16,
            y as u16,
            ch,
            Style::default().fg(color).add_modifier(Modifier::BOLD),
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::app::Msg;
    use ratatui::Terminal;
    use ratatui::backend::TestBackend;
    use ratatui::buffer::Buffer;

    /// Enable exactly one canvas flag on a fresh app.
    fn app_with(set: impl Fn(&mut crate::config::AnimationsConfig)) -> App {
        let mut app = App::new(100);
        app.config.animations.master = true;
        set(&mut app.config.animations);
        app
    }

    fn render_zone(app: &App, zone: Rect) -> Buffer {
        let backend = TestBackend::new(50, 20);
        let mut terminal = Terminal::new(backend).unwrap();
        terminal.draw(|f| render_canvas(f, app, zone)).unwrap();
        terminal.backend().buffer().clone()
    }

    /// Every canvas effect must (a) never write a cell outside the zone it was given, and
    /// (b) actually paint something inside it within a few hundred frames. One sweep covers
    /// all 15 flags, so a new effect added to `render_canvas` is tested by construction.
    #[test]
    fn every_canvas_effect_paints_inside_the_zone_only() {
        let _guard = crate::i18n::lock_for_test();
        type SetFlag = fn(&mut crate::config::AnimationsConfig);
        let flags: [(&str, SetFlag); 15] = [
            ("rain", |a| a.rain = true),
            ("donut", |a| a.donut = true),
            ("visualizer", |a| a.visualizer = true),
            ("starfield", |a| a.starfield = true),
            ("bounce", |a| a.bounce = true),
            ("comets", |a| a.comets = true),
            ("snow", |a| a.snow = true),
            ("fireflies", |a| a.fireflies = true),
            ("cube", |a| a.cube = true),
            ("aquarium", |a| a.aquarium = true),
            ("waves", |a| a.waves = true),
            ("fireworks", |a| a.fireworks = true),
            ("life", |a| a.life = true),
            ("pipes", |a| a.pipes = true),
            ("plasma", |a| a.plasma = true),
        ];
        let zone = Rect::new(5, 3, 40, 14);
        for (name, set) in flags {
            let mut app = app_with(set);
            let mut painted = false;
            for _ in 0..400 {
                app.update(Msg::AnimTick);
                let buf = render_zone(&app, zone);
                for y in 0..buf.area.height {
                    for x in 0..buf.area.width {
                        let inside = x >= zone.left()
                            && x < zone.right()
                            && y >= zone.top()
                            && y < zone.bottom();
                        if inside {
                            painted |= buf[(x, y)].symbol() != " ";
                        } else {
                            assert_eq!(
                                buf[(x, y)].symbol(),
                                " ",
                                "{name} wrote outside the zone at ({x},{y})"
                            );
                        }
                    }
                }
                // No early break: containment must hold on EVERY frame — later phases
                // (a firework's burst, a comet mid-flight, pipes after a reseed) have
                // different geometry than the first painted frame.
            }
            assert!(painted, "{name} never painted inside its zone");
        }
    }

    /// Retro mode must keep every canvas glyph inside the CP437-friendly sets the effects
    /// declare (spot check: no fancy Unicode from the snow/firefly/firework families).
    #[test]
    fn retro_mode_swaps_fancy_glyphs() {
        let _guard = crate::i18n::lock_for_test();
        let zone = Rect::new(0, 0, 40, 12);
        let mut app = app_with(|a| {
            a.snow = true;
            a.fireflies = true;
            a.waves = true;
        });
        app.config.retro_mode = true;
        for _ in 0..60 {
            app.update(Msg::AnimTick);
            let buf = render_zone(&app, zone);
            for y in 0..zone.height {
                for x in 0..zone.width {
                    let s = buf[(x, y)].symbol();
                    assert!(
                        s.is_ascii() || s == "~",
                        "non-retro glyph {s:?} at ({x},{y})"
                    );
                }
            }
        }
    }
}
