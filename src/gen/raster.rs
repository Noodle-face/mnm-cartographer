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
    let (minx, maxx, minz, maxz) = bbox;
    let w = (((maxx - minx) * ppu) as usize) + 1;
    let h = (((maxz - minz) * ppu) as usize) + 1;
    let mut buf = Grid::new(w, h, f32::NEG_INFINITY);

    for t in tris {
        // Cheap reject before any sampling work.
        let (tx0, tx1) = (
            t[0][0].min(t[1][0]).min(t[2][0]) as f64,
            t[0][0].max(t[1][0]).max(t[2][0]) as f64,
        );
        let (tz0, tz1) = (
            t[0][2].min(t[1][2]).min(t[2][2]) as f64,
            t[0][2].max(t[1][2]).max(t[2][2]) as f64,
        );
        if tx1 < minx || tx0 > maxx || tz1 < minz || tz0 > maxz {
            continue;
        }

        let px: [f64; 3] = [
            (t[0][0] as f64 - minx) * ppu,
            (t[1][0] as f64 - minx) * ppu,
            (t[2][0] as f64 - minx) * ppu,
        ];
        let pz: [f64; 3] = [
            (t[0][2] as f64 - minz) * ppu,
            (t[1][2] as f64 - minz) * ppu,
            (t[2][2] as f64 - minz) * ppu,
        ];
        let area = 0.5
            * ((px[1] - px[0]) * (pz[2] - pz[0]) - (px[2] - px[0]) * (pz[1] - pz[0])).abs();
        // ~2 samples per covered pixel is plenty. A fixed higher factor just
        // multiplies the work without resolving anything finer.
        let n = ((area * 2.0) as i64 + 4).clamp(4, 40_000) as usize;

        for k in 0..n {
            // Stratified in u, golden-ratio sequence in v: an even, low
            // discrepancy spread without needing a random source.
            let mut u = (k as f64 + 0.5) / n as f64;
            let mut v = (k as f64 * 0.618_033_988_749_894_9) % 1.0;
            if u + v > 1.0 {
                u = 1.0 - u;
                v = 1.0 - v;
            }
            let x = px[0] + u * (px[1] - px[0]) + v * (px[2] - px[0]);
            let z = pz[0] + u * (pz[1] - pz[0]) + v * (pz[2] - pz[0]);
            if x < 0.0 || z < 0.0 {
                continue;
            }
            let (xi, zi) = (x as usize, z as usize);
            if xi >= w || zi >= h {
                continue;
            }
            let y = t[0][1] as f64
                + u * (t[1][1] - t[0][1]) as f64
                + v * (t[2][1] - t[0][1]) as f64;
            let slot = &mut buf.v[zi * w + xi];
            if (y as f32) > *slot {
                *slot = y as f32;
            }
        }
    }
    for t in &mut buf.v {
        if !t.is_finite() {
            *t = f32::NAN;
        }
    }
    buf
}
