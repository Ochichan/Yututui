use crate::atlas::geometry::{Country, World};
use crate::atlas::{LandLookup, LatLon};

pub const MASK_W: usize = 2048;
pub const MASK_H: usize = 1024;

#[derive(Clone)]
pub struct LandMask {
    bits: Vec<u64>,
}

impl LandMask {
    pub fn build(world: &World) -> Self {
        let mut mask = Self {
            bits: vec![0; MASK_W * MASK_H / 64],
        };
        for country in world.countries() {
            for ring in &country.rings {
                mask.fill_ring(&ring.pts);
            }
        }
        mask
    }

    pub fn build_country(country: &Country) -> Self {
        Self::build_countries(std::iter::once(country))
    }

    pub fn build_countries<'a>(countries: impl IntoIterator<Item = &'a Country>) -> Self {
        let mut mask = Self {
            bits: vec![0; MASK_W * MASK_H / 64],
        };
        for country in countries {
            for ring in &country.rings {
                mask.fill_ring(&ring.pts);
            }
        }
        mask
    }

    /// Scanline even-odd fill in raw (−180, 180] space. An edge that jumps across the
    /// antimeridian (|Δlon| > 180°) is split at ±180 into two in-range edges, so rings the
    /// source stores split at the dateline and Antarctica's full-circle ring both fill
    /// correctly without any unwrapping heuristic.
    fn fill_ring(&mut self, pts: &[LatLon]) {
        if pts.len() < 3 {
            return;
        }
        let mut edges: Vec<(LatLon, LatLon)> = Vec::with_capacity(pts.len() + 4);
        for i in 0..pts.len() {
            let a = pts[i];
            let b = pts[(i + 1) % pts.len()];
            let delta = b.lon - a.lon;
            if delta.abs() <= 180.0 {
                edges.push((a, b));
                continue;
            }
            let shifted = if delta > 0.0 {
                b.lon - 360.0
            } else {
                b.lon + 360.0
            };
            let boundary = if a.lon < 0.0 { -180.0 } else { 180.0 };
            let t = if (shifted - a.lon).abs() < 1e-6 {
                0.0
            } else {
                (boundary - a.lon) / (shifted - a.lon)
            };
            let lat_m = a.lat + t * (b.lat - a.lat);
            edges.push((a, LatLon::new(lat_m, boundary)));
            edges.push((LatLon::new(lat_m, -boundary), b));
        }
        let px = |lon: f32| (lon + 180.0) * MASK_W as f32 / 360.0;
        let py = |lat: f32| (90.0 - lat) * MASK_H as f32 / 180.0;
        let mut min_y = f32::MAX;
        let mut max_y = f32::MIN;
        for &p in pts {
            min_y = min_y.min(py(p.lat));
            max_y = max_y.max(py(p.lat));
        }
        let last_row = (MASK_H - 1) as f32;
        let mut crossings: Vec<f32> = Vec::new();
        for y in min_y.max(0.0) as usize..=max_y.min(last_row) as usize {
            let row = y as f32 + 0.5;
            crossings.clear();
            for &(a, b) in &edges {
                let (y0, y1) = (py(a.lat), py(b.lat));
                if (y0 <= row && y1 > row) || (y1 <= row && y0 > row) {
                    let t = (row - y0) / (y1 - y0);
                    crossings.push(px(a.lon) + t * (px(b.lon) - px(a.lon)));
                }
            }
            crossings.sort_by(f32::total_cmp);
            for pair in crossings.as_chunks::<2>().0 {
                // Pixel centres, matching the row-centre sampling above.
                let x0 = ((pair[0] - 0.5).ceil().max(0.0) as usize).min(MASK_W);
                let x1 = ((pair[1] - 0.5).ceil().max(0.0) as usize).min(MASK_W);
                for x in x0..x1 {
                    self.set(x, y);
                }
            }
        }
    }

    fn set(&mut self, x: usize, y: usize) {
        let idx = y * MASK_W + x;
        self.bits[idx / 64] |= 1 << (idx % 64);
    }

    pub fn get(&self, x: usize, y: usize) -> bool {
        let idx = y * MASK_W + x;
        self.bits[idx / 64] & (1 << (idx % 64)) != 0
    }

    pub fn sample(&self, at: LatLon) -> bool {
        let lon = crate::atlas::wrap_longitude(at.lon);
        let x = ((lon + 180.0) / 360.0 * MASK_W as f32) as usize % MASK_W;
        let y = (((90.0 - at.lat) / 180.0 * MASK_H as f32) as usize).min(MASK_H - 1);
        self.get(x, y)
    }
}

impl LandLookup for LandMask {
    fn is_land(&self, at: LatLon) -> bool {
        self.sample(at)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::atlas::geometry::world;

    fn edge_distance_sq(p: LatLon, world: &World) -> f32 {
        let mut best = f32::MAX;
        for country in world.countries() {
            for ring in &country.rings {
                for i in 0..ring.pts.len() {
                    let a = ring.pts[i];
                    let b = ring.pts[(i + 1) % ring.pts.len()];
                    let ab = (b.lon - a.lon, b.lat - a.lat);
                    let ap = (p.lon - a.lon, p.lat - a.lat);
                    let dot = ap.0 * ab.0 + ap.1 * ab.1;
                    let len2 = ab.0 * ab.0 + ab.1 * ab.1;
                    let t = (dot / len2).clamp(0.0, 1.0);
                    let d0 = p.lon - (a.lon + t * ab.0);
                    let d1 = p.lat - (a.lat + t * ab.1);
                    best = best.min(d0 * d0 + d1 * d1);
                }
            }
        }
        best
    }

    #[test]
    fn mask_matches_polygons() {
        let world = world();
        let mask = LandMask::build(world);
        let mut seed = 123456789u32;
        let mut mismatches = 0;
        let mut outside_band = 0;
        for _ in 0..1000 {
            seed = seed.wrapping_mul(1664525).wrapping_add(1013904223);
            let lat = (seed >> 16) as f32 / 65536.0 * 180.0 - 90.0;
            seed = seed.wrapping_mul(1664525).wrapping_add(1013904223);
            let lon = (seed >> 16) as f32 / 65536.0 * 360.0 - 180.0;
            let p = LatLon::new(lat, lon);
            let land = mask.sample(p);
            let truth = world.country_at(p).is_some();
            if land != truth {
                mismatches += 1;
                if edge_distance_sq(p, world) > 0.3 * 0.3 {
                    outside_band += 1;
                }
            }
        }
        assert_eq!(outside_band, 0);
        assert!(
            mismatches < 20,
            "mismatch ratio too high: {mismatches}/1000"
        );
    }

    #[test]
    fn antimeridian_and_ocean() {
        let mask = LandMask::build(world());
        assert!(mask.sample(LatLon::new(-16.3, 179.95)));
        assert!(mask.sample(LatLon::new(-16.3, -179.95)));
        assert!(!mask.sample(LatLon::new(89.9, 0.0)));
    }

    #[test]
    fn country_mask() {
        let world = world();
        let kr = LandMask::build_country(world.by_code("KR").expect("KR"));
        assert!(kr.sample(LatLon::new(37.5, 127.0)));
        assert!(!kr.sample(LatLon::new(35.68, 139.69)));
    }
}
