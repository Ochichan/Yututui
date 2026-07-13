//! Showpiece canvas effects — the deliberately heavy end of the ambient family: fireworks,
//! Conway's Game of Life, the pipes screensaver and a plasma field.
//!
//! Same contract as every canvas effect: blank filler `zone` only, width-1 glyphs, phase from
//! [`App::anim_frame`] (`f`) + [`hash32`]. Fireworks and plasma are pure functions of `f`;
//! Life and Pipes keep evolving board state in a thread-local scratch (the same pattern as the
//! parent module's `RainScratch`), reseeding deterministically on resize or when the show goes
//! stale — pausing freezes them mid-scene and they resume cleanly.

use std::cell::RefCell;

use ratatui::Frame;
use ratatui::layout::Rect;
use ratatui::style::{Color, Style};

use super::{ease_out_cubic, hash32, lerp_color, put_char};
use crate::app::App;
use crate::theme::ThemeRole as R;

/// Player album art can split the filler canvas into an upper and lower zone. Keep one
/// simulation per full rectangle so those zones evolve independently without allowing an
/// unbounded cache if the terminal is repeatedly resized.
const STATEFUL_ZONE_CAPACITY: usize = 2;

struct ZoneSlots<T> {
    slots: Vec<(Rect, T)>,
}

impl<T> Default for ZoneSlots<T> {
    fn default() -> Self {
        Self { slots: Vec::new() }
    }
}

impl<T: Default> ZoneSlots<T> {
    fn get_mut(&mut self, zone: Rect) -> &mut T {
        if let Some(index) = self.slots.iter().position(|(key, _)| *key == zone) {
            let entry = self.slots.remove(index);
            self.slots.push(entry);
        } else {
            if self.slots.len() == STATEFUL_ZONE_CAPACITY {
                self.slots.remove(0);
            }
            self.slots.push((zone, T::default()));
        }
        &mut self.slots.last_mut().expect("zone slot was inserted").1
    }
}

// ── fireworks ───────────────────────────────────────────────────────────────

const FIREWORK_PALETTE: [R; 5] = [R::Accent, R::AccentAlt, R::Success, R::Warning, R::Error];

/// One launcher's state within its cycle, all derived from `f`: a rocket climbs from the
/// bottom, then a radial burst blooms at the apex, droops under gravity and fades out.
/// Two launchers run half a cycle out of phase so the sky is never empty for long.
pub(super) fn fireworks(frame: &mut Frame, app: &App, zone: Rect, f: u64) {
    let w = i64::from(zone.width);
    let h = i64::from(zone.height);
    if w < 16 || h < 6 {
        return;
    }
    let retro = app.retro_mode();
    let buf = frame.buffer_mut();
    let period = 130u64;
    for launcher in 0..2u64 {
        let lf = f + launcher * (period / 2); // half-cycle offset between the two
        let cycle = lf / period;
        let t = (lf % period) as i64;
        let seed = hash32(cycle.wrapping_mul(97) + launcher * 41);
        let launch_x = 2 + i64::from(seed % (zone.width.saturating_sub(4)).max(1) as u32);
        let apex_y = 1 + i64::from(hash32(u64::from(seed) + 11) % ((h / 3).max(1) as u32));
        let climb = (h - apex_y).max(1);
        let color = app
            .theme
            .color(FIREWORK_PALETTE[(seed as usize / 7) % FIREWORK_PALETTE.len()]);

        if t < climb {
            // Ascent: a bright head with a two-cell ember tail, wiggling one column.
            let y = h - 1 - t;
            let x = launch_x + ((t / 5) % 2) - ((t / 7) % 2);
            for k in 0..3i64 {
                let yy = y + k;
                if yy >= h || x < 0 || x >= w {
                    continue;
                }
                let (g, c) = match k {
                    0 => {
                        if retro {
                            ('^', Color::Rgb(255, 255, 220))
                        } else {
                            ('▲', Color::Rgb(255, 255, 220))
                        }
                    }
                    1 => ('|', color),
                    _ => ('.', app.theme.color(R::TextSubtle)),
                };
                put_char(
                    buf,
                    (i64::from(zone.left()) + x) as u16,
                    (i64::from(zone.top()) + yy) as u16,
                    g,
                    Style::default().fg(c),
                );
            }
            continue;
        }

        // Burst: particles fly out radially, decelerating, drooping, and dying young→old.
        let bt = (t - climb) as f64 / (period as f64 - climb as f64);
        let spread = ease_out_cubic(bt);
        let radius = 2.0 + spread * (w.min(h * 2) as f64 * 0.28);
        let particles = 24u64;
        for p in 0..particles {
            // Sparks burn out one by one over the back half of the bloom.
            if bt > 0.5
                && u64::from(hash32(p * 53 + u64::from(seed))) % 100 < ((bt - 0.5) * 220.0) as u64
            {
                continue;
            }
            let jitter = f64::from(hash32(p * 31 + u64::from(seed)) % 100) / 700.0;
            let ang = (p as f64 / particles as f64 + jitter) * std::f64::consts::TAU;
            let droop = spread * spread * 2.5; // gravity pulls the whole shell down
            let x = launch_x as f64 + ang.cos() * radius;
            let y = apex_y as f64 + ang.sin() * radius * 0.45 + droop;
            if x < 0.0 || x >= w as f64 || y < 0.0 || y >= h as f64 {
                continue;
            }
            let (glyph, bright) = if bt < 0.25 {
                (if retro { '*' } else { '✦' }, 1.0)
            } else if bt < 0.6 {
                ('*', 0.75)
            } else {
                ('.', 0.4)
            };
            let c = lerp_color(
                app.theme.color(R::TextSubtle),
                color,
                bright * (1.0 - bt * 0.5),
            );
            put_char(
                buf,
                (i64::from(zone.left()) + x as i64) as u16,
                (i64::from(zone.top()) + y as i64) as u16,
                glyph,
                Style::default().fg(c),
            );
        }
    }
}

// ── Game of Life ────────────────────────────────────────────────────────────

/// Cell ages (0 = dead, else generations alive, saturating). Kept flat, row-major.
#[derive(Default)]
struct LifeScratch {
    width: u16,
    height: u16,
    age: Vec<u8>,
    next: Vec<u8>,
    /// Frame the board last stepped on (steps happen every [`LIFE_STEP_FRAMES`]).
    last_step: u64,
    /// Bumped every reseed so each new soup is distinct but reproducible.
    epoch: u64,
}

thread_local! {
    static LIFE_SCRATCH: RefCell<ZoneSlots<LifeScratch>> = RefCell::new(ZoneSlots::default());
}

/// Frames between generations (~7 steps/sec at the default 30 fps — fast enough to read as
/// alive, slow enough to follow a glider).
const LIFE_STEP_FRAMES: u64 = 4;

/// Conway's Game of Life on the filler zone: a hashed soup seeds the board, generations tick
/// every few frames, and cells colour by age (newborn accent → elder ember). When the colony
/// dies down or stabilises into stasis it quietly reseeds.
pub(super) fn life(frame: &mut Frame, app: &App, zone: Rect, f: u64) {
    let w = usize::from(zone.width);
    let h = usize::from(zone.height);
    if w < 8 || h < 6 {
        return;
    }
    let young = app.theme.color(R::Accent);
    let old = app.theme.color(R::GaugeFilled);
    let glyphs: [char; 3] = if app.retro_mode() {
        ['#', 'x', '.']
    } else {
        ['▓', '▒', '░']
    };

    LIFE_SCRATCH.with(|scratch| {
        let mut slots = scratch.borrow_mut();
        let s = slots.get_mut(zone);
        let cells = w * h;
        if s.width != zone.width || s.height != zone.height || f < s.last_step {
            // Resize — or a frame-counter context change (f went backwards) — restarts the show.
            s.width = zone.width;
            s.height = zone.height;
            s.age = vec![0; cells];
            s.next = vec![0; cells];
            s.epoch = s.epoch.wrapping_add(1);
            s.last_step = f;
            let epoch = s.epoch;
            seed_life(&mut s.age, w, h, epoch);
        }

        // Advance at most a few generations per render even after a long park, so a resume
        // doesn't burn CPU fast-forwarding thousands of steps nobody saw.
        let mut steps = ((f - s.last_step) / LIFE_STEP_FRAMES).min(3);
        if steps > 0 {
            s.last_step = f;
        }
        while steps > 0 {
            steps -= 1;
            let LifeScratch { age, next, .. } = &mut *s;
            let mut alive = 0usize;
            for y in 0..h {
                for x in 0..w {
                    let mut n = 0u8;
                    for dy in [h - 1, 0, 1] {
                        for dx in [w - 1, 0, 1] {
                            if dx == 0 && dy == 0 {
                                continue;
                            }
                            // Toroidal wrap keeps gliders flying forever.
                            let nx = (x + dx) % w;
                            let ny = (y + dy) % h;
                            n += u8::from(age[ny * w + nx] > 0);
                        }
                    }
                    let idx = y * w + x;
                    let was = age[idx] > 0;
                    let lives = matches!((was, n), (true, 2) | (true, 3) | (false, 3));
                    next[idx] = if lives {
                        alive += 1;
                        age[idx].saturating_add(1).max(1)
                    } else {
                        0
                    };
                }
            }
            std::mem::swap(age, next);
            // A dead or near-dead board reseeds (fresh epoch → a different soup).
            if alive * 50 < cells {
                s.epoch = s.epoch.wrapping_add(1);
                let epoch = s.epoch;
                s.age.fill(0);
                seed_life(&mut s.age, w, h, epoch);
            }
        }

        let buf = frame.buffer_mut();
        for y in 0..h {
            for x in 0..w {
                let a = s.age[y * w + x];
                if a == 0 {
                    continue;
                }
                // Age buckets: newborn / settled / ancient.
                let (g, t) = match a {
                    1..=2 => (glyphs[0], 0.0),
                    3..=8 => (glyphs[1], 0.5),
                    _ => (glyphs[2], 1.0),
                };
                put_char(
                    buf,
                    zone.left() + x as u16,
                    zone.top() + y as u16,
                    g,
                    Style::default().fg(lerp_color(young, old, t)),
                );
            }
        }
    });
}

/// Hash-seed a fresh soup: ~28% fill plus a couple of gliders so there is always traffic.
fn seed_life(age: &mut [u8], w: usize, h: usize, epoch: u64) {
    for y in 0..h {
        for x in 0..w {
            if hash32(((y * w + x) as u64) ^ epoch.wrapping_mul(0x9E37)) % 100 < 28 {
                age[y * w + x] = 1;
            }
        }
    }
    // Glider stamp (one per ~40 columns) pointed down-right.
    const GLIDER: [(usize, usize); 5] = [(1, 0), (2, 1), (0, 2), (1, 2), (2, 2)];
    for g in 0..(w / 40 + 1) {
        let ox = (hash32(epoch + g as u64 * 7) as usize) % w.saturating_sub(4).max(1);
        let oy = (hash32(epoch + g as u64 * 13 + 3) as usize) % h.saturating_sub(4).max(1);
        for (dx, dy) in GLIDER {
            age[(oy + dy) * w + (ox + dx)] = 1;
        }
    }
}

// ── pipes ───────────────────────────────────────────────────────────────────

/// Box-drawing glyph per cell, packed as `(palette_index << 4) | glyph_index`; 0 = empty.
#[derive(Default)]
struct PipesScratch {
    width: u16,
    height: u16,
    cells: Vec<u8>,
    /// Live drawing heads: `(x, y, dir)` with dir 0=up 1=right 2=down 3=left.
    heads: Vec<(u16, u16, u8)>,
    filled: usize,
    last_f: u64,
    epoch: u64,
}

thread_local! {
    static PIPES_SCRATCH: RefCell<ZoneSlots<PipesScratch>> = RefCell::new(ZoneSlots::default());
}

/// glyph_index → glyph, [straight-vertical, straight-horizontal, and the four elbows].
const PIPE_GLYPHS: [char; 6] = ['│', '─', '┌', '┐', '└', '┘'];
const PIPE_GLYPHS_RETRO: [char; 6] = ['|', '-', '+', '+', '+', '+'];
const PIPE_PALETTE: [R; 5] = [R::Accent, R::AccentAlt, R::Success, R::Warning, R::Error];

/// The classic pipes screensaver: a few coloured pipes snake across the zone one cell per
/// frame, elbowing at hashed corners; when the board is ~clogged everything clears and a new
/// epoch starts. The laid pipe is repainted from scratch each frame (the back buffer holds
/// nothing between frames), which is what earns pipes its "showpiece" price tag.
pub(super) fn pipes(frame: &mut Frame, app: &App, zone: Rect, f: u64) {
    let w = usize::from(zone.width);
    let h = usize::from(zone.height);
    if w < 10 || h < 5 {
        return;
    }
    let retro = app.retro_mode();
    let glyphs = if retro {
        &PIPE_GLYPHS_RETRO
    } else {
        &PIPE_GLYPHS
    };

    PIPES_SCRATCH.with(|scratch| {
        let mut slots = scratch.borrow_mut();
        let s = slots.get_mut(zone);
        let cells = w * h;
        let clogged = s.filled * 10 >= cells * 6;
        if s.width != zone.width || s.height != zone.height || f < s.last_f || clogged {
            s.width = zone.width;
            s.height = zone.height;
            s.cells = vec![0; cells];
            s.filled = 0;
            s.epoch = s.epoch.wrapping_add(1);
            let epoch = s.epoch;
            let pipe_count = (w / 24 + 2).min(5);
            s.heads = (0..pipe_count)
                .map(|i| {
                    let seed = hash32(epoch.wrapping_mul(31) + i as u64);
                    (
                        (seed as usize % w) as u16,
                        (hash32(u64::from(seed) + 7) as usize % h) as u16,
                        (seed % 4) as u8,
                    )
                })
                .collect();
        }

        // One cell of progress per pipe per frame (bounded even after a long park).
        let advances = (f.saturating_sub(s.last_f)).min(2);
        s.last_f = f;
        for advance in 0..advances {
            let PipesScratch {
                cells: board,
                heads,
                filled,
                epoch,
                ..
            } = &mut *s;
            for (pi, head) in heads.iter_mut().enumerate() {
                let (x, y, dir) = *head;
                // Turn at hashed corners (~1 in 5 cells), never a U-turn.
                // `advance` is mixed in so a two-step catch-up after a park doesn't hand
                // both steps the identical turn decision.
                let r = hash32(f.wrapping_mul(7) ^ advance ^ (*epoch << 8) ^ (pi as u64 * 131));
                let new_dir = if r.is_multiple_of(5) {
                    if r.is_multiple_of(2) {
                        (dir + 1) % 4
                    } else {
                        (dir + 3) % 4
                    }
                } else {
                    dir
                };
                // Elbow glyph for a turn, straight glyph otherwise (indexes into PIPE_GLYPHS).
                let glyph_idx = match (dir, new_dir) {
                    (d, n) if d == n => {
                        if d % 2 == 0 {
                            0
                        } else {
                            1
                        }
                    }
                    (0, 1) | (3, 2) => 2, // ┌
                    (0, 3) | (1, 2) => 3, // ┐
                    (2, 1) | (3, 0) => 4, // └
                    _ => 5,               // ┘
                };
                let idx = usize::from(y) * w + usize::from(x);
                if board[idx] == 0 {
                    *filled += 1;
                }
                board[idx] = ((pi as u8 % PIPE_PALETTE.len() as u8) << 4) | (glyph_idx as u8 + 1);
                // Step (with toroidal wrap so pipes re-enter instead of piling on walls).
                let (nx, ny) = match new_dir {
                    0 => (x, y.checked_sub(1).unwrap_or(zone.height - 1)),
                    1 => (if x + 1 >= zone.width { 0 } else { x + 1 }, y),
                    2 => (x, if y + 1 >= zone.height { 0 } else { y + 1 }),
                    _ => (x.checked_sub(1).unwrap_or(zone.width - 1), y),
                };
                *head = (nx, ny, new_dir);
            }
        }

        let buf = frame.buffer_mut();
        for y in 0..h {
            for x in 0..w {
                let v = s.cells[y * w + x];
                if v == 0 {
                    continue;
                }
                let color = app
                    .theme
                    .color(PIPE_PALETTE[usize::from(v >> 4) % PIPE_PALETTE.len()]);
                let g = glyphs[usize::from((v & 0x0F) - 1).min(glyphs.len() - 1)];
                put_char(
                    buf,
                    zone.left() + x as u16,
                    zone.top() + y as u16,
                    g,
                    Style::default().fg(color),
                );
            }
        }
    });
}

// ── plasma ──────────────────────────────────────────────────────────────────

const PLASMA_GLYPHS: [char; 4] = [' ', '░', '▒', '▓'];

/// A demoscene plasma field washing over the whole zone: three phase-shifted sine bands
/// (one per axis plus a diagonal) sum into a scalar that picks both a density glyph and a
/// colour along a Background→Accent→AccentAlt ramp. Every cell is touched every frame —
/// deliberately the most expensive effect in the app, and last in the resource ordering.
/// The three bands are 1-D, so each frame precomputes `w + h + (w+h)` sines instead of
/// `3·w·h` (the per-cell work is two adds and two table lookups).
pub(super) fn plasma(frame: &mut Frame, app: &App, zone: Rect, f: u64) {
    let w = usize::from(zone.width);
    let h = usize::from(zone.height);
    if w < 4 || h < 2 {
        return;
    }
    let t = f as f64;
    let col_wave: Vec<f64> = (0..w)
        .map(|x| (x as f64 * 0.30 + t * 0.055).sin())
        .collect();
    let row_wave: Vec<f64> = (0..h)
        .map(|y| (y as f64 * 0.55 - t * 0.038).sin())
        .collect();
    let diag_wave: Vec<f64> = (0..w + h)
        .map(|d| (d as f64 * 0.21 + t * 0.024).sin())
        .collect();

    let bg = app.theme.color(R::Background);
    let mid = app.theme.color(R::Accent);
    let hot = app.theme.color(R::AccentAlt);
    // Quantized style ramp (16 steps) so cells share Style values instead of allocating a
    // fresh blend per cell.
    let ramp: [Style; 16] = std::array::from_fn(|i| {
        let v = i as f64 / 15.0;
        let c = if v < 0.5 {
            lerp_color(bg, mid, v * 2.0)
        } else {
            lerp_color(mid, hot, (v - 0.5) * 2.0)
        };
        Style::default().fg(c)
    });

    let buf = frame.buffer_mut();
    for y in 0..h {
        for x in 0..w {
            // v ∈ [-3, 3] → normalized [0, 1].
            let v = (col_wave[x] + row_wave[y] + diag_wave[x + y] + 3.0) / 6.0;
            let bucket = (v * 3.999) as usize; // 0..=3 density glyph
            if bucket == 0 {
                continue; // the trough stays blank — cheaper, and lets the theme breathe
            }
            let ci = (v * 15.999) as usize;
            put_char(
                buf,
                zone.left() + x as u16,
                zone.top() + y as u16,
                PLASMA_GLYPHS[bucket],
                ramp[ci.min(15)],
            );
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ratatui::Terminal;
    use ratatui::backend::TestBackend;
    use ratatui::buffer::Buffer;

    fn dual_zones() -> [Rect; 2] {
        [Rect::new(4, 1, 40, 6), Rect::new(4, 10, 40, 7)]
    }

    fn render_dual(
        app: &App,
        zones: [Rect; 2],
        f: u64,
        effect: fn(&mut Frame, &App, Rect, u64),
    ) -> Buffer {
        let backend = TestBackend::new(50, 20);
        let mut terminal = Terminal::new(backend).unwrap();
        terminal
            .draw(|frame| {
                for zone in zones {
                    effect(frame, app, zone, f);
                }
            })
            .unwrap();
        terminal.backend().buffer().clone()
    }

    fn painted_cells(buffer: &Buffer, zone: Rect) -> usize {
        let mut count = 0;
        for y in zone.top()..zone.bottom() {
            for x in zone.left()..zone.right() {
                count += usize::from(buffer[(x, y)].symbol() != " ");
            }
        }
        count
    }

    #[test]
    fn zone_slots_use_full_rect_keys_and_evict_lru_at_capacity() {
        let first = Rect::new(0, 0, 20, 6);
        let second = Rect::new(0, 8, 20, 6);
        let third = Rect::new(22, 0, 20, 6);
        let mut slots = ZoneSlots::<u8>::default();

        *slots.get_mut(first) = 1;
        *slots.get_mut(second) = 2;
        assert_eq!(slots.slots.len(), STATEFUL_ZONE_CAPACITY);
        assert_eq!(*slots.get_mut(first), 1);

        *slots.get_mut(third) = 3;
        assert_eq!(slots.slots.len(), STATEFUL_ZONE_CAPACITY);
        assert!(slots.slots.iter().all(|(zone, _)| *zone != second));
        assert_eq!(slots.slots.iter().map(|(_, value)| *value).sum::<u8>(), 4);
    }

    #[test]
    fn life_keeps_both_player_zones_evolving_independently() {
        LIFE_SCRATCH.with(|scratch| scratch.borrow_mut().slots.clear());
        let app = App::new(100);
        let zones = dual_zones();

        render_dual(&app, zones, 0, life);
        let before = LIFE_SCRATCH.with(|scratch| {
            let scratch = scratch.borrow();
            zones.map(|zone| {
                let state = &scratch
                    .slots
                    .iter()
                    .find(|(key, _)| *key == zone)
                    .expect("life state for both zones")
                    .1;
                (state.epoch, state.age.clone())
            })
        });

        let buffer = render_dual(&app, zones, LIFE_STEP_FRAMES, life);
        LIFE_SCRATCH.with(|scratch| {
            let scratch = scratch.borrow();
            assert_eq!(scratch.slots.len(), STATEFUL_ZONE_CAPACITY);
            for (index, zone) in zones.into_iter().enumerate() {
                let state = &scratch
                    .slots
                    .iter()
                    .find(|(key, _)| *key == zone)
                    .expect("life state for both zones")
                    .1;
                assert_eq!(state.epoch, before[index].0, "zone {index} was reseeded");
                assert_eq!(state.last_step, LIFE_STEP_FRAMES);
                assert_ne!(state.age, before[index].1, "zone {index} did not evolve");
                assert!(painted_cells(&buffer, zone) > 0);
            }
        });
    }

    #[test]
    fn pipes_paint_and_advance_in_both_player_zones() {
        PIPES_SCRATCH.with(|scratch| scratch.borrow_mut().slots.clear());
        let app = App::new(100);
        let zones = dual_zones();

        let first = render_dual(&app, zones, 1, pipes);
        for zone in zones {
            assert!(painted_cells(&first, zone) > 0);
        }
        let before = PIPES_SCRATCH.with(|scratch| {
            let scratch = scratch.borrow();
            assert_eq!(scratch.slots.len(), STATEFUL_ZONE_CAPACITY);
            zones.map(|zone| {
                let state = &scratch
                    .slots
                    .iter()
                    .find(|(key, _)| *key == zone)
                    .expect("pipes state for both zones")
                    .1;
                (state.epoch, state.cells.clone())
            })
        });

        let second = render_dual(&app, zones, 2, pipes);
        PIPES_SCRATCH.with(|scratch| {
            let scratch = scratch.borrow();
            assert_eq!(scratch.slots.len(), STATEFUL_ZONE_CAPACITY);
            for (index, zone) in zones.into_iter().enumerate() {
                let state = &scratch
                    .slots
                    .iter()
                    .find(|(key, _)| *key == zone)
                    .expect("pipes state for both zones")
                    .1;
                assert_eq!(state.epoch, before[index].0, "zone {index} was reset");
                assert_eq!(state.last_f, 2);
                assert_ne!(state.cells, before[index].1, "zone {index} did not advance");
                assert!(painted_cells(&second, zone) > 0);
            }
        });
    }
}
