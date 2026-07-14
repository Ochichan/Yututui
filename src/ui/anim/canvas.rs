//! Canvas composition and first-wave effects. [`render_canvas_composite`] paints each enabled
//! effect once in its assigned coordinate space and hard-masks album art for every write.

use std::cell::{Cell, RefCell};
use std::mem::size_of;
use std::sync::OnceLock;

use ratatui::Frame;
use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use ratatui::style::{Color, Modifier, Style};

use super::{bar_blocks, canvas_ext, canvas_sim, hash32, lerp_color, put_char, wave};
use crate::app::App;
use crate::theme::ThemeRole as R;

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct CanvasActivity {
    pub active: bool,
    pub heavy: bool,
}

#[derive(Clone, Copy, Debug, Default)]
pub(super) struct CellSpan {
    pub start: u16,
    pub end: u16,
}

/// Direct writer for canvas effects. Album art is a hard exclusion even though the image widget
/// renders later: Kitty album art intentionally sits below terminal text, so merely relying on
/// widget order would still let an animation glyph shine through the image.
pub(super) struct CanvasWriter<'a> {
    buffer: &'a mut Buffer,
    art_mask: Option<Rect>,
}

impl<'a> CanvasWriter<'a> {
    pub(super) fn new(buffer: &'a mut Buffer, art_mask: Option<Rect>) -> Self {
        Self { buffer, art_mask }
    }

    pub(super) fn put(&mut self, x: u16, y: u16, ch: char, style: Style) {
        if self.masked(x, y) {
            return;
        }
        put_char(self.buffer, x, y, ch, style);
    }

    /// Patch only a cell's character and foreground colour while preserving its background and
    /// modifiers. Most canvas effects use exactly `Style::default().fg(fg)`; keeping that intent
    /// explicit avoids constructing and merging a temporary [`Style`] for every sparse glyph.
    pub(super) fn put_fg(&mut self, x: u16, y: u16, ch: char, fg: Color) {
        if self.masked(x, y) {
            return;
        }
        if let Some(cell) = self.buffer.cell_mut((x, y)) {
            cell.set_char(ch);
            cell.fg = fg;
        }
    }

    /// Write a cell already proven visible by [`Self::visible_spans`]. Callers must keep the
    /// coordinates inside both the returned span and the source zone. This skips the duplicate
    /// album-art and coordinate tests on the hottest full-field loops. These effects only patch
    /// foreground colour, so assigning it directly also avoids rebuilding a one-field `Style`.
    pub(super) fn put_visible_fg(&mut self, x: u16, y: u16, ch: char, fg: Color) {
        let area = self.buffer.area;
        let index = usize::from(y - area.y) * usize::from(area.width) + usize::from(x - area.x);
        let cell = &mut self.buffer.content[index];
        cell.set_char(ch);
        cell.fg = fg;
    }

    fn masked(&self, x: u16, y: u16) -> bool {
        self.art_mask.is_some_and(|mask| {
            x >= mask.left() && x < mask.right() && y >= mask.top() && y < mask.bottom()
        })
    }

    /// Horizontal spans in `zone` that remain visible after subtracting album art. The fixed
    /// two-span result lets full-field effects skip masked cells without allocating a temporary
    /// buffer or paying a mask branch for every cell.
    pub(super) fn visible_spans(&self, zone: Rect, y: u16) -> ([CellSpan; 2], usize) {
        let full = CellSpan {
            start: zone.left(),
            end: zone.right(),
        };
        let Some(mask) = self.art_mask else {
            return (
                [full, CellSpan::default()],
                usize::from(full.start < full.end),
            );
        };
        if y < mask.top() || y >= mask.bottom() {
            return (
                [full, CellSpan::default()],
                usize::from(full.start < full.end),
            );
        }
        let cut_left = mask.left().clamp(zone.left(), zone.right());
        let cut_right = mask.right().clamp(zone.left(), zone.right());
        if cut_left >= cut_right {
            return (
                [full, CellSpan::default()],
                usize::from(full.start < full.end),
            );
        }
        let spans = [
            CellSpan {
                start: zone.left(),
                end: cut_left,
            },
            CellSpan {
                start: cut_right,
                end: zone.right(),
            },
        ];
        match (spans[0].start < spans[0].end, spans[1].start < spans[1].end) {
            (true, true) => (spans, 2),
            (true, false) => ([spans[0], CellSpan::default()], 1),
            (false, true) => ([spans[1], CellSpan::default()], 1),
            (false, false) => ([CellSpan::default(); 2], 0),
        }
    }
}

const SCRATCH_SHRINK_MIN_EXCESS_BYTES: usize = 64 * 1024;

/// Prepare a reusable scratch vector for a geometry-derived rebuild. Capacity is retained across
/// ordinary size changes unless it is both four times the new need and at least 64 KiB excessive.
/// This keeps resize churn low while eventually returning memory after a very large terminal is
/// replaced by a much smaller one.
pub(super) fn reset_scratch_vec<T>(values: &mut Vec<T>, needed: usize) {
    let element_bytes = size_of::<T>();
    let capacity_bytes = values.capacity().saturating_mul(element_bytes);
    let needed_bytes = needed.saturating_mul(element_bytes);
    let shrink_threshold = needed_bytes
        .saturating_mul(4)
        .max(needed_bytes.saturating_add(SCRATCH_SHRINK_MIN_EXCESS_BYTES));
    if element_bytes != 0 && capacity_bytes > shrink_threshold {
        *values = Vec::with_capacity(needed);
    } else {
        values.clear();
        values.reserve(needed);
    }
}

/// Resize a scratch vector while retaining the same overlapping prefix as [`Vec::resize`]. This
/// is required for the donut's historical first frame after geometry changes; only capacity may
/// change when the old allocation is far larger than the new grid.
fn resize_scratch_vec_preserving<T: Clone>(values: &mut Vec<T>, needed: usize, fill: T) {
    let element_bytes = size_of::<T>();
    let capacity_bytes = values.capacity().saturating_mul(element_bytes);
    let needed_bytes = needed.saturating_mul(element_bytes);
    let shrink_threshold = needed_bytes
        .saturating_mul(4)
        .max(needed_bytes.saturating_add(SCRATCH_SHRINK_MIN_EXCESS_BYTES));
    if element_bytes != 0 && capacity_bytes > shrink_threshold {
        let retained = values.len().min(needed);
        let mut replacement = Vec::with_capacity(needed);
        replacement.extend_from_slice(&values[..retained]);
        replacement.resize(needed, fill);
        *values = replacement;
    } else {
        values.resize(needed, fill);
    }
}

fn fixed_wave_table<const N: usize>() -> [f64; N] {
    std::array::from_fn(|i| wave(i as u64, N as u64))
}

pub(super) fn wave_24_table() -> &'static [f64; 24] {
    static TABLE: OnceLock<[f64; 24]> = OnceLock::new();
    TABLE.get_or_init(fixed_wave_table::<24>)
}

pub(super) fn wave_70_table() -> &'static [f64; 70] {
    static TABLE: OnceLock<[f64; 70]> = OnceLock::new();
    TABLE.get_or_init(fixed_wave_table::<70>)
}

pub(super) fn wave_110_table() -> &'static [f64; 110] {
    static TABLE: OnceLock<[f64; 110]> = OnceLock::new();
    TABLE.get_or_init(fixed_wave_table::<110>)
}

fn wave_14_table() -> &'static [f64; 14] {
    static TABLE: OnceLock<[f64; 14]> = OnceLock::new();
    TABLE.get_or_init(fixed_wave_table::<14>)
}

fn wave_9_table() -> &'static [f64; 9] {
    static TABLE: OnceLock<[f64; 9]> = OnceLock::new();
    TABLE.get_or_init(fixed_wave_table::<9>)
}

const MAX_FREE_RECTS: usize = 16;
const MAX_FOCAL_OPTIONS: usize = 64;

#[derive(Clone, Copy)]
struct RectList<const N: usize> {
    rects: [Rect; N],
    len: usize,
}

impl<const N: usize> RectList<N> {
    fn new() -> Self {
        Self {
            rects: [Rect::ZERO; N],
            len: 0,
        }
    }

    fn push(&mut self, rect: Rect) {
        if !rect.is_empty() && self.len < N {
            self.rects[self.len] = rect;
            self.len += 1;
        }
    }

    fn as_slice(&self) -> &[Rect] {
        &self.rects[..self.len]
    }
}

fn subtract_rect(source: Rect, exclusion: Rect, out: &mut RectList<MAX_FREE_RECTS>) {
    let cut = source.intersection(exclusion);
    if cut.is_empty() {
        out.push(source);
        return;
    }
    out.push(Rect::new(
        source.left(),
        source.top(),
        source.width,
        cut.top().saturating_sub(source.top()),
    ));
    out.push(Rect::new(
        source.left(),
        cut.bottom(),
        source.width,
        source.bottom().saturating_sub(cut.bottom()),
    ));
    out.push(Rect::new(
        source.left(),
        cut.top(),
        cut.left().saturating_sub(source.left()),
        cut.height,
    ));
    out.push(Rect::new(
        cut.right(),
        cut.top(),
        source.right().saturating_sub(cut.right()),
        cut.height,
    ));
}

fn subtract_all(
    free: RectList<MAX_FREE_RECTS>,
    exclusion: Option<Rect>,
) -> RectList<MAX_FREE_RECTS> {
    let Some(exclusion) = exclusion.filter(|rect| !rect.is_empty()) else {
        return free;
    };
    let mut next = RectList::new();
    for rect in free.as_slice() {
        subtract_rect(*rect, exclusion, &mut next);
    }
    next
}

fn art_halo(art: Rect, bounds: Rect) -> Rect {
    let left = art.left().saturating_sub(1).max(bounds.left());
    let top = art.top().saturating_sub(1).max(bounds.top());
    let right = art.right().saturating_add(1).min(bounds.right());
    let bottom = art.bottom().saturating_add(1).min(bounds.bottom());
    Rect::new(
        left,
        top,
        right.saturating_sub(left),
        bottom.saturating_sub(top),
    )
}

fn fit_two_to_one(area: Rect, min_width: u16, min_height: u16) -> Option<Rect> {
    let height = area.height.min(area.width / 2);
    let width = height.saturating_mul(2);
    if width < min_width || height < min_height {
        return None;
    }
    Some(Rect::new(
        area.x + area.width.saturating_sub(width) / 2,
        area.y + area.height.saturating_sub(height) / 2,
        width,
        height,
    ))
}

fn focal_options(
    free: &RectList<MAX_FREE_RECTS>,
    min_width: u16,
    min_height: u16,
) -> RectList<MAX_FOCAL_OPTIONS> {
    let mut options = RectList::new();
    for area in free.as_slice() {
        if let Some(rect) = fit_two_to_one(*area, min_width, min_height) {
            options.push(rect);
        }

        let left_width = area.width / 2;
        let right_width = area.width.saturating_sub(left_width);
        let top_height = area.height / 2;
        let bottom_height = area.height.saturating_sub(top_height);
        for part in [
            Rect::new(area.x, area.y, left_width, area.height),
            Rect::new(
                area.x.saturating_add(left_width),
                area.y,
                right_width,
                area.height,
            ),
            Rect::new(area.x, area.y, area.width, top_height),
            Rect::new(
                area.x,
                area.y.saturating_add(top_height),
                area.width,
                bottom_height,
            ),
        ] {
            if let Some(rect) = fit_two_to_one(part, min_width, min_height) {
                options.push(rect);
            }
        }

        // Equal halves alone miss valid asymmetric pairings (for example, a 12x6 cube beside a
        // 10x5 donut in a 22x6 strip). Edge-anchored minimum slices make every minimum-sized
        // horizontal or vertical pairing available to the fixed-array pair search.
        if area.width >= min_width {
            for part in [
                Rect::new(area.x, area.y, min_width, area.height),
                Rect::new(
                    area.right().saturating_sub(min_width),
                    area.y,
                    min_width,
                    area.height,
                ),
            ] {
                if let Some(rect) = fit_two_to_one(part, min_width, min_height) {
                    options.push(rect);
                }
            }
        }
        if area.height >= min_height {
            for part in [
                Rect::new(area.x, area.y, area.width, min_height),
                Rect::new(
                    area.x,
                    area.bottom().saturating_sub(min_height),
                    area.width,
                    min_height,
                ),
            ] {
                if let Some(rect) = fit_two_to_one(part, min_width, min_height) {
                    options.push(rect);
                }
            }
        }
    }
    options
}

fn rect_score(rect: Rect) -> u32 {
    u32::from(rect.width) * u32::from(rect.height)
}

fn best_single(options: &RectList<MAX_FOCAL_OPTIONS>) -> Option<Rect> {
    options
        .as_slice()
        .iter()
        .copied()
        .max_by_key(|rect| rect_score(*rect))
}

type FocalPlacement = (Option<Rect>, Option<Rect>);

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct FocalPlacementKey {
    player_rect: Rect,
    art_mask: Option<Rect>,
    lyrics_rect: Option<Rect>,
    cube_on: bool,
    donut_on: bool,
}

thread_local! {
    /// Exactly one entry is enough for steady-state rendering, where geometry and toggles are
    /// unchanged for thousands of frames. A different player surface simply evicts it.
    static FOCAL_PLACEMENT_CACHE: Cell<Option<(FocalPlacementKey, FocalPlacement)>> = const {
        Cell::new(None)
    };
}

fn focal_placements_uncached(
    player_rect: Rect,
    art_mask: Option<Rect>,
    lyrics_rect: Option<Rect>,
    cube_on: bool,
    donut_on: bool,
) -> FocalPlacement {
    if !cube_on && !donut_on {
        return (None, None);
    }

    let mut free = RectList::new();
    free.push(player_rect);
    free = subtract_all(free, art_mask.map(|art| art_halo(art, player_rect)));
    free = subtract_all(
        free,
        lyrics_rect.map(|lyrics| lyrics.intersection(player_rect)),
    );

    if cube_on && donut_on {
        let cube = focal_options(&free, 12, 6);
        let donut = focal_options(&free, 10, 5);
        let mut pair = None;
        let mut pair_score = 0u32;
        for cube_rect in cube.as_slice() {
            for donut_rect in donut.as_slice() {
                if !cube_rect.intersection(*donut_rect).is_empty() {
                    continue;
                }
                let score = rect_score(*cube_rect).saturating_add(rect_score(*donut_rect));
                if pair.is_none() || score > pair_score {
                    pair = Some((*cube_rect, *donut_rect));
                    pair_score = score;
                }
            }
        }
        if let Some((cube, donut)) = pair {
            return (Some(cube), Some(donut));
        }
        let cube = best_single(&cube);
        let donut = best_single(&donut);
        return match (cube, donut) {
            (Some(cube), Some(donut)) if rect_score(donut) > rect_score(cube) => {
                (None, Some(donut))
            }
            (cube, _) if cube.is_some() => (cube, None),
            (_, donut) => (None, donut),
        };
    }
    if cube_on {
        let cube = focal_options(&free, 12, 6);
        return (best_single(&cube), None);
    }
    let donut = focal_options(&free, 10, 5);
    (None, best_single(&donut))
}

fn focal_placements(
    player_rect: Rect,
    art_mask: Option<Rect>,
    lyrics_rect: Option<Rect>,
    cube_on: bool,
    donut_on: bool,
) -> FocalPlacement {
    let key = FocalPlacementKey {
        player_rect,
        art_mask,
        lyrics_rect,
        cube_on,
        donut_on,
    };
    FOCAL_PLACEMENT_CACHE.with(|cache| {
        if let Some((cached_key, placement)) = cache.get()
            && cached_key == key
        {
            return placement;
        }
        let placement =
            focal_placements_uncached(player_rect, art_mask, lyrics_rect, cube_on, donut_on);
        cache.set(Some((key, placement)));
        placement
    })
}

fn supports(zone: Rect, min_width: u16, min_height: u16) -> bool {
    zone.width >= min_width && zone.height >= min_height
}

/// Composite every enabled canvas effect exactly once in a stable coordinate system. Field
/// effects use the full Player interior; scene effects use the filler stage; focal effects are
/// assigned to allocation-free 2:1 safe regions outside album art (plus a one-cell halo) and
/// lyrics. Album art remains a hard write mask for every family.
pub fn render_canvas_composite(
    frame: &mut Frame,
    app: &App,
    player_rect: Rect,
    stage_rect: Rect,
    art_mask: Option<Rect>,
    lyrics_rect: Option<Rect>,
    suppress_donut: bool,
) -> CanvasActivity {
    let a = app.animations();
    let mut activity = CanvasActivity::default();
    if !a.master {
        app.bridges.canvas_active.set(false);
        app.bridges.canvas_heavy_active.set(false);
        return activity;
    }
    let f = app.anim_frame();
    let (cube_rect, donut_rect) = focal_placements(
        stage_rect,
        art_mask,
        lyrics_rect,
        a.cube,
        a.donut && !suppress_donut,
    );
    let mut canvas = CanvasWriter::new(frame.buffer_mut(), art_mask);
    if a.plasma && supports(player_rect, 4, 2) {
        canvas_sim::plasma(&mut canvas, app, player_rect, f);
        activity.active = true;
        activity.heavy = true;
    }
    if a.life && supports(player_rect, 8, 6) {
        canvas_sim::life(&mut canvas, app, player_rect, f);
        activity.active = true;
        activity.heavy = true;
    }
    if a.pipes && supports(player_rect, 10, 5) {
        canvas_sim::pipes(&mut canvas, app, player_rect, f);
        activity.active = true;
        activity.heavy = true;
    }
    if a.rain && supports(player_rect, 4, 2) {
        rain(&mut canvas, app, player_rect, f);
        activity.active = true;
        activity.heavy = true;
    }
    if a.snow && supports(player_rect, 4, 2) {
        canvas_ext::snow(&mut canvas, app, player_rect, f);
        activity.active = true;
        activity.heavy = true;
    }
    if a.starfield && supports(player_rect, 4, 2) {
        starfield(&mut canvas, app, player_rect, f);
        activity.active = true;
        activity.heavy = true;
    }
    if a.fireflies && supports(player_rect, 8, 3) {
        canvas_ext::fireflies(&mut canvas, app, player_rect, f);
        activity.active = true;
        activity.heavy = true;
    }
    if a.comets && supports(player_rect, 10, 4) {
        canvas_ext::comets(&mut canvas, app, player_rect, f);
        activity.active = true;
        activity.heavy = true;
    }
    if a.waves && supports(stage_rect, 8, 3) {
        canvas_ext::waves(&mut canvas, app, stage_rect, f);
        activity.active = true;
        activity.heavy = true;
    }
    if a.visualizer && supports(stage_rect, 4, 2) {
        visualizer(&mut canvas, app, stage_rect, f);
        activity.active = true;
        activity.heavy = true;
    }
    if a.fireworks && supports(stage_rect, 16, 6) {
        canvas_sim::fireworks(&mut canvas, app, stage_rect, f);
        activity.active = true;
        activity.heavy = true;
    }
    if a.aquarium && supports(stage_rect, 14, 3) {
        canvas_ext::aquarium(&mut canvas, app, stage_rect, f);
        activity.active = true;
        activity.heavy = true;
    }
    if a.bounce && supports(stage_rect, 10, 2) {
        bounce(&mut canvas, app, stage_rect, f);
        activity.active = true;
    }
    if let Some(zone) = cube_rect {
        canvas_ext::cube(&mut canvas, app, zone, f);
        activity.active = true;
        activity.heavy = true;
    }
    if let Some(zone) = donut_rect {
        donut(&mut canvas, app, zone, f);
        activity.active = true;
        activity.heavy = true;
    }
    app.bridges.canvas_active.set(activity.active);
    app.bridges.canvas_heavy_active.set(activity.heavy);
    activity
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
fn rain(canvas: &mut CanvasWriter<'_>, _app: &App, zone: Rect, f: u64) {
    let h = u64::from(zone.height);
    RAIN_SCRATCH.with(|scratch| {
        let mut scratch = scratch.borrow_mut();
        if scratch.width != zone.width || scratch.height != zone.height {
            scratch.width = zone.width;
            scratch.height = zone.height;
            reset_scratch_vec(&mut scratch.columns, usize::from(zone.width));
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
                canvas.put_fg(x, y, g, color);
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
fn starfield(canvas: &mut CanvasWriter<'_>, app: &App, zone: Rect, f: u64) {
    let w = u32::from(zone.width);
    let h = u64::from(zone.height);
    let count = ((w * u32::from(zone.height)) / 40).clamp(6, 60);
    let dim = app.theme.color(R::TextSubtle);
    let bright = app.theme.color(R::AccentAlt);
    let wave24 = wave_24_table();
    STAR_SCRATCH.with(|scratch| {
        let mut scratch = scratch.borrow_mut();
        if scratch.width != zone.width || scratch.height != zone.height || scratch.count != count {
            scratch.width = zone.width;
            scratch.height = zone.height;
            scratch.count = count;
            reset_scratch_vec(&mut scratch.stars, count as usize);
            for i in 0..u64::from(count) {
                let x = (hash32(i * 2 + 1) % w) as u16;
                let speed = 1 + u64::from(hash32(i * 3 + 2) % 4);
                let seed = u64::from(hash32(i * 5 + 3));
                scratch.stars.push(Star { x, speed, seed });
            }
        }

        for (i, star) in scratch.stars.iter().enumerate() {
            let i = i as u64;
            let yv = (f / star.speed + star.seed) % (h + 4);
            if yv >= h {
                continue; // off-screen pause → a twinkle gap
            }
            let y = zone.top() + (h - 1 - yv) as u16;
            let g = STAR_GLYPHS[(hash32(i * 7 + f / 20) as usize) % STAR_GLYPHS.len()];
            let color = lerp_color(dim, bright, wave24[((f + i * 5) % 24) as usize]);
            canvas.put_fg(zone.left() + star.x, y, g, color);
        }
    });
}

/// A decorative (non-audio-reactive) spectrum: bars rising from the bottom strip of the zone, each
/// mixing two waves, coloured from the gauge colour up to the accent.
fn visualizer(canvas: &mut CanvasWriter<'_>, app: &App, zone: Rect, f: u64) {
    let blocks = bar_blocks(app);
    let strip = (zone.height / 3).clamp(2, 8);
    let baseline = i32::from(zone.bottom());
    let top = i32::from(zone.top());
    let filled = app.theme.color(R::GaugeFilled);
    let accent = app.theme.color(R::Accent);
    let wave14 = wave_14_table();
    let wave9 = wave_9_table();
    let row_colors: [Color; 8] = std::array::from_fn(|i| {
        let row = i as u16;
        if row < strip {
            lerp_color(filled, accent, f64::from(row + 1) / f64::from(strip))
        } else {
            Color::Reset
        }
    });
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
            let color = row_colors[row as usize];
            if row < full {
                canvas.put_fg(x, y, '█', color);
            } else if row == full && frac > 0.05 {
                let idx = (frac * (blocks.len() - 1) as f64).round() as usize;
                canvas.put_fg(x, y, blocks[idx.min(blocks.len() - 1)], color);
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
fn donut(canvas: &mut CanvasWriter<'_>, app: &App, zone: Rect, f: u64) {
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
    let colors: [Color; 12] = std::array::from_fn(|ci| {
        let t = ci as f64 / (DONUT_LUM.len() - 1) as f64;
        lerp_color(base, bright, t)
    });

    DONUT_SCRATCH.with(|scratch| {
        let mut scratch = scratch.borrow_mut();
        if scratch.zbuf.len() != cells {
            resize_scratch_vec_preserving(&mut scratch.zbuf, cells, 0.0);
            resize_scratch_vec_preserving(&mut scratch.out, cells, 0);
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

        for yy in 0..h {
            for xx in 0..w {
                let v = scratch.out[(xx + yy * w) as usize];
                if v == 0 {
                    continue;
                }
                let ci = (v as usize - 1).min(DONUT_LUM.len() - 1);
                canvas.put_fg(
                    zone.left() + xx as u16,
                    zone.top() + yy as u16,
                    DONUT_LUM[ci] as char,
                    colors[ci],
                );
            }
        }
    });
}

/// DVD-style bouncing logo: an ASCII tag ricochets around the zone on a triangle-wave path, its
/// colour cycling each time it crosses a wall.
fn bounce(canvas: &mut CanvasWriter<'_>, app: &App, zone: Rect, f: u64) {
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
    for (i, ch) in LABEL.chars().enumerate() {
        let cx = x + i as i64;
        if cx < i64::from(zone.left()) || cx >= right {
            continue;
        }
        canvas.put(
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

    fn render_zone_with_mask(app: &App, zone: Rect, art_mask: Option<Rect>) -> Buffer {
        let backend = TestBackend::new(50, 20);
        let mut terminal = Terminal::new(backend).unwrap();
        terminal
            .draw(|f| {
                render_canvas_composite(f, app, zone, zone, art_mask, None, false);
            })
            .unwrap();
        terminal.backend().buffer().clone()
    }

    fn render_zone(app: &App, zone: Rect) -> Buffer {
        render_zone_with_mask(app, zone, None)
    }

    fn render_regions(app: &App, player: Rect, stage: Rect) -> Buffer {
        let backend = TestBackend::new(50, 20);
        let mut terminal = Terminal::new(backend).unwrap();
        terminal
            .draw(|frame| {
                render_canvas_composite(frame, app, player, stage, None, None, false);
            })
            .unwrap();
        terminal.backend().buffer().clone()
    }

    #[test]
    fn foreground_writer_matches_fg_only_style_patching() {
        let area = Rect::new(0, 0, 6, 4);
        let mask = Rect::new(2, 1, 2, 2);
        let mut styled = Buffer::empty(area);
        for cell in &mut styled.content {
            cell.set_style(
                Style::default()
                    .bg(Color::Blue)
                    .add_modifier(Modifier::ITALIC),
            );
        }
        let mut foreground = styled.clone();
        {
            let mut writer = CanvasWriter::new(&mut styled, Some(mask));
            for (x, y, ch, color) in [
                (1, 1, 'a', Color::Red),
                (2, 1, 'b', Color::Green),
                (4, 3, 'c', Color::Yellow),
                (9, 9, 'd', Color::Cyan),
            ] {
                writer.put(x, y, ch, Style::default().fg(color));
            }
        }
        {
            let mut writer = CanvasWriter::new(&mut foreground, Some(mask));
            for (x, y, ch, color) in [
                (1, 1, 'a', Color::Red),
                (2, 1, 'b', Color::Green),
                (4, 3, 'c', Color::Yellow),
                (9, 9, 'd', Color::Cyan),
            ] {
                writer.put_fg(x, y, ch, color);
            }
        }
        assert_eq!(foreground, styled);
    }

    #[test]
    fn fixed_wave_tables_are_bit_identical_to_direct_evaluation() {
        for (actual, expected) in wave_24_table().iter().zip((0..24).map(|i| wave(i, 24))) {
            assert_eq!(actual.to_bits(), expected.to_bits());
        }
        for (actual, expected) in wave_70_table().iter().zip((0..70).map(|i| wave(i, 70))) {
            assert_eq!(actual.to_bits(), expected.to_bits());
        }
        for (actual, expected) in wave_110_table().iter().zip((0..110).map(|i| wave(i, 110))) {
            assert_eq!(actual.to_bits(), expected.to_bits());
        }
        for (actual, expected) in wave_14_table().iter().zip((0..14).map(|i| wave(i, 14))) {
            assert_eq!(actual.to_bits(), expected.to_bits());
        }
        for (actual, expected) in wave_9_table().iter().zip((0..9).map(|i| wave(i, 9))) {
            assert_eq!(actual.to_bits(), expected.to_bits());
        }
    }

    #[test]
    fn scratch_capacity_downsize_has_four_x_and_64_kib_hysteresis() {
        let mut retained = Vec::<u8>::with_capacity(64 * 1024);
        let retained_capacity = retained.capacity();
        reset_scratch_vec(&mut retained, 1);
        assert_eq!(retained.capacity(), retained_capacity);
        assert!(retained.is_empty());

        let mut downsized = Vec::<u8>::with_capacity(70 * 1024);
        let oversized_capacity = downsized.capacity();
        reset_scratch_vec(&mut downsized, 1);
        assert!(downsized.capacity() < oversized_capacity);
        assert!(downsized.capacity() >= 1);
        assert!(downsized.is_empty());
    }

    #[test]
    fn preserving_resize_matches_vec_resize_prefix_while_downsizing_capacity() {
        let mut expected = Vec::with_capacity(80 * 1024);
        expected.extend(0..70 * 1024u32);
        let mut actual = expected.clone();
        actual.reserve(80 * 1024);
        let oversized_capacity = actual.capacity();

        expected.resize(32, u32::MAX);
        resize_scratch_vec_preserving(&mut actual, 32, u32::MAX);

        assert_eq!(actual, expected);
        assert!(actual.capacity() < oversized_capacity);
    }

    #[test]
    fn one_entry_focal_cache_matches_uncached_placements_and_evicts() {
        FOCAL_PLACEMENT_CACHE.with(|cache| cache.set(None));
        let cases = [
            (
                Rect::new(0, 0, 100, 30),
                Some(Rect::new(4, 6, 30, 12)),
                Some(Rect::new(58, 5, 36, 18)),
                true,
                true,
            ),
            (Rect::new(3, 4, 22, 6), None, None, true, true),
            (
                Rect::new(1, 2, 30, 5),
                None,
                Some(Rect::new(1, 2, 30, 5)),
                false,
                true,
            ),
        ];
        for (player, art, lyrics, cube, donut) in cases {
            let expected = focal_placements_uncached(player, art, lyrics, cube, donut);
            assert_eq!(focal_placements(player, art, lyrics, cube, donut), expected);
            let cached = FOCAL_PLACEMENT_CACHE.with(Cell::get).expect("cache entry");
            assert_eq!(cached.1, expected);
        }
        let cached = FOCAL_PLACEMENT_CACHE
            .with(Cell::get)
            .expect("last cache entry");
        assert_eq!(cached.0.player_rect, cases[2].0);
    }

    /// Every canvas effect must (a) never write a cell outside the zone it was given, and
    /// (b) actually paint something inside it within a few hundred frames. One sweep covers
    /// all 15 flags, so a new effect added to the compositor is tested by construction.
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

    #[test]
    fn every_canvas_family_obeys_the_art_hard_mask() {
        let _guard = crate::i18n::lock_for_test();
        let zone = Rect::new(2, 1, 46, 18);
        let mask = Rect::new(15, 5, 18, 9);
        let mut app = app_with(|a| {
            a.rain = true;
            a.donut = true;
            a.visualizer = true;
            a.starfield = true;
            a.bounce = true;
            a.comets = true;
            a.snow = true;
            a.fireflies = true;
            a.cube = true;
            a.aquarium = true;
            a.waves = true;
            a.fireworks = true;
            a.life = true;
            a.pipes = true;
            a.plasma = true;
        });
        for _ in 0..80 {
            app.update(Msg::AnimTick);
            let buffer = render_zone_with_mask(&app, zone, Some(mask));
            for y in mask.top()..mask.bottom() {
                for x in mask.left()..mask.right() {
                    assert_eq!(buffer[(x, y)].symbol(), " ", "masked write at ({x},{y})");
                }
            }
        }
    }

    #[test]
    fn every_stage_effect_stays_inside_the_filler_stage() {
        let _guard = crate::i18n::lock_for_test();
        type SetFlag = fn(&mut crate::config::AnimationsConfig);
        let flags: [(&str, SetFlag); 5] = [
            ("waves", |a| a.waves = true),
            ("visualizer", |a| a.visualizer = true),
            ("fireworks", |a| a.fireworks = true),
            ("aquarium", |a| a.aquarium = true),
            ("bounce", |a| a.bounce = true),
        ];
        let player = Rect::new(2, 1, 46, 18);
        let stage = Rect::new(10, 9, 30, 9);
        for (name, set) in flags {
            let mut app = app_with(set);
            let mut painted = false;
            for _ in 0..200 {
                app.update(Msg::AnimTick);
                let buffer = render_regions(&app, player, stage);
                for y in 0..buffer.area.height {
                    for x in 0..buffer.area.width {
                        if buffer[(x, y)].symbol() == " " {
                            continue;
                        }
                        assert!(
                            x >= stage.left()
                                && x < stage.right()
                                && y >= stage.top()
                                && y < stage.bottom(),
                            "{name} wrote outside the filler stage at ({x},{y})"
                        );
                        painted = true;
                    }
                }
            }
            assert!(painted, "{name} never painted inside the filler stage");
        }
    }

    #[test]
    fn focal_effects_share_safe_space_and_exclude_art_halo_and_lyrics() {
        let player = Rect::new(0, 0, 100, 30);
        let art = Rect::new(4, 6, 30, 12);
        let lyrics = Rect::new(58, 5, 36, 18);
        let (cube, donut) = focal_placements(player, Some(art), Some(lyrics), true, true);
        let cube = cube.expect("cube should receive a safe region");
        let donut = donut.expect("donut should receive a separate safe region");
        let halo = art_halo(art, player);
        assert!(cube.intersection(donut).is_empty());
        for focal in [cube, donut] {
            assert!(focal.intersection(halo).is_empty());
            assert!(focal.intersection(lyrics).is_empty());
            assert_eq!(focal.width, focal.height * 2);
        }
    }

    #[test]
    fn focal_effects_keep_both_minimum_regions_in_an_asymmetric_strip() {
        let strip = Rect::new(3, 4, 22, 6);
        let (cube, donut) = focal_placements(strip, None, None, true, true);
        let cube = cube.expect("12x6 cube should fit");
        let donut = donut.expect("10x5 donut should fit beside the cube");
        assert!(cube.intersection(donut).is_empty());
        assert_eq!(cube.as_size(), ratatui::layout::Size::new(12, 6));
        assert_eq!(donut.as_size(), ratatui::layout::Size::new(10, 5));
    }

    #[test]
    fn focal_only_activity_stays_off_when_lyrics_consume_the_stage() {
        let _guard = crate::i18n::lock_for_test();
        let app = app_with(|a| {
            a.cube = true;
            a.donut = true;
        });
        let player = Rect::new(1, 1, 30, 12);
        let stage = Rect::new(1, 2, 30, 5);
        let backend = TestBackend::new(32, 14);
        let mut terminal = Terminal::new(backend).unwrap();
        terminal
            .draw(|frame| {
                let activity =
                    render_canvas_composite(frame, &app, player, stage, None, Some(stage), false);
                assert_eq!(activity, CanvasActivity::default());
            })
            .unwrap();
        assert!(!app.bridges.canvas_active.get());
        assert!(!app.bridges.canvas_heavy_active.get());
    }

    #[test]
    fn suppressed_or_unplaceable_donut_preserves_raw_config_but_stays_inactive() {
        let _guard = crate::i18n::lock_for_test();
        let app = app_with(|a| a.donut = true);
        let zone = Rect::new(0, 0, 40, 12);
        let backend = TestBackend::new(40, 12);
        let mut terminal = Terminal::new(backend).unwrap();
        terminal
            .draw(|frame| {
                let activity = render_canvas_composite(
                    frame,
                    &app,
                    zone,
                    zone,
                    Some(Rect::new(4, 2, 30, 8)),
                    None,
                    true,
                );
                assert_eq!(activity, CanvasActivity::default());
            })
            .unwrap();
        assert!(app.config.animations.donut);
        assert!(!app.bridges.canvas_active.get());
        assert!(!app.bridges.canvas_heavy_active.get());
    }
}
