//! Showpiece canvas effects — the deliberately heavy end of the ambient family: fireworks,
//! Conway's Game of Life, the pipes screensaver and a plasma field.
//!
//! Same contract as every canvas effect: blank filler `zone` only, width-1 glyphs, phase from
//! [`App::anim_frame`] (`f`) + [`hash32`]. Fireworks and plasma are pure functions of `f`;
//! Life and Pipes keep evolving board state in a thread-local scratch (the same pattern as the
//! parent module's `RainScratch`), reseeding deterministically on resize or when the show goes
//! stale — pausing freezes them mid-scene and they resume cleanly.

use std::cell::RefCell;

use ratatui::layout::Rect;
use ratatui::style::Color;

use super::canvas::{CanvasWriter, reset_scratch_vec};
use super::{ease_out_cubic, hash32, lerp_color};
use crate::app::App;
use crate::theme::ThemeRole as R;

// ── fireworks ───────────────────────────────────────────────────────────────

const FIREWORK_PALETTE: [R; 5] = [R::Accent, R::AccentAlt, R::Success, R::Warning, R::Error];
const FIREWORK_PARTICLE_COUNT: usize = 24;

#[derive(Clone, Copy, Debug, Default)]
struct FireworkParticle {
    burn_roll: u64,
    angle_cos: f64,
    angle_sin: f64,
}

impl FireworkParticle {
    fn new(seed: u32, particle: u64) -> Self {
        let burn_roll = u64::from(hash32(particle * 53 + u64::from(seed))) % 100;
        let jitter = f64::from(hash32(particle * 31 + u64::from(seed)) % 100) / 700.0;
        let angle =
            (particle as f64 / FIREWORK_PARTICLE_COUNT as f64 + jitter) * std::f64::consts::TAU;
        // Keep the original evaluation order instead of using `sin_cos`, whose last bits may
        // differ on some targets.
        let angle_cos = angle.cos();
        let angle_sin = angle.sin();
        Self {
            burn_roll,
            angle_cos,
            angle_sin,
        }
    }
}

#[derive(Default)]
struct FireworkLauncherCache {
    valid: bool,
    seed: u32,
    particles: [FireworkParticle; FIREWORK_PARTICLE_COUNT],
}

#[derive(Default)]
struct FireworkScratch {
    launchers: [FireworkLauncherCache; 2],
}

thread_local! {
    // Keep both particle descriptor arrays off every native thread's static TLS block. The box is
    // created only on the render thread, and only after an eligible fireworks surface is active.
    static FIREWORK_SCRATCH: RefCell<Option<Box<FireworkScratch>>> = const { RefCell::new(None) };
}

/// One launcher's state within its cycle, all derived from `f`: a rocket climbs from the
/// bottom, then a radial burst blooms at the apex, droops under gravity and fades out.
/// Two launchers run half a cycle out of phase so the sky is never empty for long.
pub(super) fn fireworks(canvas: &mut CanvasWriter<'_>, app: &App, zone: Rect, f: u64) {
    let w = i64::from(zone.width);
    let h = i64::from(zone.height);
    if w < 16 || h < 6 {
        return;
    }
    let retro = app.retro_mode();
    let period = 130u64;
    FIREWORK_SCRATCH.with(|slot| {
        let mut slot = slot.borrow_mut();
        let scratch = slot.get_or_insert_with(|| Box::new(FireworkScratch::default()));
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
                    canvas.put_fg(
                        (i64::from(zone.left()) + x) as u16,
                        (i64::from(zone.top()) + yy) as u16,
                        g,
                        c,
                    );
                }
                continue;
            }

            // Burst: particles fly out radially, decelerating, drooping, and dying young→old.
            let bt = (t - climb) as f64 / (period as f64 - climb as f64);
            let spread = ease_out_cubic(bt);
            let radius = 2.0 + spread * (w.min(h * 2) as f64 * 0.28);
            let cache = &mut scratch.launchers[launcher as usize];
            if !cache.valid || cache.seed != seed {
                cache.valid = true;
                cache.seed = seed;
                for (particle, descriptor) in cache.particles.iter_mut().enumerate() {
                    *descriptor = FireworkParticle::new(seed, particle as u64);
                }
            }
            for descriptor in &cache.particles {
                // Sparks burn out one by one over the back half of the bloom.
                if bt > 0.5 && descriptor.burn_roll < ((bt - 0.5) * 220.0) as u64 {
                    continue;
                }
                let droop = spread * spread * 2.5; // gravity pulls the whole shell down
                let x = launch_x as f64 + descriptor.angle_cos * radius;
                let y = apex_y as f64 + descriptor.angle_sin * radius * 0.45 + droop;
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
                canvas.put_fg(
                    (i64::from(zone.left()) + x as i64) as u16,
                    (i64::from(zone.top()) + y as i64) as u16,
                    glyph,
                    c,
                );
            }
        }
    });
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
    static LIFE_SCRATCH: RefCell<LifeScratch> = RefCell::new(LifeScratch::default());
}

/// Frames between generations (~7 steps/sec at the default 30 fps — fast enough to read as
/// alive, slow enough to follow a glider).
const LIFE_STEP_FRAMES: u64 = 4;

/// Conway's Game of Life on the filler zone: a hashed soup seeds the board, generations tick
/// every few frames, and cells colour by age (newborn accent → elder ember). When the colony
/// dies down or stabilises into stasis it quietly reseeds.
pub(super) fn life(canvas: &mut CanvasWriter<'_>, app: &App, zone: Rect, f: u64) {
    let w = usize::from(zone.width);
    let h = usize::from(zone.height);
    if w < 8 || h < 6 {
        return;
    }
    let young = app.theme.color(R::Accent);
    let old = app.theme.color(R::GaugeFilled);
    let age_colors = [young, lerp_color(young, old, 0.5), old];
    let glyphs: [char; 3] = if app.retro_mode() {
        ['#', 'x', '.']
    } else {
        ['▓', '▒', '░']
    };

    LIFE_SCRATCH.with(|scratch| {
        let mut scratch = scratch.borrow_mut();
        let s = &mut *scratch;
        let cells = w * h;
        let geometry_changed = s.width != zone.width || s.height != zone.height;
        if geometry_changed || f < s.last_step {
            // Resize — or a frame-counter context change (f went backwards) — restarts the show.
            s.width = zone.width;
            s.height = zone.height;
            if geometry_changed {
                reset_scratch_vec(&mut s.age, cells);
                reset_scratch_vec(&mut s.next, cells);
            }
            s.age.resize(cells, 0);
            s.age.fill(0);
            s.next.resize(cells, 0);
            s.next.fill(0);
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
                let prev_y = if y == 0 { h - 1 } else { y - 1 } * w;
                let row_y = y * w;
                let next_y = if y + 1 == h { 0 } else { y + 1 } * w;
                for x in 0..w {
                    // Toroidal wrap keeps gliders flying forever. Resolve the four edge cases
                    // once instead of executing eight modulo operations for every cell.
                    let prev_x = if x == 0 { w - 1 } else { x - 1 };
                    let next_x = if x + 1 == w { 0 } else { x + 1 };
                    let n = u8::from(age[prev_y + prev_x] > 0)
                        + u8::from(age[prev_y + x] > 0)
                        + u8::from(age[prev_y + next_x] > 0)
                        + u8::from(age[row_y + prev_x] > 0)
                        + u8::from(age[row_y + next_x] > 0)
                        + u8::from(age[next_y + prev_x] > 0)
                        + u8::from(age[next_y + x] > 0)
                        + u8::from(age[next_y + next_x] > 0);
                    let idx = row_y + x;
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

        for y in 0..h {
            let abs_y = zone.top() + y as u16;
            let (spans, span_count) = canvas.visible_spans(zone, abs_y);
            for span in &spans[..span_count] {
                for abs_x in span.start..span.end {
                    let x = usize::from(abs_x - zone.left());
                    let a = s.age[y * w + x];
                    if a == 0 {
                        continue;
                    }
                    // Age buckets: newborn / settled / ancient.
                    let (g, color) = match a {
                        1..=2 => (glyphs[0], age_colors[0]),
                        3..=8 => (glyphs[1], age_colors[1]),
                        _ => (glyphs[2], age_colors[2]),
                    };
                    canvas.put_visible_fg(abs_x, abs_y, g, color);
                }
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
    static PIPES_SCRATCH: RefCell<PipesScratch> = RefCell::new(PipesScratch::default());
}

/// glyph_index → glyph, [straight-vertical, straight-horizontal, and the four elbows].
const PIPE_GLYPHS: [char; 6] = ['│', '─', '┌', '┐', '└', '┘'];
const PIPE_GLYPHS_RETRO: [char; 6] = ['|', '-', '+', '+', '+', '+'];
const PIPE_PALETTE: [R; 5] = [R::Accent, R::AccentAlt, R::Success, R::Warning, R::Error];

/// The classic pipes screensaver: a few coloured pipes snake across the zone one cell per
/// frame, elbowing at hashed corners; when the board is ~clogged everything clears and a new
/// epoch starts. The laid pipe is repainted from scratch each frame (the back buffer holds
/// nothing between frames), which is what earns pipes its "showpiece" price tag.
pub(super) fn pipes(canvas: &mut CanvasWriter<'_>, app: &App, zone: Rect, f: u64) {
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
    let colors = PIPE_PALETTE.map(|role| app.theme.color(role));

    PIPES_SCRATCH.with(|scratch| {
        let mut scratch = scratch.borrow_mut();
        let s = &mut *scratch;
        let cells = w * h;
        let clogged = s.filled * 10 >= cells * 6;
        let geometry_changed = s.width != zone.width || s.height != zone.height;
        if geometry_changed || f < s.last_f || clogged {
            s.width = zone.width;
            s.height = zone.height;
            if geometry_changed {
                reset_scratch_vec(&mut s.cells, cells);
            }
            s.cells.resize(cells, 0);
            s.cells.fill(0);
            s.filled = 0;
            s.epoch = s.epoch.wrapping_add(1);
            let epoch = s.epoch;
            let pipe_count = (w / 24 + 2).min(5);
            if geometry_changed {
                reset_scratch_vec(&mut s.heads, pipe_count);
            } else {
                s.heads.clear();
                s.heads.reserve(pipe_count);
            }
            for i in 0..pipe_count {
                let seed = hash32(epoch.wrapping_mul(31) + i as u64);
                s.heads.push((
                    (seed as usize % w) as u16,
                    (hash32(u64::from(seed) + 7) as usize % h) as u16,
                    (seed % 4) as u8,
                ));
            }
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

        for y in 0..h {
            let abs_y = zone.top() + y as u16;
            let (spans, span_count) = canvas.visible_spans(zone, abs_y);
            for span in &spans[..span_count] {
                for abs_x in span.start..span.end {
                    let x = usize::from(abs_x - zone.left());
                    let v = s.cells[y * w + x];
                    if v == 0 {
                        continue;
                    }
                    let color = colors[usize::from(v >> 4) % colors.len()];
                    let g = glyphs[usize::from((v & 0x0F) - 1).min(glyphs.len() - 1)];
                    canvas.put_visible_fg(abs_x, abs_y, g, color);
                }
            }
        }
    });
}

// ── plasma ──────────────────────────────────────────────────────────────────

const PLASMA_GLYPHS: [char; 4] = [' ', '░', '▒', '▓'];

#[derive(Default)]
struct PlasmaScratch {
    col_wave: Vec<f64>,
    row_wave: Vec<f64>,
    diag_wave: Vec<f64>,
}

thread_local! {
    static PLASMA_SCRATCH: RefCell<PlasmaScratch> = RefCell::new(PlasmaScratch::default());
}

/// A demoscene plasma field washing over the whole zone: three phase-shifted sine bands
/// (one per axis plus a diagonal) sum into a scalar that picks both a density glyph and a
/// colour along a Background→Accent→AccentAlt ramp. Every cell is touched every frame —
/// deliberately the most expensive effect in the app, and last in the resource ordering.
/// The three bands are 1-D, so each frame precomputes `w + h + (w+h)` sines instead of
/// `3·w·h` (the per-cell work is two adds and two table lookups).
pub(super) fn plasma(canvas: &mut CanvasWriter<'_>, app: &App, zone: Rect, f: u64) {
    let w = usize::from(zone.width);
    let h = usize::from(zone.height);
    if w < 4 || h < 2 {
        return;
    }
    let bg = app.theme.color(R::Background);
    let mid = app.theme.color(R::Accent);
    let hot = app.theme.color(R::AccentAlt);
    // Quantized style ramp (16 steps) so cells share Style values instead of allocating a
    // fresh blend per cell.
    let ramp: [Color; 16] = std::array::from_fn(|i| {
        let v = i as f64 / 15.0;
        if v < 0.5 {
            lerp_color(bg, mid, v * 2.0)
        } else {
            lerp_color(mid, hot, (v - 0.5) * 2.0)
        }
    });

    PLASMA_SCRATCH.with(|scratch| {
        let mut scratch = scratch.borrow_mut();
        if scratch.col_wave.len() != w {
            reset_scratch_vec(&mut scratch.col_wave, w);
        }
        if scratch.row_wave.len() != h {
            reset_scratch_vec(&mut scratch.row_wave, h);
        }
        if scratch.diag_wave.len() != w + h {
            reset_scratch_vec(&mut scratch.diag_wave, w + h);
        }
        scratch.col_wave.resize(w, 0.0);
        scratch.row_wave.resize(h, 0.0);
        scratch.diag_wave.resize(w + h, 0.0);
        let t = f as f64;
        for (x, value) in scratch.col_wave.iter_mut().enumerate() {
            *value = (x as f64 * 0.30 + t * 0.055).sin();
        }
        for (y, value) in scratch.row_wave.iter_mut().enumerate() {
            *value = (y as f64 * 0.55 - t * 0.038).sin();
        }
        for (d, value) in scratch.diag_wave.iter_mut().enumerate() {
            *value = (d as f64 * 0.21 + t * 0.024).sin();
        }

        for y in 0..h {
            let abs_y = zone.top() + y as u16;
            let (spans, span_count) = canvas.visible_spans(zone, abs_y);
            for span in &spans[..span_count] {
                for abs_x in span.start..span.end {
                    let x = usize::from(abs_x - zone.left());
                    // v ∈ [-3, 3] → normalized [0, 1].
                    let v = (scratch.col_wave[x]
                        + scratch.row_wave[y]
                        + scratch.diag_wave[x + y]
                        + 3.0)
                        / 6.0;
                    let bucket = (v * 3.999) as usize; // 0..=3 density glyph
                    if bucket == 0 {
                        continue; // the trough stays blank — cheaper, and lets the theme breathe
                    }
                    let ci = (v * 15.999) as usize;
                    canvas.put_visible_fg(abs_x, abs_y, PLASMA_GLYPHS[bucket], ramp[ci.min(15)]);
                }
            }
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;
    use ratatui::Terminal;
    use ratatui::backend::TestBackend;
    use ratatui::buffer::Buffer;

    fn render_effect(
        app: &App,
        zone: Rect,
        art_mask: Option<Rect>,
        f: u64,
        effect: impl Fn(&mut CanvasWriter<'_>, &App, Rect, u64),
    ) -> Buffer {
        let backend = TestBackend::new(50, 20);
        let mut terminal = Terminal::new(backend).unwrap();
        terminal
            .draw(|frame| {
                let mut canvas = CanvasWriter::new(frame.buffer_mut(), art_mask);
                effect(&mut canvas, app, zone, f);
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
    fn cached_firework_particles_match_the_original_formulas() {
        for seed in [0, 1, 0xDEAD_BEEF] {
            for particle in 0..FIREWORK_PARTICLE_COUNT as u64 {
                let descriptor = FireworkParticle::new(seed, particle);
                assert_eq!(
                    descriptor.burn_roll,
                    u64::from(hash32(particle * 53 + u64::from(seed))) % 100
                );
                let jitter = f64::from(hash32(particle * 31 + u64::from(seed)) % 100) / 700.0;
                let angle = (particle as f64 / FIREWORK_PARTICLE_COUNT as f64 + jitter)
                    * std::f64::consts::TAU;
                assert_eq!(descriptor.angle_cos.to_bits(), angle.cos().to_bits());
                assert_eq!(descriptor.angle_sin.to_bits(), angle.sin().to_bits());
            }
        }
    }

    #[test]
    fn firework_cache_is_lazy_until_the_first_eligible_surface() {
        FIREWORK_SCRATCH.with(|slot| *slot.borrow_mut() = None);
        let app = App::new(100);

        render_effect(&app, Rect::new(4, 2, 15, 6), None, 35, fireworks);
        FIREWORK_SCRATCH.with(|slot| assert!(slot.borrow().is_none()));

        render_effect(&app, Rect::new(4, 2, 16, 6), None, 35, fireworks);
        FIREWORK_SCRATCH.with(|slot| assert!(slot.borrow().is_some()));
    }

    #[test]
    fn firework_cache_reuses_identical_output_and_rekeys_by_cycle() {
        FIREWORK_SCRATCH.with(|slot| *slot.borrow_mut() = None);
        let app = App::new(100);
        let zone = Rect::new(4, 2, 40, 14);
        let first = render_effect(&app, zone, None, 35, fireworks);
        let warm = render_effect(&app, zone, None, 35, fireworks);
        assert_eq!(warm, first);
        let old_seeds = FIREWORK_SCRATCH.with(|slot| {
            let slot = slot.borrow();
            let scratch = slot.as_deref().expect("eligible surface creates cache");
            assert!(scratch.launchers.iter().all(|cache| cache.valid));
            [scratch.launchers[0].seed, scratch.launchers[1].seed]
        });

        render_effect(&app, zone, None, 165, fireworks);
        FIREWORK_SCRATCH.with(|slot| {
            let slot = slot.borrow();
            let scratch = slot.as_deref().expect("cache remains available");
            assert_ne!(scratch.launchers[0].seed, old_seeds[0]);
            assert_ne!(scratch.launchers[1].seed, old_seeds[1]);
        });
    }

    #[test]
    fn geometry_resets_downsize_large_state_without_changing_life_or_pipes_epochs() {
        const LARGE_CELLS: usize = 80 * 1024;
        LIFE_SCRATCH.with(|scratch| {
            let mut age = Vec::with_capacity(LARGE_CELLS);
            age.resize(LARGE_CELLS, 1);
            let next = vec![0; LARGE_CELLS];
            *scratch.borrow_mut() = LifeScratch {
                width: 400,
                height: 200,
                age,
                next,
                last_step: 0,
                epoch: 7,
            };
        });
        let app = App::new(100);
        let zone = Rect::new(4, 2, 40, 14);
        render_effect(&app, zone, None, 0, life);
        LIFE_SCRATCH.with(|scratch| {
            let scratch = scratch.borrow();
            assert_eq!(scratch.epoch, 8);
            assert_eq!(scratch.age.len(), usize::from(zone.width * zone.height));
            assert_eq!(scratch.next.len(), usize::from(zone.width * zone.height));
            assert!(scratch.age.capacity() < LARGE_CELLS);
            assert!(scratch.next.capacity() < LARGE_CELLS);
        });

        PIPES_SCRATCH.with(|scratch| {
            let mut cells = Vec::with_capacity(LARGE_CELLS);
            cells.resize(LARGE_CELLS, 1);
            let mut heads = Vec::with_capacity(16 * 1024);
            heads.resize(16 * 1024, (0, 0, 0));
            *scratch.borrow_mut() = PipesScratch {
                width: 400,
                height: 200,
                cells,
                heads,
                filled: 1,
                last_f: 0,
                epoch: 11,
            };
        });
        render_effect(&app, zone, None, 1, pipes);
        PIPES_SCRATCH.with(|scratch| {
            let scratch = scratch.borrow();
            assert_eq!(scratch.epoch, 12);
            assert_eq!(scratch.cells.len(), usize::from(zone.width * zone.height));
            assert!(scratch.cells.capacity() < LARGE_CELLS);
            assert!(scratch.heads.capacity() < 16 * 1024);
        });
    }

    #[test]
    fn plasma_downsizes_large_vectors_only_when_geometry_changes() {
        const LARGE_LEN: usize = 10 * 1024;
        PLASMA_SCRATCH.with(|scratch| {
            *scratch.borrow_mut() = PlasmaScratch {
                col_wave: vec![0.0; LARGE_LEN],
                row_wave: vec![0.0; LARGE_LEN],
                diag_wave: vec![0.0; LARGE_LEN],
            };
        });
        let app = App::new(100);
        let zone = Rect::new(4, 2, 40, 14);
        render_effect(&app, zone, None, 1, plasma);
        let capacities = PLASMA_SCRATCH.with(|scratch| {
            let scratch = scratch.borrow();
            assert!(scratch.col_wave.capacity() < LARGE_LEN);
            assert!(scratch.row_wave.capacity() < LARGE_LEN);
            assert!(scratch.diag_wave.capacity() < LARGE_LEN);
            (
                scratch.col_wave.capacity(),
                scratch.row_wave.capacity(),
                scratch.diag_wave.capacity(),
            )
        });
        render_effect(&app, zone, None, 2, plasma);
        PLASMA_SCRATCH.with(|scratch| {
            let scratch = scratch.borrow();
            assert_eq!(
                (
                    scratch.col_wave.capacity(),
                    scratch.row_wave.capacity(),
                    scratch.diag_wave.capacity(),
                ),
                capacities
            );
        });
    }

    #[test]
    fn life_mask_changes_do_not_reset_the_scene() {
        LIFE_SCRATCH.with(|scratch| *scratch.borrow_mut() = LifeScratch::default());
        let app = App::new(100);
        let zone = Rect::new(4, 2, 40, 14);
        let first_mask = Rect::new(10, 5, 10, 5);
        let second_mask = Rect::new(24, 7, 8, 4);

        render_effect(&app, zone, Some(first_mask), 0, life);
        let before = LIFE_SCRATCH.with(|scratch| {
            let scratch = scratch.borrow();
            (scratch.epoch, scratch.age.clone())
        });

        render_effect(&app, zone, Some(second_mask), 0, life);
        LIFE_SCRATCH.with(|scratch| {
            let scratch = scratch.borrow();
            assert_eq!(scratch.epoch, before.0);
            assert_eq!(scratch.age, before.1);
        });

        let buffer = render_effect(&app, zone, Some(second_mask), LIFE_STEP_FRAMES, life);
        LIFE_SCRATCH.with(|scratch| {
            let scratch = scratch.borrow();
            assert_eq!(scratch.last_step, LIFE_STEP_FRAMES);
            assert_ne!(scratch.age, before.1);
        });
        assert!(painted_cells(&buffer, zone) > 0);
        assert_eq!(painted_cells(&buffer, second_mask), 0);
    }

    #[test]
    fn pipes_mask_changes_do_not_reset_the_scene() {
        PIPES_SCRATCH.with(|scratch| *scratch.borrow_mut() = PipesScratch::default());
        let app = App::new(100);
        let zone = Rect::new(4, 2, 40, 14);
        let first_mask = Rect::new(10, 5, 10, 5);
        let second_mask = Rect::new(24, 7, 8, 4);

        let first = render_effect(&app, zone, Some(first_mask), 1, pipes);
        assert!(painted_cells(&first, zone) > 0);
        let before = PIPES_SCRATCH.with(|scratch| {
            let scratch = scratch.borrow();
            (scratch.epoch, scratch.cells.clone())
        });

        let second = render_effect(&app, zone, Some(second_mask), 2, pipes);
        PIPES_SCRATCH.with(|scratch| {
            let scratch = scratch.borrow();
            assert_eq!(scratch.epoch, before.0);
            assert_eq!(scratch.last_f, 2);
            assert_ne!(scratch.cells, before.1);
        });
        assert!(painted_cells(&second, zone) > 0);
        assert_eq!(painted_cells(&second, second_mask), 0);
    }

    #[test]
    fn plasma_reuses_scratch_capacity_and_skips_the_art_mask() {
        PLASMA_SCRATCH.with(|scratch| *scratch.borrow_mut() = PlasmaScratch::default());
        let app = App::new(100);
        let zone = Rect::new(4, 2, 40, 14);
        let mask = Rect::new(14, 6, 16, 7);

        render_effect(&app, zone, Some(mask), 1, plasma);
        let capacities = PLASMA_SCRATCH.with(|scratch| {
            let scratch = scratch.borrow();
            (
                scratch.col_wave.capacity(),
                scratch.row_wave.capacity(),
                scratch.diag_wave.capacity(),
            )
        });
        let buffer = render_effect(&app, zone, Some(mask), 2, plasma);
        PLASMA_SCRATCH.with(|scratch| {
            let scratch = scratch.borrow();
            assert_eq!(
                (
                    scratch.col_wave.capacity(),
                    scratch.row_wave.capacity(),
                    scratch.diag_wave.capacity(),
                ),
                capacities
            );
        });
        assert_eq!(painted_cells(&buffer, mask), 0);
    }
}
