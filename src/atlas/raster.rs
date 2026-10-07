//! Pure dot-grid rasterization and terminal-cell packing for Atlas.

use std::sync::OnceLock;

use crate::atlas::{LandLookup, LatLon, Marker, MarkerKind};

use super::view::{Camera, degrees_per_dot, project_with_frame};

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Renderer {
    Braille,
    Ascii,
}

impl Renderer {
    pub const fn dots_per_cell(self) -> (u16, u16) {
        match self {
            Self::Braille => (2, 4),
            Self::Ascii => (1, 1),
        }
    }
}

/// Screen-space cell rectangle, deliberately independent of the UI framework.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct CellRect {
    pub x: u16,
    pub y: u16,
    pub width: u16,
    pub height: u16,
}

#[repr(u8)]
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum DotClass {
    Outside = 0,
    Ocean,
    Grid,
    Limb,
    Land,
    Coast,
    VisitedLand,
    ActiveLand,
}

pub const CELL_H_OVER_W: f32 = 2.0;

/// Physical-dot geometry. `dot_aspect_y` expresses one vertical dot pitch in horizontal-dot
/// units, which makes Braille dots square and accounts for ASCII cells being twice as tall.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Geometry {
    pub rect: CellRect,
    pub renderer: Renderer,
    pub dots_x: u16,
    pub dots_y: u16,
    pub w: usize,
    pub h: usize,
    pub cx: f32,
    pub cy: f32,
    radius: f32,
    dot_aspect_y: f32,
}

impl Geometry {
    pub fn new(rect: CellRect, renderer: Renderer, scale: f32) -> Self {
        let (dots_x, dots_y) = renderer.dots_per_cell();
        let w = usize::from(rect.width) * usize::from(dots_x);
        let h = usize::from(rect.height) * usize::from(dots_y);
        let dot_aspect_y = CELL_H_OVER_W * f32::from(dots_x) / f32::from(dots_y);
        let scale = if scale.is_finite() {
            scale.max(0.0)
        } else {
            0.0
        };
        let radius = 0.46 * ((w as f32).min(h as f32 * dot_aspect_y)) * scale;
        Self {
            rect,
            renderer,
            dots_x,
            dots_y,
            w,
            h,
            cx: w as f32 * 0.5,
            cy: h as f32 * 0.5,
            radius,
            dot_aspect_y,
        }
    }

    #[inline]
    pub fn radius_dots(&self) -> f32 {
        self.radius
    }

    #[inline]
    pub fn dot_to_disc(&self, dx: usize, dy: usize) -> (f32, f32) {
        if self.radius <= 0.0 {
            return (f32::INFINITY, f32::INFINITY);
        }
        (
            (dx as f32 + 0.5 - self.cx) / self.radius,
            (self.cy - (dy as f32 + 0.5)) * self.dot_aspect_y / self.radius,
        )
    }

    #[inline]
    pub fn cell_to_disc(&self, col: u16, row: u16) -> (f32, f32) {
        if self.radius <= 0.0 {
            return (f32::INFINITY, f32::INFINITY);
        }
        let local_col = i32::from(col) - i32::from(self.rect.x);
        let local_row = i32::from(row) - i32::from(self.rect.y);
        let x = local_col as f32 * f32::from(self.dots_x) + f32::from(self.dots_x) * 0.5;
        let y = local_row as f32 * f32::from(self.dots_y) + f32::from(self.dots_y) * 0.5;
        (
            (x - self.cx) / self.radius,
            (self.cy - y) * self.dot_aspect_y / self.radius,
        )
    }

    #[inline]
    pub fn disc_to_cell(&self, nx: f32, ny: f32) -> Option<(u16, u16)> {
        if !nx.is_finite() || !ny.is_finite() || self.radius <= 0.0 {
            return None;
        }
        let x = nx.mul_add(self.radius, self.cx);
        let y = self.cy - ny * self.radius / self.dot_aspect_y;
        if x < 0.0 || y < 0.0 || x >= self.w as f32 || y >= self.h as f32 {
            return None;
        }
        let local_col = (x / f32::from(self.dots_x)) as u16;
        let local_row = (y / f32::from(self.dots_y)) as u16;
        if local_col >= self.rect.width || local_row >= self.rect.height {
            return None;
        }
        Some((self.rect.x + local_col, self.rect.y + local_row))
    }

    #[inline]
    fn limb_width(&self) -> f32 {
        if self.radius > 0.0 {
            self.dot_aspect_y.max(1.0) / self.radius
        } else {
            0.0
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DotGrid {
    pub w: usize,
    pub h: usize,
    pub class: Vec<DotClass>,
}

pub struct RasterParams<'a> {
    pub land: &'a dyn LandLookup,
    pub active: Option<&'a dyn LandLookup>,
    pub visited: Option<&'a dyn LandLookup>,
    pub grid: bool,
}

const ASIN_SEGMENTS: usize = 4096;
const ATAN_SEGMENTS: usize = 1024;
const DEG_PER_RAD: f32 = 180.0 / std::f32::consts::PI;

struct TrigLuts {
    asin: Box<[f32]>,
    atan: Box<[f32]>,
}

static TRIG_LUTS: OnceLock<TrigLuts> = OnceLock::new();

fn trig_luts() -> &'static TrigLuts {
    TRIG_LUTS.get_or_init(|| TrigLuts {
        asin: (0..=ASIN_SEGMENTS)
            .map(|index| {
                let value = index as f32 * 2.0 / ASIN_SEGMENTS as f32 - 1.0;
                value.asin()
            })
            .collect(),
        atan: (0..=ATAN_SEGMENTS)
            .map(|index| (index as f32 / ATAN_SEGMENTS as f32).atan())
            .collect(),
    })
}

#[inline]
fn interpolate(table: &[f32], position: f32, segments: usize) -> f32 {
    let position = position.clamp(0.0, segments as f32);
    let lower = position as usize;
    if lower >= segments {
        return table[segments];
    }
    table[lower] + (table[lower + 1] - table[lower]) * (position - lower as f32)
}

#[inline]
fn fast_asin(value: f32, luts: &TrigLuts) -> f32 {
    interpolate(
        &luts.asin,
        (value.clamp(-1.0, 1.0) + 1.0) * 0.5 * ASIN_SEGMENTS as f32,
        ASIN_SEGMENTS,
    )
}

/// Uses one LUT for the first octant, then reconstructs the quadrant from signs.
#[inline]
fn fast_atan2(y: f32, x: f32, luts: &TrigLuts) -> f32 {
    let ay = y.abs();
    let ax = x.abs();
    if ax == 0.0 {
        return if y > 0.0 {
            std::f32::consts::FRAC_PI_2
        } else if y < 0.0 {
            -std::f32::consts::FRAC_PI_2
        } else {
            0.0
        };
    }
    let base = if ax >= ay {
        interpolate(&luts.atan, ay / ax * ATAN_SEGMENTS as f32, ATAN_SEGMENTS)
    } else {
        std::f32::consts::FRAC_PI_2
            - interpolate(&luts.atan, ax / ay * ATAN_SEGMENTS as f32, ATAN_SEGMENTS)
    };
    match (x >= 0.0, y >= 0.0) {
        (true, true) => base,
        (true, false) => -base,
        (false, true) => std::f32::consts::PI - base,
        (false, false) => base - std::f32::consts::PI,
    }
}

#[inline]
fn wrap_nearby_longitude(lon: f32) -> f32 {
    if lon > 180.0 {
        lon - 360.0
    } else if lon <= -180.0 {
        lon + 360.0
    } else {
        lon
    }
}

#[inline]
fn near_graticule_line(degrees: f32, epsilon: f32) -> bool {
    let remainder = degrees.rem_euclid(30.0);
    remainder.min(30.0 - remainder) <= epsilon
}

pub fn rasterize(geom: &Geometry, cam: &Camera, p: &RasterParams<'_>) -> DotGrid {
    let mut class = vec![DotClass::Outside; geom.w * geom.h];
    if geom.radius <= 0.0 || geom.w == 0 || geom.h == 0 {
        return DotGrid {
            w: geom.w,
            h: geom.h,
            class,
        };
    }

    let frame = cam.frame();
    let luts = trig_luts();
    let inverse_radius = 1.0 / geom.radius;
    let limb_limit = 1.0 + geom.limb_width();
    let limb_limit_squared = limb_limit * limb_limit;
    let grid_epsilon = 0.6 * degrees_per_dot(geom.radius);

    for row in 0..geom.h {
        let ny = (geom.cy - (row as f32 + 0.5)) * geom.dot_aspect_y * inverse_radius;
        let ny_squared = ny * ny;
        for column in 0..geom.w {
            let nx = (column as f32 + 0.5 - geom.cx) * inverse_radius;
            let rho_squared = nx.mul_add(nx, ny_squared);
            let index = row * geom.w + column;
            if rho_squared > 1.0 {
                if rho_squared <= limb_limit_squared {
                    class[index] = DotClass::Limb;
                }
                continue;
            }

            let nz = (1.0 - rho_squared).max(0.0).sqrt();
            let sin_lat = ny.mul_add(frame.cos_lat, nz * frame.sin_lat);
            let lat = fast_asin(sin_lat, luts) * DEG_PER_RAD;
            let lon = wrap_nearby_longitude(
                frame.centre_lon_deg
                    + fast_atan2(nx, nz.mul_add(frame.cos_lat, -ny * frame.sin_lat), luts)
                        * DEG_PER_RAD,
            );
            let at = LatLon::new(lat, lon);
            let mut dot_class = if p.land.is_land(at) {
                if p.active.is_some_and(|active| active.is_land(at)) {
                    DotClass::ActiveLand
                } else if p.visited.is_some_and(|visited| visited.is_land(at)) {
                    DotClass::VisitedLand
                } else {
                    DotClass::Land
                }
            } else {
                DotClass::Ocean
            };
            if dot_class == DotClass::Ocean
                && p.grid
                && ((lat.abs() < 90.0 - grid_epsilon && near_graticule_line(lat, grid_epsilon))
                    || near_graticule_line(lon, grid_epsilon))
            {
                dot_class = DotClass::Grid;
            }
            class[index] = dot_class;
        }
    }

    for row in 0..geom.h {
        for column in 0..geom.w {
            let index = row * geom.w + column;
            if class[index] == DotClass::Land
                && has_coast_neighbour(&class, geom.w, geom.h, column, row)
            {
                class[index] = DotClass::Coast;
            }
        }
    }

    DotGrid {
        w: geom.w,
        h: geom.h,
        class,
    }
}

#[inline]
fn has_coast_neighbour(class: &[DotClass], w: usize, h: usize, x: usize, y: usize) -> bool {
    (x > 0 && coast_neighbour(class[y * w + x - 1]))
        || (x + 1 < w && coast_neighbour(class[y * w + x + 1]))
        || (y > 0 && coast_neighbour(class[(y - 1) * w + x]))
        || (y + 1 < h && coast_neighbour(class[(y + 1) * w + x]))
}

#[inline]
fn coast_neighbour(class: DotClass) -> bool {
    matches!(
        class,
        DotClass::Ocean | DotClass::Grid | DotClass::Outside | DotClass::Limb
    )
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct PackedCell {
    pub col: u16,
    pub row: u16,
    pub ch: char,
    pub class: DotClass,
}

pub fn pack(geom: &Geometry, grid: &DotGrid, renderer: Renderer) -> Vec<PackedCell> {
    debug_assert_eq!(grid.w, geom.w);
    debug_assert_eq!(grid.h, geom.h);
    debug_assert_eq!(
        renderer, geom.renderer,
        "pack renderer must match the geometry's"
    );
    match renderer {
        Renderer::Braille => pack_braille(geom, grid),
        Renderer::Ascii => pack_ascii(geom, grid),
    }
}

fn pack_braille(geom: &Geometry, grid: &DotGrid) -> Vec<PackedCell> {
    let mut packed = Vec::new();
    for row in 0..geom.rect.height {
        for col in 0..geom.rect.width {
            let mut bits = 0_u8;
            let mut best = DotClass::Outside;
            for dot_y in 0..4_usize {
                for dot_x in 0..2_usize {
                    let x = usize::from(col) * 2 + dot_x;
                    let y = usize::from(row) * 4 + dot_y;
                    let dot_class = grid.class[y * grid.w + x];
                    if ink_priority(dot_class) > 0 {
                        bits |= braille_bit(dot_x, dot_y);
                        if ink_priority(dot_class) > ink_priority(best) {
                            best = dot_class;
                        }
                    }
                }
            }
            if bits != 0 {
                packed.push(PackedCell {
                    col: geom.rect.x + col,
                    row: geom.rect.y + row,
                    ch: char::from_u32(0x2800 | u32::from(bits)).unwrap_or('\u{2800}'),
                    class: best,
                });
            }
        }
    }
    packed
}

#[inline]
fn braille_bit(x: usize, y: usize) -> u8 {
    match (x, y) {
        (0, 0) => 0x01,
        (0, 1) => 0x02,
        (0, 2) => 0x04,
        (0, 3) => 0x40,
        (1, 0) => 0x08,
        (1, 1) => 0x10,
        (1, 2) => 0x20,
        (1, 3) => 0x80,
        _ => 0,
    }
}

fn pack_ascii(geom: &Geometry, grid: &DotGrid) -> Vec<PackedCell> {
    let mut packed = Vec::new();
    for row in 0..geom.rect.height {
        for col in 0..geom.rect.width {
            let dot_class = grid.class[usize::from(row) * grid.w + usize::from(col)];
            let Some(ch) = ascii_char(dot_class) else {
                continue;
            };
            packed.push(PackedCell {
                col: geom.rect.x + col,
                row: geom.rect.y + row,
                ch,
                class: dot_class,
            });
        }
    }
    packed
}

#[inline]
fn ink_priority(class: DotClass) -> u8 {
    match class {
        DotClass::Grid => 1,
        DotClass::Limb => 2,
        DotClass::Land => 3,
        DotClass::Coast => 4,
        DotClass::VisitedLand => 5,
        DotClass::ActiveLand => 6,
        DotClass::Outside | DotClass::Ocean => 0,
    }
}

#[inline]
fn ascii_char(class: DotClass) -> Option<char> {
    match class {
        DotClass::Land => Some('#'),
        DotClass::Coast => Some('+'),
        DotClass::ActiveLand => Some('@'),
        DotClass::VisitedLand => Some('*'),
        DotClass::Grid => Some('.'),
        DotClass::Limb => Some('-'),
        DotClass::Outside | DotClass::Ocean => None,
    }
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct MarkerCell {
    pub col: u16,
    pub row: u16,
    pub kind: MarkerKind,
    pub count: u16,
    pub index: usize,
    pub depth: f32,
}

/// One aggregated entry per occupied cell, in first-seen order. A rect-sized slot table
/// keeps this linear in the marker count: the catalog can hold thousands of stations and this
/// runs on every drawn frame.
pub fn project_markers(geom: &Geometry, cam: &Camera, markers: &[Marker]) -> Vec<MarkerCell> {
    let frame = cam.frame();
    let mut cells = Vec::new();
    let width = usize::from(geom.rect.width);
    let mut slots: Vec<u32> = vec![u32::MAX; width * usize::from(geom.rect.height)];
    for marker in markers {
        let (nx, ny, depth) = project_with_frame(marker.unit, frame);
        if !depth.is_finite() || depth < 0.0 {
            continue;
        }
        let Some((col, row)) = geom.disc_to_cell(nx, ny) else {
            continue;
        };
        let slot = usize::from(row - geom.rect.y) * width + usize::from(col - geom.rect.x);
        if slots[slot] != u32::MAX {
            let cell: &mut MarkerCell = &mut cells[slots[slot] as usize];
            cell.count = cell.count.saturating_add(1);
            if marker.kind > cell.kind || (marker.kind == cell.kind && depth > cell.depth) {
                cell.kind = marker.kind;
                cell.index = marker.index;
                cell.depth = depth;
            }
        } else {
            slots[slot] = cells.len() as u32;
            cells.push(MarkerCell {
                col,
                row,
                kind: marker.kind,
                count: 1,
                index: marker.index,
                depth,
            });
        }
    }
    cells
}

pub fn nearest_marker(
    cells: &[MarkerCell],
    col: u16,
    row: u16,
    max_dist_cells: f32,
) -> Option<&MarkerCell> {
    if !max_dist_cells.is_finite() || max_dist_cells < 0.0 {
        return None;
    }
    let limit_squared = max_dist_cells * max_dist_cells;
    let mut nearest = None;
    let mut nearest_squared = limit_squared;
    for cell in cells {
        let distance_squared = marker_distance_squared(cell, col, row);
        if distance_squared <= nearest_squared
            && (nearest.is_none() || distance_squared < nearest_squared)
        {
            nearest = Some(cell);
            nearest_squared = distance_squared;
        }
    }
    nearest
}

pub fn markers_by_distance(cells: &[MarkerCell], col: u16, row: u16) -> Vec<usize> {
    let mut indices = (0..cells.len()).collect::<Vec<_>>();
    indices.sort_by(|left, right| {
        marker_distance_squared(&cells[*left], col, row)
            .total_cmp(&marker_distance_squared(&cells[*right], col, row))
            .then_with(|| left.cmp(right))
    });
    indices
}

#[inline]
fn marker_distance_squared(cell: &MarkerCell, col: u16, row: u16) -> f32 {
    let dx = f32::from(cell.col) - f32::from(col);
    let dy = (f32::from(cell.row) - f32::from(row)) * CELL_H_OVER_W;
    dx.mul_add(dx, dy * dy)
}

#[cfg(test)]
mod tests {
    use std::time::Instant;

    use super::*;

    fn rect(width: u16, height: u16) -> CellRect {
        CellRect {
            x: 0,
            y: 0,
            width,
            height,
        }
    }

    fn camera_at_centre() -> Camera {
        Camera {
            centre: LatLon::new(0.0, 0.0),
            scale: 1.0,
        }
    }

    #[test]
    fn braille_bit_mapping_is_unicode_correct() {
        let geom = Geometry::new(rect(1, 1), Renderer::Braille, 1.0);
        let mut grid = DotGrid {
            w: 2,
            h: 4,
            class: vec![DotClass::Outside; 8],
        };
        grid.class[0] = DotClass::Land;
        assert_eq!(pack(&geom, &grid, Renderer::Braille)[0].ch, '\u{2801}');

        grid.class.fill(DotClass::Outside);
        grid.class[7] = DotClass::ActiveLand;
        assert_eq!(pack(&geom, &grid, Renderer::Braille)[0].ch, '\u{2880}');

        grid.class.fill(DotClass::Land);
        assert_eq!(pack(&geom, &grid, Renderer::Braille)[0].ch, '\u{28ff}');
    }

    #[test]
    fn ascii_mapping_uses_only_ink_classes() {
        let geom = Geometry::new(rect(5, 1), Renderer::Ascii, 1.0);
        let grid = DotGrid {
            w: 5,
            h: 1,
            class: vec![
                DotClass::Land,
                DotClass::Coast,
                DotClass::ActiveLand,
                DotClass::Grid,
                DotClass::Limb,
            ],
        };
        let packed = pack(&geom, &grid, Renderer::Ascii);
        assert_eq!(
            packed.iter().map(|cell| cell.ch).collect::<Vec<_>>(),
            vec!['#', '+', '@', '.', '-']
        );
    }

    #[test]
    fn hemisphere_raster_has_land_ocean_and_a_coast_band() {
        let geom = Geometry::new(rect(40, 20), Renderer::Braille, 1.0);
        let land = |at: LatLon| at.lat > 0.0;
        let params = RasterParams {
            visited: None,
            land: &land,
            active: None,
            grid: false,
        };
        let grid = rasterize(&geom, &camera_at_centre(), &params);
        assert_eq!(grid.w, geom.w);
        assert_eq!(grid.h, geom.h);
        assert_eq!(grid.class.len(), geom.w * geom.h);
        assert_eq!(grid.class[0], DotClass::Outside);

        let midpoint = grid.h / 2;
        let top_inside = grid.class[..midpoint * grid.w]
            .iter()
            .filter(|class| !matches!(class, DotClass::Outside | DotClass::Limb))
            .count();
        let top_land = grid.class[..midpoint * grid.w]
            .iter()
            .filter(|class| matches!(class, DotClass::Land | DotClass::Coast))
            .count();
        let bottom_ocean = grid.class[midpoint * grid.w..]
            .iter()
            .filter(|class| **class == DotClass::Ocean)
            .count();
        assert!(top_land * 10 > top_inside * 8);
        assert!(bottom_ocean * 10 > top_inside * 8);
        assert!(
            grid.class[(midpoint - 2) * grid.w..(midpoint + 2) * grid.w].contains(&DotClass::Coast)
        );
    }

    #[test]
    fn high_zoom_covers_the_whole_grid_and_graticule_is_optional() {
        let ocean = |_at: LatLon| false;
        let camera = camera_at_centre();
        let normal = Geometry::new(rect(40, 20), Renderer::Braille, 1.0);
        let enabled = RasterParams {
            visited: None,
            land: &ocean,
            active: None,
            grid: true,
        };
        let disabled = RasterParams {
            visited: None,
            land: &ocean,
            active: None,
            grid: false,
        };
        let with_grid = rasterize(&normal, &camera, &enabled);
        let without_grid = rasterize(&normal, &camera, &disabled);
        let centre = (normal.h / 2) * normal.w + normal.w / 2;
        assert_eq!(with_grid.class[centre], DotClass::Grid);
        assert_eq!(without_grid.class[centre], DotClass::Ocean);

        let zoomed = Geometry::new(rect(40, 20), Renderer::Braille, 12.0);
        let high_zoom = rasterize(&zoomed, &camera, &disabled);
        assert!(
            high_zoom
                .class
                .iter()
                .all(|class| *class != DotClass::Outside)
        );
    }

    #[test]
    fn cell_and_disc_coordinates_round_trip() {
        let geom = Geometry::new(
            CellRect {
                x: 7,
                y: 11,
                width: 13,
                height: 9,
            },
            Renderer::Braille,
            1.0,
        );
        for row in geom.rect.y..geom.rect.y + geom.rect.height {
            for col in geom.rect.x..geom.rect.x + geom.rect.width {
                let (nx, ny) = geom.cell_to_disc(col, row);
                assert_eq!(geom.disc_to_cell(nx, ny), Some((col, row)));
            }
        }
    }

    #[test]
    fn projected_markers_aggregate_and_hit_test_with_cell_aspect() {
        let geom = Geometry::new(
            CellRect {
                x: 5,
                y: 7,
                width: 20,
                height: 10,
            },
            Renderer::Braille,
            1.0,
        );
        let camera = camera_at_centre();
        let centre = Marker {
            index: 4,
            unit: LatLon::new(0.0, 0.0).to_unit(),
            kind: MarkerKind::Normal,
        };
        let far_side = Marker {
            index: 5,
            unit: LatLon::new(0.0, 180.0).to_unit(),
            kind: MarkerKind::Selected,
        };
        let centre_cells = project_markers(&geom, &camera, &[centre, far_side]);
        assert_eq!(centre_cells.len(), 1);
        assert_eq!(centre_cells[0].col, geom.rect.x + geom.rect.width / 2);
        assert_eq!(centre_cells[0].row, geom.rect.y + geom.rect.height / 2);
        assert!((centre_cells[0].depth - 1.0).abs() < 1e-6);

        let selected = Marker {
            index: 6,
            kind: MarkerKind::Selected,
            ..centre
        };
        let aggregate = project_markers(&geom, &camera, &[centre, selected]);
        assert_eq!(aggregate[0].count, 2);
        assert_eq!(aggregate[0].kind, MarkerKind::Selected);
        assert_eq!(aggregate[0].index, 6);

        let cells = [
            MarkerCell {
                col: 0,
                row: 1,
                kind: MarkerKind::Normal,
                count: 1,
                index: 10,
                depth: 1.0,
            },
            MarkerCell {
                col: 1,
                row: 0,
                kind: MarkerKind::Normal,
                count: 1,
                index: 11,
                depth: 1.0,
            },
        ];
        assert_eq!(
            nearest_marker(&cells, 0, 0, 2.0).map(|cell| cell.index),
            Some(11)
        );
        assert_eq!(markers_by_distance(&cells, 0, 0), vec![1, 0]);
    }

    #[test]
    #[ignore]
    fn atlas_perf_budget() {
        let land = |at: LatLon| at.lat > -10.0 && at.lon.is_finite();
        let params = RasterParams {
            visited: None,
            land: &land,
            active: None,
            grid: false,
        };
        let camera = camera_at_centre();
        let scale_one = Geometry::new(rect(200, 50), Renderer::Braille, 1.0);
        let scale_twelve = Geometry::new(rect(200, 50), Renderer::Braille, 12.0);
        let one_ms = median_raster_ms(&scale_one, &camera, &params);
        let twelve_ms = median_raster_ms(&scale_twelve, &camera, &params);
        println!("Atlas raster median: scale 1 {one_ms:.3} ms, scale 12 {twelve_ms:.3} ms");
        assert!(
            one_ms < 1.5,
            "scale 1 median exceeded budget: {one_ms:.3} ms"
        );
        assert!(
            twelve_ms < 4.0,
            "scale 12 median exceeded budget: {twelve_ms:.3} ms"
        );
    }

    fn median_raster_ms(geom: &Geometry, camera: &Camera, params: &RasterParams<'_>) -> f64 {
        let mut measurements = Vec::with_capacity(100);
        for _ in 0..100 {
            let start = Instant::now();
            std::hint::black_box(rasterize(geom, camera, params));
            measurements.push(start.elapsed().as_secs_f64() * 1_000.0);
        }
        measurements.sort_by(f64::total_cmp);
        measurements[measurements.len() / 2]
    }

    #[test]
    fn trig_luts_track_libm_within_a_fiftieth_of_a_degree() {
        let luts = trig_luts();
        let mut worst_atan = 0.0_f32;
        for step in 0..720 {
            let angle = step as f32 * std::f32::consts::PI / 360.0 - std::f32::consts::PI;
            for radius in [0.05_f32, 0.7, 1.0] {
                let (y, x) = (radius * angle.sin(), radius * angle.cos());
                let diff = (fast_atan2(y, x, luts) - y.atan2(x)).abs();
                let diff = diff.min((diff - 2.0 * std::f32::consts::PI).abs());
                worst_atan = worst_atan.max(diff);
            }
        }
        assert!(
            worst_atan.to_degrees() < 0.02,
            "atan2 worst error {}°",
            worst_atan.to_degrees()
        );
        let mut worst_asin = 0.0_f32;
        let h = 2.0 / ASIN_SEGMENTS as f32;
        let mut v = -1.0 + 2.0 * h;
        while v <= 1.0 - 2.0 * h {
            worst_asin = worst_asin.max((fast_asin(v, luts) - v.asin()).abs());
            v += 0.001;
        }
        assert!(
            worst_asin.to_degrees() < 0.02,
            "asin worst error {}°",
            worst_asin.to_degrees()
        );
    }
}
