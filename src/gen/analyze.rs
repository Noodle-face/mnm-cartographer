//! Raster -> land mask, filled heights, elevation bands and wall lines.

use super::grid::*;
use rayon::prelude::*;

/// Drop speckle and pinholes but leave real geometry alone.
pub fn tidy(mask: &Mask, min_area: usize, close_r: usize, open_r: usize, max_hole: usize) -> Mask {
    if !mask.any() {
        return mask.clone();
    }
    let t = super::Timer::start("    tidy: morph");
    let m = closing(mask, &disk(close_r));
    let mut m = opening(&m, &disk(open_r));
    drop(t);
    let _t = super::Timer::start("    tidy: label+holes");

    let (lab, n) = label(&m);
    if n > 0 {
        let sz = component_sizes(&lab, n);
        for (i, l) in lab.iter().enumerate() {
            if *l != 0 && sz[*l as usize] < min_area {
                m.v[i] = false;
            }
        }
    }
    if max_hole > 0 {
        // Fill only *small* holes, so courtyards and lakes survive.
        let inv = m.not();
        let (hl, hn) = label(&inv);
        if hn > 0 {
            let hs = component_sizes(&hl, hn);
            let mut touches_border = vec![false; hn + 1];
            for x in 0..m.w {
                touches_border[hl[x] as usize] = true;
                touches_border[hl[(m.h - 1) * m.w + x] as usize] = true;
            }
            for y in 0..m.h {
                touches_border[hl[y * m.w] as usize] = true;
                touches_border[hl[y * m.w + m.w - 1] as usize] = true;
            }
            for (i, l) in hl.iter().enumerate() {
                let l = *l as usize;
                if l != 0 && !touches_border[l] && hs[l] <= max_hole {
                    m.v[i] = true;
                }
            }
        }
    }
    m
}

pub struct Analysis {
    pub land: Mask,
    /// Height with gaps filled from the nearest real sample.
    pub hf: Grid,
    pub walls: Mask,
    /// (band index, mask of everything at or above that band)
    pub layers: Vec<(usize, Mask)>,
}

pub fn analyze(height: &Grid, ppu: f64, edges: &[f32], step_thresh: f32) -> Analysis {
    let _t = super::Timer::start("analyze (total)");
    let solid = height.finite();
    let min_area = ((10.0 * ppu * ppu) as usize).max(40);
    let land = { let _t = super::Timer::start("  tidy(land)");
        tidy(&solid, min_area, 2, 1, (18.0 * ppu * ppu) as usize) };

    // Fill the gaps from the nearest real sample so filters see no holes.
    let (_, idx) = { let _t = super::Timer::start("  edt(fill)"); edt(&solid) };
    let mut filled = Grid::new(height.w, height.h, 0.0);
    for i in 0..filled.v.len() {
        let src = idx[i];
        filled.v[i] = if src == u32::MAX { 0.0 } else { height.v[src as usize] };
    }
    let hf = { let _t = super::Timer::start("  median3"); median3(&filled) };

    let nb = edges.len().saturating_sub(1).max(1);
    // np.digitize against the interior edges.
    let interior = &edges[1..edges.len().saturating_sub(1)];
    let band: Vec<usize> = hf
        .v
        .iter()
        .map(|t| interior.partition_point(|e| *e <= *t).min(nb - 1))
        .collect();

    // The step detector must look across a constant number of WORLD units, or
    // raising ppu shrinks its window and it stops seeing walls at all.
    let w = (((3.0 * ppu / 0.9).round() as usize) | 1).max(3);
    let rough_max = { let _t = super::Timer::start("  max_filter"); max_filter(&hf, w) };
    let rough_min = { let _t = super::Timer::start("  min_filter"); min_filter(&hf, w) };
    // Only used to estimate the local slope, so the box approximation is
    // indistinguishable here and removes a 143-tap kernel at high zoom.
    let smooth = { let _t = super::Timer::start("  gaussian(slope)");
        gaussian_approx(&hf, (2.0 * ppu / 0.9) as f32) };
    let (gz, gx) = gradient(&smooth);

    let mut walls = Mask::new(hf.w, hf.h, false);
    for i in 0..walls.v.len() {
        // Subtract the variation a smooth local slope would explain, so ramps
        // do not register as architecture.
        let slope = (gx.v[i] * gx.v[i] + gz.v[i] * gz.v[i]).sqrt() * (w - 1) as f32;
        walls.v[i] = (rough_max.v[i] - rough_min.v[i] - slope) > step_thresh && land.v[i];
    }
    walls = opening(&walls, &disk(1));
    if w > 5 {
        let _t = super::Timer::start("  skeletonize+morph");
        // The detector flags every pixel within its window of a step, so the
        // raw mask is a band as wide as the window. Reduce it to a centreline
        // and draw that at fixed weight: a wall is a LINE, and its drawn width
        // must not depend on the zoom level.
        walls = { let _t = super::Timer::start("    walls: closing"); closing(&walls, &disk(2)) };
        walls = { let _t = super::Timer::start("    walls: skeleton"); skeletonize(&walls) };
        walls = { let _t = super::Timer::start("    walls: dilate"); dilate(&walls, &disk(1)) };
    }

    // Size alone is the wrong filter. In open terrain thousands of scattered
    // props each sit a little above the ground and each becomes a compact
    // blob. A wall is a LINE, so keep components by ELONGATION, which is
    // scale-invariant, and drop anything too short to be a wall at all.
    let (lab, n) = label(&walls);
    if n > 0 {
        let sz = component_sizes(&lab, n);
        let boxes = component_boxes(&lab, n, walls.w);
        let min_px = ((6.0 * ppu * ppu) as usize).max(6);
        let mut keep = vec![false; n + 1];
        for i in 1..=n {
            let Some((x0, y0, x1, y1)) = boxes[i] else { continue };
            if sz[i] < min_px {
                continue;
            }
            let diag = (x1 - x0).max(y1 - y0);
            if diag < 8 {
                continue;
            }
            let thickness = (sz[i] as f64 / diag as f64).max(0.5);
            if diag as f64 / thickness >= 4.5 {
                keep[i] = true;
            }
        }
        for (i, l) in lab.iter().enumerate() {
            walls.v[i] = keep[*l as usize];
        }
    }

    // One mask per elevation band, and they do not interact: the morphology
    // for each is independent, so they run together.
    let _tl = super::Timer::start("  layers");
    let max_hole = (10.0 * ppu * ppu) as usize;
    let mut layers: Vec<(usize, Mask)> = (0..nb)
        .into_par_iter()
        .filter_map(|k| {
            super::throttle::gate();
            let mut m = Mask::new(hf.w, hf.h, false);
            for i in 0..m.v.len() {
                m.v[i] = band[i] >= k && land.v[i];
            }
            let m = tidy(&m, min_area, 1, 1, max_hole);
            m.any().then_some((k, m))
        })
        .collect();
    layers.sort_by_key(|(k, _)| *k);

    Analysis { land, hf, walls, layers }
}
