//! The part of a zone a player can actually get to.
//!
//! A zone's scenes carry more than the zone: backdrop mountains drawn to be
//! seen from afar, ocean floor to the horizon, and sometimes whole stretches
//! of a neighbouring zone. A map should be the zone and only a thin margin
//! of what lies beyond it.
//!
//! Two ways to find it, best first:
//!
//! 1. **The zone's own boundary.** Many zones mark their edge with invisible
//!    walls and blocking volumes (see [`super::extract::is_boundary_name`]).
//!    Flooding out from the zone's content until those walls stop it gives
//!    the playable area exactly -- if the walls close. Shaded Dunes' east
//!    wall, for one, cuts off a long strip of backdrop terrain that is
//!    otherwise indistinguishable from the zone.
//! 2. **Where you can walk.** Ground joined by slopes a player can climb,
//!    grown from the ground the zone's objects stand on. Backdrop mountains
//!    are too steep to join; ground below sea level is open water.
//!
//! Either way the result is a region, not a rectangle: geometry outside it
//! is dropped, so the map shows nothing of a neighbour even in its corners.
//!
//! Only for large zones. Inside a dungeon the coarse grid cannot follow
//! stairs, and dungeons do not have the problem anyway.

use super::extract::{Landmarks, Tri};
use super::raster::{self, Bbox};

/// Zones narrower than this keep everything.
const MIN_SPAN: f64 = 1500.0;
/// Steepest climb between grid cells, as rise over run: about 50 degrees.
const SLOPE: f64 = 1.2;
/// A connected stretch of ground counts if this share of the grid cells
/// holding the zone's objects lie on it (and at least a few). Cells, not
/// objects: one pile of props can hold thousands of renderers and colliders,
/// and must not outvote ground whose content is spread thinly.
const MIN_SHARE: f64 = 0.05;
/// Margin kept around the play area, as a share of its longer side: enough
/// that a coast or cliff edge does not stop dead, little enough that the map
/// stays about 98% the zone.
const MARGIN: f64 = 0.02;

/// The play area of a zone, as cells of a grid over the world.
pub struct Region {
    x0: f64,
    z0: f64,
    cell: f64,
    w: usize,
    h: usize,
    inside: Vec<bool>,
    /// The play area itself, before the margin was added.
    core: Vec<bool>,
    /// How it was found: "boundary" or "walkable".
    pub method: &'static str,
}

impl Region {
    fn idx(&self, x: f64, z: f64) -> Option<usize> {
        let (fx, fz) = ((x - self.x0) / self.cell, (z - self.z0) / self.cell);
        if fx < 0.0 || fz < 0.0 { return None }
        let (cx, cz) = (fx as usize, fz as usize);
        (cx < self.w && cz < self.h).then(|| cz * self.w + cx)
    }
    /// The world box around the region.
    pub fn bbox(&self) -> Bbox {
        let (mut x0, mut x1, mut z0, mut z1) = (usize::MAX, 0, usize::MAX, 0);
        for i in 0..self.inside.len() {
            if !self.inside[i] { continue }
            let (x, z) = (i % self.w, i / self.w);
            x0 = x0.min(x); x1 = x1.max(x); z0 = z0.min(z); z1 = z1.max(z);
        }
        (self.x0 + x0 as f64 * self.cell, self.x0 + (x1 + 1) as f64 * self.cell,
         self.z0 + z0 as f64 * self.cell, self.z0 + (z1 + 1) as f64 * self.cell)
    }
    /// Grow the region by `r` cells in every direction.
    ///
    /// By true distance, so the margin is rounded. Grown a ring of
    /// four-neighbours at a time it was a diamond, whose sides are long
    /// straight 45-degree lines -- and everything beyond them is dropped, so
    /// they cut lakes and streets off along a ruler.
    fn dilate(&mut self, r: usize) {
        let m = super::grid::Mask { w: self.w, h: self.h, v: self.inside.clone() };
        let d = super::grid::edt(&m).0;
        for (i, &t) in d.v.iter().enumerate() {
            if t <= r as f32 { self.inside[i] = true }
        }
    }
}

/// The play area of a zone with a small margin around it, or None to keep
/// everything (a small zone, or no confident answer).
pub fn play_region(tris: &[Tri], walls: &[Tri], blocks: &[[f64; 6]], lm: &Landmarks,
                   sea: Option<f32>, full: Bbox) -> Option<Region> {
    let span = (full.1 - full.0).max(full.3 - full.2);
    if span < MIN_SPAN || tris.is_empty() { return None }
    let mut r = enclosed(tris, walls, blocks, lm, sea, full)
        .or_else(|| walkable(tris, walls, blocks, lm, sea, full))?;
    // Level designers draw OcclusionAreas around where the camera can be.
    // Where they narrow the region, the part outside them is backdrop.
    if !lm.occlusion.is_empty() {
        let o = lm.occlusion.iter().fold(
            (f64::MAX, f64::MIN, f64::MAX, f64::MIN),
            |a, b| (a.0.min(b[0]), a.1.max(b[1]), a.2.min(b[2]), a.3.max(b[3])));
        let b = r.bbox();
        let c = (b.0.max(o.0), b.1.min(o.1), b.2.max(o.2), b.3.min(o.3));
        // Some zones carry a stock box far larger than the zone, or one that
        // misses it; use it only when it actually narrows things.
        if c.1 - c.0 > 50.0 && c.3 - c.2 > 50.0 && (c.1 - c.0) * (c.3 - c.2) < 0.97 * (b.1 - b.0) * (b.3 - b.2) {
            if std::env::var("MNM_BOUNDS_DEBUG").is_ok() {
                eprintln!("bounds: occlusion areas clip the region from {b:?} to {c:?}");
            }
            for i in 0..r.inside.len() {
                let (x, z) = (r.x0 + ((i % r.w) as f64 + 0.5) * r.cell, r.z0 + ((i / r.w) as f64 + 0.5) * r.cell);
                if x < c.0 || x > c.1 || z < c.2 || z > c.3 { r.inside[i] = false }
            }
        }
    }
    r.core = r.inside.clone();
    let b = r.bbox();
    let grow = (MARGIN * (b.1 - b.0).max(b.3 - b.2) / r.cell).ceil() as usize + 1;
    r.dilate(grow);
    Some(r)
}

/// Height range of the walkable surfaces in each cell of a grid.
fn ground(tris: &[Tri], ppu: f64, bbox: Bbox) -> (Vec<f32>, Vec<f32>) {
    let (w, h) = raster::raster_size(ppu, bbox);
    let mut lo = vec![f32::INFINITY; w * h];
    let mut hi = vec![f32::NEG_INFINITY; w * h];
    raster::for_each_sample(tris, ppu, bbox, |i, y| {
        if y < lo[i] { lo[i] = y }
        if y > hi[i] { hi[i] = y }
    });
    (lo, hi)
}

/// Grid cells holding the zone's objects.
fn object_cells(lm: &Landmarks, x0: f64, z0: f64, cell: f64, w: usize, h: usize) -> Vec<usize> {
    let mut v: Vec<usize> = lm.objects.iter().filter_map(|p| {
        let (fx, fz) = ((p[0] - x0) / cell, (p[1] - z0) / cell);
        if fx < 0.0 || fz < 0.0 { return None }
        let (cx, cz) = (fx as usize, fz as usize);
        (cx < w && cz < h).then(|| cz * w + cx)
    }).collect();
    v.sort_unstable();
    v.dedup();
    v
}

/// Grid cells an invisible wall or a grounded blocking volume crosses,
/// with single-cell gaps between wall pieces closed.
fn barriers(walls: &[Tri], blocks: &[[f64; 6]], lo: &[f32], hi: &[f32], sea: Option<f32>,
            bb: Bbox, ppu: f64, w: usize, h: usize) -> Vec<bool> {
    let cell = 1.0 / ppu;
    let mut blocked = vec![false; w * h];
    let at = |x: f64, z: f64| -> Option<usize> {
        let (fx, fz) = ((x - bb.0) * ppu, (z - bb.2) * ppu);
        if fx < 0.0 || fz < 0.0 { return None }
        let (cx, cz) = (fx as usize, fz as usize);
        (cx < w && cz < h).then(|| cz * w + cx)
    };
    // Walls: trace each face's edges across the grid.
    for t in walls {
        for (a, b) in [(t[0], t[1]), (t[1], t[2]), (t[2], t[0])] {
            let len = ((b[0] - a[0]).hypot(b[2] - a[2])) as f64;
            let n = (len / (cell * 0.5)).ceil() as usize + 1;
            for k in 0..=n {
                let f = k as f32 / n as f32;
                if let Some(i) = at((a[0] + (b[0] - a[0]) * f) as f64, (a[2] + (b[2] - a[2]) * f) as f64) {
                    blocked[i] = true;
                }
            }
        }
    }
    // Blocking volumes count where they reach down to the ground there: a
    // box hung in the sky over a valley does not wall the valley off.
    for b in blocks {
        if b[1] < bb.0 || b[0] > bb.1 || b[5] < bb.2 || b[4] > bb.3 { continue }
        let (cx0, cz0) = (((b[0] - bb.0) * ppu).max(0.0) as usize, ((b[4] - bb.2) * ppu).max(0.0) as usize);
        let (cx1, cz1) = ((((b[1] - bb.0) * ppu) as usize).min(w - 1), (((b[5] - bb.2) * ppu) as usize).min(h - 1));
        for cz in cz0..=cz1 {
            for cx in cx0..=cx1 {
                let i = cz * w + cx;
                let floor = if hi[i].is_finite() { lo[i] as f64 } else { sea.unwrap_or(f32::MIN) as f64 };
                let top = if hi[i].is_finite() { hi[i] as f64 } else { floor };
                if b[2] <= top + 3.0 && b[3] >= floor - 3.0 { blocked[i] = true }
            }
        }
    }
    let src = blocked.clone();
    for z in 1..h.saturating_sub(1) {
        for x in 1..w.saturating_sub(1) {
            let i = z * w + x;
            if src[i] { continue }
            if (src[i - 1] && src[i + 1]) || (src[i - w] && src[i + w]) { blocked[i] = true }
        }
    }
    blocked
}

/// The area inside the zone's boundary walls, flooded from its content; None
/// if the zone has no walls or they do not close.
fn enclosed(tris: &[Tri], walls: &[Tri], blocks: &[[f64; 6]], lm: &Landmarks,
            sea: Option<f32>, full: Bbox) -> Option<Region> {
    if walls.is_empty() && blocks.is_empty() { return None }
    let span = (full.1 - full.0).max(full.3 - full.2);
    // A border of open cells all round, so a fill that escapes the walls
    // reaches the grid's edge and is seen to have leaked.
    let pad = span * 0.08;
    let bb = (full.0 - pad, full.1 + pad, full.2 - pad, full.3 + pad);
    let cell = (span / 700.0).max(2.0);
    let ppu = 1.0 / cell;
    let (w, h) = raster::raster_size(ppu, bb);
    let (lo, hi) = ground(tris, ppu, bb);
    let blocked = barriers(walls, blocks, &lo, &hi, sea, bb, ppu, w, h);
    // Flood every open area; label each.
    let mut label = vec![u32::MAX; w * h];
    let mut sizes: Vec<usize> = Vec::new();
    let mut leaks: Vec<bool> = Vec::new();
    let mut stack = Vec::new();
    for start in 0..w * h {
        if blocked[start] || label[start] != u32::MAX { continue }
        let id = sizes.len() as u32;
        let (mut size, mut leak) = (0usize, false);
        label[start] = id;
        stack.push(start);
        while let Some(i) = stack.pop() {
            size += 1;
            let (x, z) = (i % w, i / w);
            if x == 0 || z == 0 || x + 1 == w || z + 1 == h { leak = true }
            for j in [(x > 0).then(|| i - 1), (x + 1 < w).then(|| i + 1),
                      (z > 0).then(|| i - w), (z + 1 < h).then(|| i + w)].into_iter().flatten() {
                if !blocked[j] && label[j] == u32::MAX {
                    label[j] = id;
                    stack.push(j);
                }
            }
        }
        sizes.push(size);
        leaks.push(leak);
    }

    // The zone is the sealed area holding the most of its objects. Areas
    // that leak are outside; tiny ones are inside walls or props.
    let seeds = object_cells(lm, bb.0, bb.2, cell, w, h);
    let mut count = vec![0usize; sizes.len()];
    for &i in &seeds {
        if label[i] != u32::MAX { count[label[i] as usize] += 1 }
    }
    let total: usize = count.iter().sum();
    if std::env::var("MNM_BOUNDS_DEBUG").is_ok() {
        let leaked: usize = (0..sizes.len()).filter(|&k| leaks[k]).map(|k| count[k]).sum();
        let mut sealed: Vec<(usize, usize)> = (0..sizes.len()).filter(|&k| !leaks[k] && count[k] > 0)
            .map(|k| (count[k], sizes[k])).collect();
        sealed.sort_unstable_by(|a, b| b.cmp(a));
        eprintln!("bounds: {} walls, {} blocks; object cells {total}: {leaked} in leaking areas, sealed (objs, cells) {:?}",
                  walls.len(), blocks.len(), &sealed[..sealed.len().min(5)]);
    }
    let best = (0..sizes.len()).filter(|&k| !leaks[k]).max_by_key(|&k| count[k])?;
    // Not convincing: the walls enclose only a corner of the zone's content,
    // or the zone's content mostly sits in areas that leak.
    if total == 0 || count[best] * 2 < total { return None }
    let mut inside: Vec<bool> = label.iter().map(|&l| l == best as u32).collect();
    // Wall cells bordering the area are part of it: the edge itself.
    let src = inside.clone();
    for z in 1..h - 1 {
        for x in 1..w - 1 {
            let i = z * w + x;
            if blocked[i] && (src[i - 1] || src[i + 1] || src[i - w] || src[i + w]) { inside[i] = true }
        }
    }
    if std::env::var("MNM_BOUNDS_DEBUG").is_ok() {
        eprintln!("bounds: boundary region {} cells of {}, holding {}/{} object cells",
                  sizes[best], w * h, count[best], total);
    }
    Some(Region { x0: bb.0, z0: bb.2, cell, w, h, core: Vec::new(), inside, method: "boundary" })
}

/// Ground a player can walk to from the zone's content.
fn walkable(tris: &[Tri], walls: &[Tri], blocks: &[[f64; 6]], lm: &Landmarks,
            sea: Option<f32>, full: Bbox) -> Option<Region> {
    let span = (full.1 - full.0).max(full.3 - full.2);
    let cell = (span / 400.0).max(4.0);
    let ppu = 1.0 / cell;
    let (w, h) = raster::raster_size(ppu, full);
    let n = w * h;
    let (lo, hi) = ground(tris, ppu, full);
    // Invisible walls cut the ground: a player cannot walk through them, even
    // where the terrain beyond them is gentle.
    let blocked = barriers(walls, blocks, &lo, &hi, sea, full, ppu, w, h);
    let ground: Vec<bool> = (0..n)
        .map(|i| !blocked[i] && hi[i].is_finite() && sea.map_or(true, |s| hi[i] > s + 0.5))
        .collect();

    // Join neighbouring cells a player can walk between.
    let mut uf = UnionFind::new(n);
    let step = (SLOPE * cell) as f32;
    for z in 0..h {
        for x in 0..w {
            let i = z * w + x;
            if !ground[i] { continue }
            for j in [(x + 1 < w).then(|| i + 1), (z + 1 < h).then(|| i + w)].into_iter().flatten() {
                if ground[j] && lo[j] - step <= hi[i] && lo[i] - step <= hi[j] {
                    uf.union(i, j);
                }
            }
        }
    }
    let occupied: Vec<usize> = object_cells(lm, full.0, full.2, cell, w, h)
        .into_iter().filter(|&i| ground[i]).collect();
    let placed = occupied.len();
    let mut per_root = std::collections::HashMap::<usize, usize>::new();
    for &i in &occupied {
        *per_root.entry(uf.find(i)).or_default() += 1;
    }
    let top = per_root.values().copied().max().unwrap_or(0);
    // A stretch counts if it holds a fair share of the zone's content, and
    // a fair share next to the main stretch: a scrap of backdrop with a few
    // rocks on it is neither.
    let need = ((placed as f64 * MIN_SHARE) as usize).max(top / 5).max(3);
    let good: std::collections::HashSet<usize> =
        per_root.into_iter().filter(|&(_, c)| c >= need).map(|(r, _)| r).collect();
    if good.is_empty() { return None }
    let mut inside: Vec<bool> = (0..n).map(|i| ground[i] && good.contains(&uf.find(i))).collect();
    if inside.iter().filter(|&&b| b).count() < 20 { return None }
    // Water and gaps enclosed by the walkable ground belong to the zone:
    // a lake is part of the map even though no one stands on it.
    fill_holes(&mut inside, w, h);
    if std::env::var("MNM_BOUNDS_DEBUG").is_ok() {
        eprintln!("bounds: walkable region, cell {cell:.1}, {} of {} cells", inside.iter().filter(|&&b| b).count(), n);
    }
    Some(Region { x0: full.0, z0: full.2, cell, w, h, core: Vec::new(), inside, method: "walkable" })
}

/// Mark every cell not reachable from the grid's edge through outside cells.
fn fill_holes(inside: &mut [bool], w: usize, h: usize) {
    let mut outside = vec![false; w * h];
    let mut stack: Vec<usize> = (0..w * h)
        .filter(|&i| { let (x, z) = (i % w, i / w); (x == 0 || z == 0 || x + 1 == w || z + 1 == h) && !inside[i] })
        .collect();
    for &i in &stack { outside[i] = true }
    while let Some(i) = stack.pop() {
        let (x, z) = (i % w, i / w);
        for j in [(x > 0).then(|| i - 1), (x + 1 < w).then(|| i + 1),
                  (z > 0).then(|| i - w), (z + 1 < h).then(|| i + w)].into_iter().flatten() {
            if !inside[j] && !outside[j] { outside[j] = true; stack.push(j) }
        }
    }
    for i in 0..w * h { if !outside[i] { inside[i] = true } }
}

/// The triangles of the play area, and of the margin around it only where
/// they carry on from it. Without that check the margin swept in separate
/// scraps of backdrop that happened to lie near the edge -- a strip of
/// terrain beside Scarwood -- rather than a little more of the zone's own
/// cliffs and shore.
pub fn region_keep(tris: &[Tri], r: &Region) -> Vec<bool> {
    let cell_of = |t: &Tri| r.idx(((t[0][0] + t[1][0] + t[2][0]) / 3.0) as f64,
                                  ((t[0][2] + t[1][2] + t[2][2]) / 3.0) as f64);
    // Cells in the margin that hold geometry.
    let mut occ = vec![false; r.w * r.h];
    for t in tris {
        if let Some(i) = cell_of(t) { if r.inside[i] { occ[i] = true } }
    }
    // Spread from the play area through occupied margin cells.
    let mut keep = r.core.clone();
    let mut stack: Vec<usize> = (0..keep.len()).filter(|&i| keep[i]).collect();
    while let Some(i) = stack.pop() {
        let (x, z) = (i % r.w, i / r.w);
        for dz in -1i64..=1 {
            for dx in -1i64..=1 {
                let (nx, nz) = (x as i64 + dx, z as i64 + dz);
                if nx < 0 || nz < 0 || nx >= r.w as i64 || nz >= r.h as i64 { continue }
                let j = nz as usize * r.w + nx as usize;
                if !keep[j] && occ[j] { keep[j] = true; stack.push(j) }
            }
        }
    }
    tris.iter().map(|t| cell_of(t).is_some_and(|i| keep[i])).collect()
}

/// Drop geometry that sits alone far from the rest of the zone.
///
/// Every crypt carries a four-triangle slab 700 units from its rooms; a
/// trimmed percentile of extents cannot tell that from real edge geometry,
/// but its isolation can. Occupied cells of a coarse grid are joined across
/// small gaps into clusters. The main body is every cluster with a fifth of
/// the cells or a tenth of the cells holding the zone's objects. Another
/// cluster goes only if it lies wholly outside the main body's box grown by
/// 15% and has next to none of the objects. Being small is not enough: a
/// sewer's tunnel sections are each small, and Party City keeps its busiest
/// spot on a small patch of ground well away from the rest.
pub fn strays_keep(tris: &[Tri], objects: &[[f64; 2]]) -> Vec<bool> {
    if tris.len() < 4 { return vec![true; tris.len()] }
    let (mut x0, mut x1, mut z0, mut z1) = (f64::MAX, f64::MIN, f64::MAX, f64::MIN);
    for t in tris {
        for v in t {
            x0 = x0.min(v[0] as f64); x1 = x1.max(v[0] as f64);
            z0 = z0.min(v[2] as f64); z1 = z1.max(v[2] as f64);
        }
    }
    let span = (x1 - x0).max(z1 - z0);
    let cell = (span / 256.0).max(2.0);
    let full = (x0, x1, z0, z1);
    let ppu = 1.0 / cell;
    let (w, h) = raster::raster_size(ppu, full);
    let mut occ = vec![false; w * h];
    raster::for_each_sample(tris, ppu, full, |i, _| occ[i] = true);
    // Join cells within GAP of each other, so a doorway or a gap between
    // meshes does not split a zone into pieces.
    const GAP: isize = 2;
    let mut uf = UnionFind::new(w * h);
    for z in 0..h as isize {
        for x in 0..w as isize {
            let i = (z * w as isize + x) as usize;
            if !occ[i] { continue }
            for dz in 0..=GAP {
                for dx in -GAP..=GAP {
                    if dz == 0 && dx <= 0 { continue }
                    let (nx, nz) = (x + dx, z + dz);
                    if nx < 0 || nx >= w as isize || nz >= h as isize { continue }
                    let j = (nz * w as isize + nx) as usize;
                    if occ[j] { uf.union(i, j) }
                }
            }
        }
    }
    // Cells holding objects, per cluster.
    let obj_cells: std::collections::HashSet<usize> = objects.iter().filter_map(|p| {
        let (fx, fz) = ((p[0] - x0) * ppu, (p[1] - z0) * ppu);
        if fx < 0.0 || fz < 0.0 { return None }
        let (cx, cz) = (fx as usize, fz as usize);
        (cx < w && cz < h && occ[cz * w + cx]).then(|| cz * w + cx)
    }).collect();
    let mut objs = std::collections::HashMap::<usize, usize>::new();
    for &i in &obj_cells { *objs.entry(uf.find(i)).or_default() += 1 }
    let total_objs = obj_cells.len();
    // Per cluster: cell count and cell-index box.
    let mut info = std::collections::HashMap::<usize, (usize, usize, usize, usize, usize)>::new();
    let total = occ.iter().filter(|&&o| o).count();
    for i in 0..w * h {
        if !occ[i] { continue }
        let (x, z) = (i % w, i / w);
        let e = info.entry(uf.find(i)).or_insert((0, x, x, z, z));
        e.0 += 1;
        e.1 = e.1.min(x); e.2 = e.2.max(x); e.3 = e.3.min(z); e.4 = e.4.max(z);
    }
    let objs_of = |r: &usize| objs.get(r).copied().unwrap_or(0);
    let is_main = |r: &usize, c: &(usize, usize, usize, usize, usize)|
        c.0 * 5 >= total || (total_objs > 0 && objs_of(r) * 10 >= total_objs);
    let main = info.iter().filter(|(r, c)| is_main(r, c)).map(|(_, c)| c)
        .fold(None, |a: Option<(usize, usize, usize, usize)>, c| Some(match a {
            None => (c.1, c.2, c.3, c.4),
            Some(a) => (a.0.min(c.1), a.1.max(c.2), a.2.min(c.3), a.3.max(c.4)),
        }));
    let Some(m) = main else { return vec![true; tris.len()] };
    let grow = (((m.1 - m.0).max(m.3 - m.2)) as f64 * 0.15) as isize;
    let (mx0, mx1, mz0, mz1) = (m.0 as isize - grow, m.1 as isize + grow,
                                m.2 as isize - grow, m.3 as isize + grow);
    let stray: std::collections::HashSet<usize> = info.iter()
        .filter(|(r, c)| !is_main(r, c)
            && objs_of(r) < 3.max(total_objs / 50)
            && ((c.2 as isize) < mx0 || (c.1 as isize) > mx1
                || (c.4 as isize) < mz0 || (c.3 as isize) > mz1))
        .map(|(r, _)| *r)
        .collect();
    if stray.is_empty() { return vec![true; tris.len()] }
    tris.iter().map(|t| {
        let cx = (t[0][0] + t[1][0] + t[2][0]) as f64 / 3.0;
        let cz = (t[0][2] + t[1][2] + t[2][2]) as f64 / 3.0;
        let (gx, gz) = (((cx - x0) * ppu) as usize, ((cz - z0) * ppu) as usize);
        let i = gz.min(h - 1) * w + gx.min(w - 1);
        // A centroid can fall in an empty cell of a thin triangle; keep it.
        !occ[i] || !stray.contains(&uf.find(i))
    }).collect()
}

/// Drop vast flat triangles lying over the zone: ceiling or boundary planes.
///
/// Shaded Dunes carries two triangles at its very top that between them
/// cover nearly half its frame, and Night Harbor nine, one covering 91%.
/// They face up, so they pass as floor, and the top-down view draws the
/// highest surface -- one such plane blanks the whole map. A triangle goes
/// if it alone covers over 5% of `frame` and sits above 95% of the zone's
/// other walkable area. Large planes lower down (water levels) stay.
pub fn sky_keep(tris: &[Tri], frame: Bbox) -> Vec<bool> {
    let frame_area = ((frame.1 - frame.0) * (frame.3 - frame.2)).max(1.0);
    let area = |t: &Tri| 0.5 * ((t[1][0] - t[0][0]) as f64 * (t[2][2] - t[0][2]) as f64
        - (t[2][0] - t[0][0]) as f64 * (t[1][2] - t[0][2]) as f64).abs();
    let y = |t: &Tri| (t[0][1] + t[1][1] + t[2][1]) / 3.0;
    let huge = |t: &Tri| area(t) > 0.05 * frame_area;
    if !tris.iter().any(huge) { return vec![true; tris.len()] }
    // Area-weighted 95th percentile of the other triangles' heights.
    let mut ys: Vec<(f32, f64)> = tris.iter().filter(|t| !huge(t)).map(|t| (y(t), area(t))).collect();
    if ys.is_empty() { return vec![true; tris.len()] }
    ys.sort_by(|a, b| a.0.partial_cmp(&b.0).unwrap_or(std::cmp::Ordering::Equal));
    let total: f64 = ys.iter().map(|p| p.1).sum();
    let mut acc = 0.0;
    let mut top = ys[ys.len() - 1].0;
    for p in &ys {
        acc += p.1;
        if acc >= 0.95 * total { top = p.0; break }
    }
    tris.iter().map(|t| !(huge(t) && y(t) > top)).collect()
}

struct UnionFind { parent: Vec<usize> }

impl UnionFind {
    fn new(n: usize) -> Self { Self { parent: (0..n).collect() } }
    fn find(&mut self, mut i: usize) -> usize {
        while self.parent[i] != i {
            self.parent[i] = self.parent[self.parent[i]];
            i = self.parent[i];
        }
        i
    }
    fn union(&mut self, a: usize, b: usize) {
        let (ra, rb) = (self.find(a), self.find(b));
        if ra != rb { self.parent[ra] = rb }
    }
}

/// The triangles a keep-mask from this module keeps.
pub fn apply<T: Copy>(v: &[T], keep: &[bool]) -> Vec<T> {
    v.iter().zip(keep).filter(|(_, k)| **k).map(|(t, _)| *t).collect()
}
