//! Triangles -> a top-down height raster.
//!
//! Each triangle is sampled at roughly two points per covered pixel and the
//! greatest height wins, which is what makes the result a *top-down* view:
//! where a walkway crosses a floor, the walkway is what you see.

use super::extract::Tri;
use super::grid::{quantile, sorted_copy, Grid};

/// (min_x, max_x, min_z, max_z) in world units.
pub type Bbox = (f64, f64, f64, f64);

/// Bounds that ignore the long tail of stray geometry, as the Python used
/// percentiles rather than the raw extremes.
pub fn auto_bbox(tris: &[Tri]) -> Bbox {
    let mut xs = Vec::with_capacity(tris.len() * 3);
    let mut zs = Vec::with_capacity(tris.len() * 3);
    for t in tris {
        for v in t {
            xs.push(v[0]);
            zs.push(v[2]);
        }
    }
    let xs = sorted_copy(&xs);
    let zs = sorted_copy(&zs);
    (
        quantile(&xs, 0.0005) as f64,
        quantile(&xs, 0.9995) as f64,
        quantile(&zs, 0.0005) as f64,
        quantile(&zs, 0.9995) as f64,
    )
}

/// Area-weighted height quantiles over the whole zone.
///
/// Computed from the geometry rather than from a raster so every zoom level
/// and every tile gets *identical* band breaks. Per-level quantiles make the
/// terrain change colour as you zoom, which looks like a bug and is one.
pub fn global_band_edges(tris: &[Tri], bands: usize) -> Vec<f32> {
    if tris.is_empty() {
        return vec![0.0, 1.0];
    }
    let mut pairs: Vec<(f32, f32)> = tris
        .iter()
        .map(|t| {
            let y = (t[0][1] + t[1][1] + t[2][1]) / 3.0;
            let u = [t[1][0] - t[0][0], t[1][1] - t[0][1], t[1][2] - t[0][2]];
            let v = [t[2][0] - t[0][0], t[2][1] - t[0][1], t[2][2] - t[0][2]];
            let n = [
                u[1] * v[2] - u[2] * v[1],
                u[2] * v[0] - u[0] * v[2],
                u[0] * v[1] - u[1] * v[0],
            ];
            let area = 0.5 * (n[0] * n[0] + n[1] * n[1] + n[2] * n[2]).sqrt();
            (y, area)
        })
        .collect();
    pairs.sort_by(|a, b| a.0.partial_cmp(&b.0).unwrap_or(std::cmp::Ordering::Equal));

    let total: f64 = pairs.iter().map(|p| p.1 as f64).sum();
    if total <= 0.0 {
        return vec![pairs[0].0, pairs[pairs.len() - 1].0 + 1.0];
    }
    // Cumulative area fraction, then read the height off at even steps.
    let mut cum = Vec::with_capacity(pairs.len());
    let mut acc = 0.0f64;
    for (_, a) in &pairs {
        acc += *a as f64;
        cum.push(acc / total);
    }
    let mut edges = Vec::with_capacity(bands + 1);
    for i in 0..=bands {
        let target = i as f64 / bands as f64;
        let j = cum.partition_point(|c| *c < target).min(pairs.len() - 1);
        edges.push(pairs[j].0);
    }
    edges.dedup();
    if edges.len() < 2 {
        let lo = pairs[0].0;
        edges = vec![lo, lo + 1.0];
    }
    edges
}

/// Rasterise to a height grid. Returns the grid (NaN where nothing was hit)
/// and the bbox actually used.
///
/// Restricting `bbox` both frames the view and drops triangles before
/// sampling, which is what makes a high pixels-per-unit affordable and is how
/// tiles are cut.
pub fn rasterize(tris: &[Tri], ppu: f64, bbox: Bbox) -> Grid {
    let (w, h) = raster_size(ppu, bbox);
    let mut buf = Grid::new(w, h, f32::NEG_INFINITY);
    for_each_sample(tris, ppu, bbox, |i, y| {
        if y > buf.v[i] { buf.v[i] = y }
    });
    finish(buf)
}

/// A floor plan of a slab of a multi-storey zone: the LOWEST surface in each
/// pixel, so a room shows rather than the roof over it -- unless something
/// lies more than `over` above that surface, which is a bridge or walkway
/// across open space and is drawn instead.
///
/// Roofs sit a room's height over their floor (4-14 units in Blind Midden)
/// while bridges cross halls (~30), so one threshold tells them apart.
/// Samples within `same` of the lowest are the same surface, bumps and all.
pub fn rasterize_floor(tris: &[Tri], ppu: f64, bbox: Bbox, same: f32, over: f32) -> Grid {
    let (w, h) = raster_size(ppu, bbox);
    let mut low = vec![f32::INFINITY; w * h];
    for_each_sample(tris, ppu, bbox, |i, y| {
        if y < low[i] { low[i] = y }
    });
    let mut floor = Grid::new(w, h, f32::NEG_INFINITY);
    let mut high = vec![f32::NEG_INFINITY; w * h];
    for_each_sample(tris, ppu, bbox, |i, y| {
        if y <= low[i] + same {
            if y > floor.v[i] { floor.v[i] = y }
        } else if y >= low[i] + over && y > high[i] {
            high[i] = y;
        }
    });
    for (f, hi) in floor.v.iter_mut().zip(high) {
        if hi.is_finite() { *f = hi }
    }
    finish(floor)
}

pub(crate) fn raster_size(ppu: f64, bbox: Bbox) -> (usize, usize) {
    let (minx, maxx, minz, maxz) = bbox;
    ((((maxx - minx) * ppu) as usize) + 1, (((maxz - minz) * ppu) as usize) + 1)
}

fn finish(mut buf: Grid) -> Grid {
    for t in &mut buf.v {
        if !t.is_finite() {
            *t = f32::NAN;
        }
    }
    buf
}

/// Visit every pixel each triangle covers, as (pixel index, height there).
///
/// Exact coverage: every pixel whose centre lies inside the triangle, with
/// the height interpolated at that centre. This replaced random-ish point
/// sampling, which capped big triangles at 40,000 samples and so left holes
/// in them at fine zoom -- the surface below showed through the holes as a
/// regular dot pattern over whole regions. A triangle too small to cover any
/// pixel centre still marks the pixel holding its centroid, so thin
/// geometry (a plank, a ledge) is not lost.
pub(crate) fn for_each_sample(tris: &[Tri], ppu: f64, bbox: Bbox, mut f: impl FnMut(usize, f32)) {
    let (minx, maxx, minz, maxz) = bbox;
    let (w, h) = raster_size(ppu, bbox);
    let (wf, hf) = (w as f64, h as f64);

    for t in tris {
        // Pixel space: x right, z "up" in rows (row 0 = south).
        let p: [(f64, f64, f64); 3] = [0, 1, 2].map(|k| (
            (t[k][0] as f64 - minx) * ppu,
            (t[k][2] as f64 - minz) * ppu,
            t[k][1] as f64,
        ));
        let (x0, x1) = (p[0].0.min(p[1].0).min(p[2].0), p[0].0.max(p[1].0).max(p[2].0));
        let (z0, z1) = (p[0].1.min(p[1].1).min(p[2].1), p[0].1.max(p[1].1).max(p[2].1));
        if x1 < 0.0 || z1 < 0.0 || x0 >= wf || z0 >= hf { continue }
        let _ = (maxx, maxz);

        let area = (p[1].0 - p[0].0) * (p[2].1 - p[0].1) - (p[2].0 - p[0].0) * (p[1].1 - p[0].1);
        let mut hit = false;
        if area.abs() > 1e-12 {
            let inv = 1.0 / area;
            let (cx0, cx1) = (((x0 - 0.5).ceil().max(0.0)) as usize, ((x1 - 0.5).floor().min(wf - 1.0)) as i64);
            let (cz0, cz1) = (((z0 - 0.5).ceil().max(0.0)) as usize, ((z1 - 0.5).floor().min(hf - 1.0)) as i64);
            for cz in cz0 as i64..=cz1 {
                let pz = cz as f64 + 0.5;
                for cx in cx0 as i64..=cx1 {
                    let px = cx as f64 + 0.5;
                    let l1 = ((px - p[0].0) * (p[2].1 - p[0].1) - (p[2].0 - p[0].0) * (pz - p[0].1)) * inv;
                    let l2 = ((p[1].0 - p[0].0) * (pz - p[0].1) - (px - p[0].0) * (p[1].1 - p[0].1)) * inv;
                    let l0 = 1.0 - l1 - l2;
                    // A hair of tolerance so shared edges leave no seam.
                    if l0 < -1e-9 || l1 < -1e-9 || l2 < -1e-9 { continue }
                    let y = p[0].2 * l0 + p[1].2 * l1 + p[2].2 * l2;
                    f(cz as usize * w + cx as usize, y as f32);
                    hit = true;
                }
            }
        }
        if !hit {
            let (cx, cz) = ((p[0].0 + p[1].0 + p[2].0) / 3.0, (p[0].1 + p[1].1 + p[2].1) / 3.0);
            if cx >= 0.0 && cz >= 0.0 && cx < wf && cz < hf {
                f(cz as usize * w + cx as usize, ((p[0].2 + p[1].2 + p[2].2) / 3.0) as f32);
            }
        }
    }
}
