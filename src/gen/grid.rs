//! The image-processing primitives the renderer needs.
//!
//! These replace the handful of scipy/skimage calls the Python generator used.
//! They are written out rather than pulled from a crate because the set is
//! small, the exact edge behaviour matters for matching the existing maps, and
//! the alternative was three dependencies for a dozen functions.
//!
//! Conventions follow scipy so the ported code reads the same: gaussian uses
//! `reflect` edges and a 4-sigma truncation, `label` is 4-connected, and the
//! distance transform measures to the nearest zero.

/// A dense f32 raster, row-major, row 0 = south (matching the Python).
#[derive(Clone)]
pub struct Grid {
    pub w: usize,
    pub h: usize,
    pub v: Vec<f32>,
}

/// A boolean raster of the same shape.
#[derive(Clone)]
pub struct Mask {
    pub w: usize,
    pub h: usize,
    pub v: Vec<bool>,
}

impl Grid {
    pub fn new(w: usize, h: usize, fill: f32) -> Self {
        Self { w, h, v: vec![fill; w * h] }
    }
    #[inline]
    pub fn at(&self, x: usize, y: usize) -> f32 {
        self.v[y * self.w + x]
    }
    #[inline]
    pub fn set(&mut self, x: usize, y: usize, t: f32) {
        self.v[y * self.w + x] = t;
    }
    pub fn finite(&self) -> Mask {
        Mask { w: self.w, h: self.h, v: self.v.iter().map(|t| t.is_finite()).collect() }
    }
    /// Values selected by a mask, for quantile work.
    pub fn select(&self, m: &Mask) -> Vec<f32> {
        self.v.iter().zip(&m.v).filter(|(_, b)| **b).map(|(t, _)| *t).collect()
    }
}

impl Mask {
    pub fn new(w: usize, h: usize, fill: bool) -> Self {
        Self { w, h, v: vec![fill; w * h] }
    }
    #[inline]
    pub fn at(&self, x: usize, y: usize) -> bool {
        self.v[y * self.w + x]
    }
    pub fn any(&self) -> bool {
        self.v.iter().any(|b| *b)
    }
    pub fn count(&self) -> usize {
        self.v.iter().filter(|b| **b).count()
    }
    pub fn not(&self) -> Mask {
        Mask { w: self.w, h: self.h, v: self.v.iter().map(|b| !b).collect() }
    }
    pub fn or(&self, o: &Mask) -> Mask {
        Mask { w: self.w, h: self.h, v: self.v.iter().zip(&o.v).map(|(a, b)| *a || *b).collect() }
    }
    pub fn andnot(&self, o: &Mask) -> Mask {
        Mask { w: self.w, h: self.h, v: self.v.iter().zip(&o.v).map(|(a, b)| *a && !*b).collect() }
    }
}

// --------------------------------------------------------------- quantiles

/// Linear-interpolation quantile, matching numpy's default method.
pub fn quantile(sorted: &[f32], q: f64) -> f32 {
    if sorted.is_empty() {
        return f32::NAN;
    }
    let pos = q.clamp(0.0, 1.0) * (sorted.len() - 1) as f64;
    let lo = pos.floor() as usize;
    let hi = pos.ceil() as usize;
    if lo == hi {
        return sorted[lo];
    }
    let f = (pos - lo as f64) as f32;
    sorted[lo] * (1.0 - f) + sorted[hi] * f
}

/// [`quantile`] of the finite values in `v` at each of `qs`, without sorting
/// them all: the ranks quantile reads are picked out directly, so the values
/// are exactly the same. Sorting every land height of a map to read two
/// quartiles was one of the slower steps of drawing it.
pub fn quantiles(v: &[f32], qs: &[f64]) -> Vec<f32> {
    let mut s: Vec<f32> = v.iter().copied().filter(|t| t.is_finite()).collect();
    if s.is_empty() { return vec![f32::NAN; qs.len()] }
    let n = s.len();
    let mut ranks: Vec<usize> = qs.iter().flat_map(|q| {
        let pos = q.clamp(0.0, 1.0) * (n - 1) as f64;
        [pos.floor() as usize, pos.ceil() as usize]
    }).collect();
    ranks.sort_unstable();
    ranks.dedup();
    // Each pick partitions what is left above the previous one.
    let mut at: std::collections::HashMap<usize, f32> = Default::default();
    let mut lo = 0;
    for &r in &ranks {
        let (_, v, _) = s[lo..].select_nth_unstable_by(r - lo, |a, b| a.partial_cmp(b).unwrap());
        at.insert(r, *v);
        lo = r;
    }
    qs.iter().map(|q| {
        let pos = q.clamp(0.0, 1.0) * (n - 1) as f64;
        let (l, h) = (pos.floor() as usize, pos.ceil() as usize);
        if l == h { return at[&l] }
        let f = (pos - l as f64) as f32;
        at[&l] * (1.0 - f) + at[&h] * f
    }).collect()
}

pub fn sorted_copy(v: &[f32]) -> Vec<f32> {
    let mut s: Vec<f32> = v.iter().copied().filter(|t| t.is_finite()).collect();
    s.sort_by(|a, b| a.partial_cmp(b).unwrap());
    s
}

// ------------------------------------------------------------ parallelism
//
// Every filter here works row by row and then column by column, and each row
// (or column) is independent. Rows are split across threads directly; for
// columns the grid is transposed so they become rows, processed the same way,
// and transposed back. A transpose is cheap next to the filter, and it keeps
// every thread writing its own contiguous memory. Results are bit-identical
// to the sequential forms (--selftest checks the sums).

use rayon::prelude::*;

fn transpose<T: Copy + Send + Sync + Default>(v: &[T], w: usize, h: usize) -> Vec<T> {
    let mut out = vec![T::default(); w * h];
    out.par_chunks_mut(h).enumerate().for_each(|(x, col)| {
        super::throttle::gate();
        for y in 0..h { col[y] = v[y * w + x] }
    });
    out
}

/// Apply `f(row_in, row_out)` to every row of a `w`-wide raster in parallel.
fn rows<T: Copy + Send + Sync + Default, U: Copy + Send + Sync + Default>(
    v: &[T], w: usize, f: impl Fn(&[T], &mut [U]) + Sync,
) -> Vec<U> {
    let mut out = vec![U::default(); v.len()];
    out.par_chunks_mut(w).zip(v.par_chunks(w)).for_each(|(o, i)| { super::throttle::gate(); f(i, o) });
    out
}

/// As [`rows`], but along columns.
fn cols<T: Copy + Send + Sync + Default, U: Copy + Send + Sync + Default>(
    v: &[T], w: usize, h: usize, f: impl Fn(&[T], &mut [U]) + Sync,
) -> Vec<U> {
    let t = transpose(v, w, h);
    let r = rows(&t, h, f);
    transpose(&r, h, w)
}

// ---------------------------------------------------------------- gaussian

/// scipy's reflect mode: `d c b a | a b c d | d c b a`.
#[inline]
fn reflect(i: isize, n: isize) -> usize {
    let mut i = i;
    let period = 2 * n;
    while i < 0 || i >= n {
        if i < 0 {
            i = -i - 1;
        }
        if i >= n {
            i = period - i - 1;
        }
    }
    i as usize
}

fn gauss_kernel(sigma: f32) -> Vec<f32> {
    if sigma <= 0.0 {
        return vec![1.0];
    }
    let r = (4.0 * sigma + 0.5) as isize; // scipy truncate = 4.0
    let mut k: Vec<f32> = (-r..=r)
        .map(|i| (-(i as f32).powi(2) / (2.0 * sigma * sigma)).exp())
        .collect();
    let s: f32 = k.iter().sum();
    for t in &mut k {
        *t /= s;
    }
    k
}

/// Separable gaussian blur with independent sigmas, as scipy allows.
pub fn gaussian_xy(g: &Grid, sigma_x: f32, sigma_y: f32) -> Grid {
    fn conv(k: &[f32]) -> impl Fn(&[f32], &mut [f32]) + Sync + '_ {
        let r = (k.len() / 2) as isize;
        move |src: &[f32], dst: &mut [f32]| {
            let n = src.len() as isize;
            for x in 0..src.len() {
                let mut acc = 0.0;
                for (j, kv) in k.iter().enumerate() {
                    acc += src[reflect(x as isize + j as isize - r, n)] * kv;
                }
                dst[x] = acc;
            }
        }
    }
    let (kx, ky) = (gauss_kernel(sigma_x), gauss_kernel(sigma_y));
    let tmp = rows(&g.v, g.w, conv(&kx));
    Grid { w: g.w, h: g.h, v: cols(&tmp, g.w, g.h, conv(&ky)) }
}

pub fn gaussian(g: &Grid, sigma: f32) -> Grid {
    gaussian_xy(g, sigma, sigma)
}

/// Bilinear upsample to an exact target size.
pub fn upsample(g: &Grid, w: usize, h: usize) -> Grid {
    let mut out = Grid::new(w, h, 0.0);
    let sx = (g.w.max(2) - 1) as f32 / (w.max(2) - 1) as f32;
    let sy = (g.h.max(2) - 1) as f32 / (h.max(2) - 1) as f32;
    out.v.par_chunks_mut(w).enumerate().for_each(|(y, row)| {
        super::throttle::gate();
        let fy = (y as f32 * sy).clamp(0.0, g.h as f32 - 1.0);
        let (y0, fry) = (fy.floor() as usize, fy.fract());
        let y1 = (y0 + 1).min(g.h - 1);
        for x in 0..w {
            let fx = (x as f32 * sx).clamp(0.0, g.w as f32 - 1.0);
            let (x0, frx) = (fx.floor() as usize, fx.fract());
            let x1 = (x0 + 1).min(g.w - 1);
            let v = g.at(x0, y0) * (1.0 - frx) * (1.0 - fry)
                + g.at(x1, y0) * frx * (1.0 - fry)
                + g.at(x0, y1) * (1.0 - frx) * fry
                + g.at(x1, y1) * frx * fry;
            row[x] = v;
        }
    });
    out
}

/// A smooth random field of the given size, blurred to `sigma`.
///
/// Built at reduced resolution and upsampled: a field blurred to sigma 26 has
/// no detail finer than ~26px, so computing it at full resolution means a
/// 209-tap kernel per axis producing something a 1/16-scale field reproduces
/// exactly. This was 64% of total render time.
pub fn smooth_noise(w: usize, h: usize, sigma: f32, seed: u32) -> Grid {
    // Two samples per sigma is the Nyquist limit for a gaussian-blurred field,
    // and this is noise rather than signal -- nothing here is being measured,
    // so there is no detail for the upsample to lose. At four samples per sigma
    // the pen-pressure field (sigma 5) fell back to a full 43-tap convolution.
    let step = ((sigma / 2.0).floor() as usize).clamp(1, 32);
    if step <= 1 {
        return gaussian(&noise(w, h, seed), sigma);
    }
    let (lw, lh) = ((w / step).max(4), (h / step).max(4));
    let small = gaussian(&noise(lw, lh, seed), sigma / step as f32);
    upsample(&small, w, h)
}

/// Gaussian blur that switches to a three-pass box blur for large sigma.
///
/// Three boxes approximate a gaussian to within a few percent (central limit),
/// at O(1) per pixel via running sums instead of O(sigma). Used where the
/// result is a smoothing field rather than something measured exactly.
pub fn gaussian_approx(g: &Grid, sigma: f32) -> Grid {
    if sigma < 4.0 {
        return gaussian(g, sigma);
    }
    // Box widths whose triple convolution matches the gaussian's variance.
    let n = 3.0f32;
    let wi = ((12.0 * sigma * sigma / n + 1.0).sqrt() - 1.0).floor();
    let mut wl = wi as usize;
    if wl % 2 == 0 {
        wl -= 1;
    }
    let wu = wl + 2;
    let m = ((12.0 * sigma * sigma - (n * wl as f32 * wl as f32)
        - (4.0 * n * wl as f32) - 3.0 * n)
        / (-4.0 * wl as f32 - 4.0))
        .round() as usize;
    let mut out = g.clone();
    for i in 0..3 {
        let r = if i < m { wl / 2 } else { wu / 2 };
        out = box_blur(&out, r);
    }
    out
}

fn box_blur(g: &Grid, r: usize) -> Grid {
    if r == 0 {
        return g.clone();
    }
    let norm = 1.0 / (2 * r + 1) as f32;
    let line = |src: &[f32], dst: &mut [f32]| {
        let n = src.len() as isize;
        let mut acc = 0.0f32;
        for i in -(r as isize)..=(r as isize) {
            acc += src[reflect(i, n)];
        }
        for x in 0..src.len() {
            dst[x] = acc * norm;
            acc += src[reflect(x as isize + r as isize + 1, n)] - src[reflect(x as isize - r as isize, n)];
        }
    };
    let tmp = rows(&g.v, g.w, line);
    Grid { w: g.w, h: g.h, v: cols(&tmp, g.w, g.h, line) }
}

// ------------------------------------------------------ distance transform

/// Exact squared Euclidean distance transform (Felzenszwalb & Huttenlocher),
/// plus the index of the nearest true pixel -- scipy's `return_indices`.
///
/// Distance is measured to the nearest **true** pixel of `src`.
pub fn edt(src: &Mask) -> (Grid, Vec<u32>) {
    // f64 throughout. The parabola intersections subtract squared indices,
    // and in f32 those run out of precision past ~4,000 pixels (16.7M): on a
    // tall raster pixels inside the mask came out at a nonzero distance, and
    // the rock hatching that keys off this distance drew a dot screen over
    // whole regions of land at fine zoom.
    let (w, h) = (src.w, src.h);
    const INF: f64 = 1e30;
    let mut f = vec![INF; w * h];
    let mut idx = vec![u32::MAX; w * h];
    for i in 0..w * h {
        if src.v[i] {
            f[i] = 0.0;
            idx[i] = i as u32;
        }
    }
    // columns, then rows: the 1-D transform is applied along each axis.
    // Each line is independent; columns go through a transpose.
    fn pass(f: &mut [f64], idx: &mut [u32]) {
        const INF: f64 = 1e30;
        let n = f.len();
        let d: Vec<f64> = f.to_vec();
        let di: Vec<u32> = idx.to_vec();
        let mut vpos = vec![0usize; n];
        let mut z = vec![0.0f64; n + 1];
        let mut k = 0usize;
        vpos[0] = 0;
        z[0] = -INF;
        z[1] = INF;
        for q in 1..n {
            loop {
                let p = vpos[k];
                let (qf, pf) = (q as f64, p as f64);
                let s = ((d[q] + qf * qf) - (d[p] + pf * pf)) / (2.0 * qf - 2.0 * pf);
                if s <= z[k] && k > 0 {
                    k -= 1;
                } else {
                    k += 1;
                    vpos[k] = q;
                    z[k] = s;
                    z[k + 1] = INF;
                    break;
                }
            }
        }
        k = 0;
        for q in 0..n {
            while z[k + 1] < q as f64 {
                k += 1;
            }
            let p = vpos[k];
            f[q] = (q as f64 - p as f64).powi(2) + d[p];
            idx[q] = di[p];
        }
    }
    let both = |f: &mut Vec<f64>, idx: &mut Vec<u32>, lw: usize| {
        f.par_chunks_mut(lw).zip(idx.par_chunks_mut(lw)).for_each(|(a, b)| { super::throttle::gate(); pass(a, b) });
    };
    let mut ft = transpose(&f, w, h);
    let mut it = transpose(&idx, w, h);
    both(&mut ft, &mut it, h);
    let mut f = transpose(&ft, h, w);
    let mut idx = transpose(&it, h, w);
    both(&mut f, &mut idx, w);
    let g = Grid { w, h, v: f.iter().map(|t| t.max(0.0).sqrt() as f32).collect() };
    (g, idx)
}

/// Distance to the nearest *false* pixel -- what scipy gives for a boolean
/// input, where background is zero.
pub fn distance_to_background(m: &Mask) -> Grid {
    edt(&m.not()).0
}

// ------------------------------------------------------------- morphology

/// A filled disk of radius r, as scipy's `disk()` produced.
pub fn disk(r: usize) -> Vec<(isize, isize)> {
    let r = r as isize;
    let mut out = Vec::new();
    for y in -r..=r {
        for x in -r..=r {
            if x * x + y * y <= r * r {
                out.push((x, y));
            }
        }
    }
    out
}

pub fn dilate(m: &Mask, se: &[(isize, isize)]) -> Mask {
    // Gathered rather than scattered, so rows can be filled independently:
    // a pixel is set if any pixel of the reflected element is. Whole rows at
    // a time, one shifted row per offset, rather than an offset at a time per
    // pixel: the same result, in a form the compiler can vectorise.
    let mut out = Mask::new(m.w, m.h, false);
    let (w, h) = (m.w as isize, m.h as isize);
    out.v.par_chunks_mut(m.w.max(1)).enumerate().for_each(|(y, row)| {
        super::throttle::gate();
        for &(dx, dy) in se {
            let sy = y as isize - dy;
            if sy < 0 || sy >= h { continue }
            let src = &m.v[sy as usize * m.w..(sy as usize + 1) * m.w];
            // out[x] |= src[x - dx] wherever x - dx is on the row.
            let (a, b) = (dx.max(0), (w + dx).min(w));
            if a >= b { continue }
            let (a, b) = (a as usize, b as usize);
            let s0 = (a as isize - dx) as usize;
            for (o, s) in row[a..b].iter_mut().zip(&src[s0..s0 + (b - a)]) { *o |= *s }
        }
    });
    out
}

pub fn erode(m: &Mask, se: &[(isize, isize)]) -> Mask {
    let mut out = Mask::new(m.w, m.h, true);
    let (w, h) = (m.w as isize, m.h as isize);
    out.v.par_chunks_mut(m.w.max(1)).enumerate().for_each(|(y, row)| {
        super::throttle::gate();
        for &(dx, dy) in se {
            let sy = y as isize + dy;
            // scipy's binary_erosion pads with border_value = 0, so a pixel
            // whose structuring element hangs off the frame is eroded.
            if sy < 0 || sy >= h { row.fill(false); break }
            let src = &m.v[sy as usize * m.w..(sy as usize + 1) * m.w];
            // out[x] &= src[x + dx] wherever x + dx is on the row; elsewhere false.
            let (a, b) = ((-dx).max(0), (w - dx).min(w));
            if a >= b { row.fill(false); break }
            let (a, b) = (a as usize, b as usize);
            row[..a].fill(false);
            row[b..].fill(false);
            let s0 = (a as isize + dx) as usize;
            for (o, s) in row[a..b].iter_mut().zip(&src[s0..s0 + (b - a)]) { *o &= *s }
        }
    });
    out
}

pub fn closing(m: &Mask, se: &[(isize, isize)]) -> Mask {
    erode(&dilate(m, se), se)
}

pub fn opening(m: &Mask, se: &[(isize, isize)]) -> Mask {
    dilate(&erode(m, se), se)
}

pub fn outline(m: &Mask, w: usize) -> Mask {
    m.andnot(&erode(m, &disk(w.max(1))))
}

// --------------------------------------------------- connected components

/// 4-connected labelling, matching `scipy.ndimage.label`'s default structure.
/// Returns (labels, count); label 0 is background. Components are numbered in
/// the raster order of their first pixel, as scipy numbers them.
///
/// Works on runs: each row's runs of set pixels, joined to the runs they
/// touch in the row above, then numbered in raster order. A pixel-by-pixel
/// flood fill did the same thing one pixel at a time, on one core, and tidying
/// a zone's land and elevation bands spent more time in it than anywhere else.
pub fn label(m: &Mask) -> (Vec<u32>, usize) {
    let (w, h) = (m.w, m.h);
    let rows: Vec<Vec<(u32, u32)>> = (0..h).into_par_iter().map(|y| {
        let row = &m.v[y * w..(y + 1) * w];
        let mut runs = Vec::new();
        let mut x = 0;
        while x < w {
            if row[x] {
                let x0 = x;
                while x < w && row[x] { x += 1 }
                runs.push((x0 as u32, x as u32));
            } else {
                x += 1;
            }
        }
        runs
    }).collect();
    let mut first = vec![0usize; h + 1];
    for y in 0..h { first[y + 1] = first[y] + rows[y].len() }
    let total = first[h];
    let mut parent: Vec<u32> = (0..total as u32).collect();
    fn find(parent: &mut [u32], mut i: u32) -> u32 {
        while parent[i as usize] != i {
            parent[i as usize] = parent[parent[i as usize] as usize];
            i = parent[i as usize];
        }
        i
    }
    for y in 1..h {
        let (above, here) = (&rows[y - 1], &rows[y]);
        let (mut i, mut j) = (0, 0);
        // Two runs touch, 4-connected, when they share a column.
        while i < above.len() && j < here.len() {
            let (a, b) = (above[i], here[j]);
            if a.0 < b.1 && b.0 < a.1 {
                let (ra, rb) = (find(&mut parent, (first[y - 1] + i) as u32),
                                find(&mut parent, (first[y] + j) as u32));
                if ra != rb { parent[ra.max(rb) as usize] = ra.min(rb) }
            }
            if a.1 <= b.1 { i += 1 } else { j += 1 }
        }
    }
    let mut label_of = vec![0u32; total];
    let mut run_label = vec![0u32; total];
    let mut n = 0u32;
    for r in 0..total {
        let root = find(&mut parent, r as u32) as usize;
        if label_of[root] == 0 { n += 1; label_of[root] = n }
        run_label[r] = label_of[root];
    }
    let mut lab = vec![0u32; w * h];
    lab.par_chunks_mut(w.max(1)).enumerate().for_each(|(y, row)| {
        for (j, &(x0, x1)) in rows[y].iter().enumerate() {
            row[x0 as usize..x1 as usize].fill(run_label[first[y] + j]);
        }
    });
    (lab, n as usize)
}

pub fn component_sizes(lab: &[u32], n: usize) -> Vec<usize> {
    let mut sz = vec![0usize; n + 1];
    for &l in lab {
        sz[l as usize] += 1;
    }
    sz[0] = 0;
    sz
}

/// Bounding box per label: (x0, y0, x1, y1) exclusive, as `find_objects`.
pub fn component_boxes(lab: &[u32], n: usize, w: usize) -> Vec<Option<(usize, usize, usize, usize)>> {
    let mut out: Vec<Option<(usize, usize, usize, usize)>> = vec![None; n + 1];
    for (i, &l) in lab.iter().enumerate() {
        if l == 0 {
            continue;
        }
        let (x, y) = (i % w, i / w);
        let e = &mut out[l as usize];
        *e = Some(match *e {
            None => (x, y, x + 1, y + 1),
            Some((x0, y0, x1, y1)) => (x0.min(x), y0.min(y), x1.max(x + 1), y1.max(y + 1)),
        });
    }
    out
}

// -------------------------------------------------------- rank filters

/// Sliding-window maximum over a square window of side `win` (odd).
pub fn max_filter(g: &Grid, win: usize) -> Grid {
    rank_filter(g, win, true)
}

pub fn min_filter(g: &Grid, win: usize) -> Grid {
    rank_filter(g, win, false)
}

/// van Herk / Gil-Werman: a sliding max or min in O(1) per pixel regardless of
/// window size, instead of O(window).
///
/// This matters because the wall detector's window is in WORLD units -- it is
/// `3 * ppu / 0.9` pixels wide, so it grows with resolution. At the finest
/// zoom the naive form was doing ~27 comparisons per pixel per axis; this does
/// three passes over the row and is exact, not an approximation.
fn rank_filter(g: &Grid, win: usize, want_max: bool) -> Grid {
    let r = win / 2;
    let line = |src: &[f32], dst: &mut [f32]| {
        let n = src.len();
        let row: Vec<f32> = (0..n + 2 * r).map(|i| src[reflect(i as isize - r as isize, n as isize)]).collect();
        let mut pre = vec![0.0f32; n + 2 * r];
        let mut suf = vec![0.0f32; n + 2 * r];
        line_rank(&row, win, want_max, &mut pre, &mut suf);
        dst.copy_from_slice(&pre[..n]);
    };
    let tmp = rows(&g.v, g.w, line);
    Grid { w: g.w, h: g.h, v: cols(&tmp, g.w, g.h, line) }
}

/// One-dimensional windowed max/min. `src` is already padded by the radius on
/// both sides; the first `src.len() - 2*r` results are written back into `out`.
fn line_rank(src: &[f32], win: usize, want_max: bool, out: &mut [f32], suf: &mut [f32]) {
    let n = src.len();
    let w = win.max(1);
    // Running extreme forward within each block of `w`, and backward likewise.
    let mut pre = vec![0.0f32; n];
    for i in 0..n {
        pre[i] = if i % w == 0 { src[i] } else { pick(pre[i - 1], src[i], want_max) };
    }
    for i in (0..n).rev() {
        suf[i] = if i % w == w - 1 || i == n - 1 {
            src[i]
        } else {
            pick(suf[i + 1], src[i], want_max)
        };
    }
    let r = w / 2;
    let count = n - 2 * r;
    for i in 0..count {
        // The window [i, i+w) spans at most two blocks: take the suffix of the
        // first and the prefix of the last.
        let a = suf[i];
        let j = i + w - 1;
        let b = if j < n { pre[j] } else { pre[n - 1] };
        out[i] = pick(a, b, want_max);
    }
    let _ = r;
}

#[inline]
fn pick(acc: f32, v: f32, want_max: bool) -> f32 {
    if !acc.is_finite() {
        return v;
    }
    if !v.is_finite() {
        return acc;
    }
    if want_max {
        acc.max(v)
    } else {
        acc.min(v)
    }
}

/// 3x3 median, as used to tidy the nearest-neighbour height fill.
pub fn median3(g: &Grid) -> Grid {
    let (w, h) = (g.w as isize, g.h as isize);
    let mut out = Grid::new(g.w, g.h, 0.0);
    out.v.par_chunks_mut(g.w).enumerate().for_each(|(y, row)| {
        super::throttle::gate();
        let mut buf = [0.0f32; 9];
        for x in 0..g.w {
            let mut n = 0;
            for dy in -1..=1 {
                for dx in -1..=1 {
                    let sx = reflect(x as isize + dx, w);
                    let sy = reflect(y as isize + dy, h);
                    buf[n] = g.at(sx, sy);
                    n += 1;
                }
            }
            let s = &mut buf[..n];
            s.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
            row[x] = s[n / 2];
        }
    });
    out
}

/// Central-difference gradient, as numpy's `gradient`: (d/dy, d/dx).
pub fn gradient(g: &Grid) -> (Grid, Grid) {
    let mut gy = Grid::new(g.w, g.h, 0.0);
    let mut gx = Grid::new(g.w, g.h, 0.0);
    gy.v.par_chunks_mut(g.w).zip(gx.v.par_chunks_mut(g.w)).enumerate().for_each(|(y, (ry, rx))| {
        super::throttle::gate();
        for x in 0..g.w {
            let a = g.at(x, if y + 1 < g.h { y + 1 } else { y });
            let b = g.at(x, y.saturating_sub(1));
            let span = if y + 1 < g.h && y > 0 { 2.0 } else { 1.0 };
            ry[x] = (a - b) / span;
            let c = g.at(if x + 1 < g.w { x + 1 } else { x }, y);
            let d = g.at(x.saturating_sub(1), y);
            let span = if x + 1 < g.w && x > 0 { 2.0 } else { 1.0 };
            rx[x] = (c - d) / span;
        }
    });
    (gy, gx)
}

// ------------------------------------------------------------ skeletonize

/// Zhang-Suen thinning -- skimage's default 2-D skeletonize.
///
/// The wall detector flags a band as wide as its window, so the raw mask is a
/// slab tens of pixels across. Reducing it to a centreline is what lets a wall
/// be drawn at a fixed weight regardless of zoom.
///
/// Each sub-step decides every removal from the image as it stood when the
/// sub-step began, so the order pixels are looked at in does not matter: only
/// the pixels still set are visited, in parallel, and the result is the same
/// as scanning the whole image one pixel at a time. That scan took dozens of
/// passes over every pixel to thin a thick band, on one core -- the slowest
/// single step of a build.
pub fn skeletonize(m: &Mask) -> Mask {
    let (w, h) = (m.w, m.h);
    let mut img = m.clone();
    if w < 3 || h < 3 { return img }
    let mut on: Vec<u32> = (0..w * h)
        .filter(|&i| img.v[i] && (1..w - 1).contains(&(i % w)) && (1..h - 1).contains(&(i / w)))
        .map(|i| i as u32)
        .collect();
    loop {
        let mut changed = false;
        for step in 0..2 {
            let img_ref = &img;
            let remove: Vec<u32> = on.par_chunks(4096).flat_map_iter(|chunk| {
                super::throttle::gate();
                chunk.iter().copied().filter(move |&i| thins(img_ref, i as usize, step))
            }).collect();
            if !remove.is_empty() {
                changed = true;
                for &i in &remove { img.v[i as usize] = false }
                on.retain(|&i| img.v[i as usize]);
            }
        }
        if !changed {
            break;
        }
    }
    img
}

/// Whether Zhang-Suen sub-step `step` removes the set pixel at `i`, which is
/// not on the image's edge.
fn thins(img: &Mask, i: usize, step: usize) -> bool {
    let w = img.w;
    let (x, y) = (i % w, i / w);
    // P2..P9 clockwise from north.
    let p = [
        img.at(x, y - 1),
        img.at(x + 1, y - 1),
        img.at(x + 1, y),
        img.at(x + 1, y + 1),
        img.at(x, y + 1),
        img.at(x - 1, y + 1),
        img.at(x - 1, y),
        img.at(x - 1, y - 1),
    ];
    let b = p.iter().filter(|t| **t).count();
    if !(2..=6).contains(&b) {
        return false;
    }
    let a = (0..8).filter(|&i| !p[i] && p[(i + 1) % 8]).count();
    if a != 1 {
        return false;
    }
    let (c1, c2) = if step == 0 {
        (p[0] && p[2] && p[4], p[2] && p[4] && p[6])
    } else {
        (p[0] && p[2] && p[6], p[0] && p[4] && p[6])
    };
    !(c1 || c2)
}

// --------------------------------------------------------------- warping

/// Bilinear resample at arbitrary coordinates, clamped at the edges --
/// `map_coordinates(order=1, mode="nearest")`.
pub fn warp(g: &Grid, dx: &Grid, dy: &Grid) -> Grid {
    let mut out = Grid::new(g.w, g.h, 0.0);
    // Rows in parallel: run once per colour plane, three planes side by
    // side used three cores and left the rest idle.
    out.v.par_chunks_mut(g.w.max(1)).enumerate().for_each(|(y, row)| {
        super::throttle::gate();
        for x in 0..g.w {
            let sx = (x as f32 + dx.at(x, y)).clamp(0.0, g.w as f32 - 1.0);
            let sy = (y as f32 + dy.at(x, y)).clamp(0.0, g.h as f32 - 1.0);
            let (x0, y0) = (sx.floor() as usize, sy.floor() as usize);
            let (x1, y1) = ((x0 + 1).min(g.w - 1), (y0 + 1).min(g.h - 1));
            let (fx, fy) = (sx - x0 as f32, sy - y0 as f32);
            let v = g.at(x0, y0) * (1.0 - fx) * (1.0 - fy)
                + g.at(x1, y0) * fx * (1.0 - fy)
                + g.at(x0, y1) * (1.0 - fx) * fy
                + g.at(x1, y1) * fx * fy;
            row[x] = v;
        }
    });
    out
}

/// Deterministic value noise in [0,1), standing in for a seeded RNG field.
/// The renderer only needs *a* stable random field, and this keeps generated
/// maps byte-identical across runs and machines.
pub fn noise(w: usize, h: usize, seed: u32) -> Grid {
    let mut g = Grid::new(w, h, 0.0);
    for y in 0..h {
        for x in 0..w {
            let mut s = (x as u32)
                .wrapping_mul(0x9E3779B9)
                .wrapping_add((y as u32).wrapping_mul(0x85EBCA6B))
                .wrapping_add(seed.wrapping_mul(0xC2B2AE35));
            s ^= s >> 15;
            s = s.wrapping_mul(0x2545F491);
            s ^= s >> 13;
            s = s.wrapping_mul(0x3C79AC49);
            s ^= s >> 16;
            g.set(x, y, (s >> 8) as f32 / 16_777_216.0);
        }
    }
    g
}

/// Zero-mean, unit-variance, as the renderer's `nrm()` helper.
pub fn normalize(g: &Grid) -> Grid {
    let n = g.v.len().max(1) as f32;
    let mean = g.v.iter().sum::<f32>() / n;
    let var = g.v.iter().map(|t| (t - mean).powi(2)).sum::<f32>() / n;
    let sd = var.sqrt() + 1e-6;
    Grid { w: g.w, h: g.h, v: g.v.iter().map(|t| (t - mean) / sd).collect() }
}

#[cfg(test)]
mod skeleton_tests {
    use super::*;

    /// The whole-image scan the parallel version replaced, kept to check it.
    /// Zhang-Suen thinning -- skimage's default 2-D skeletonize.
    ///
    /// The wall detector flags a band as wide as its window, so the raw mask is a
    /// slab tens of pixels across. Reducing it to a centreline is what lets a wall
    /// be drawn at a fixed weight regardless of zoom.
    fn skeletonize_reference(m: &Mask) -> Mask {
        let (w, h) = (m.w, m.h);
        let mut img = m.clone();
        loop {
            let mut changed = false;
            for step in 0..2 {
                let mut remove = Vec::new();
                for y in 1..h.saturating_sub(1) {
                    for x in 1..w.saturating_sub(1) {
                        if !img.at(x, y) {
                            continue;
                        }
                        // P2..P9 clockwise from north.
                        let p = [
                            img.at(x, y - 1),
                            img.at(x + 1, y - 1),
                            img.at(x + 1, y),
                            img.at(x + 1, y + 1),
                            img.at(x, y + 1),
                            img.at(x - 1, y + 1),
                            img.at(x - 1, y),
                            img.at(x - 1, y - 1),
                        ];
                        let b = p.iter().filter(|t| **t).count();
                        if !(2..=6).contains(&b) {
                            continue;
                        }
                        let a = (0..8).filter(|&i| !p[i] && p[(i + 1) % 8]).count();
                        if a != 1 {
                            continue;
                        }
                        let (c1, c2) = if step == 0 {
                            (p[0] && p[2] && p[4], p[2] && p[4] && p[6])
                        } else {
                            (p[0] && p[2] && p[6], p[0] && p[4] && p[6])
                        };
                        if c1 || c2 {
                            continue;
                        }
                        remove.push(y * w + x);
                    }
                }
                if !remove.is_empty() {
                    changed = true;
                    for i in remove {
                        img.v[i] = false;
                    }
                }
            }
            if !changed {
                break;
            }
        }
        img
    }


    #[test]
    fn same_skeleton_as_the_full_scan() {
        // Thick diagonal and straight bands, a ring, and noise: the shapes the
        // wall detector makes, and some it does not.
        let (w, h) = (160, 120);
        let mut seed = 12345u64;
        let mut rnd = || { seed = seed.wrapping_mul(6364136223846793005).wrapping_add(1442695040888963407); (seed >> 33) as u32 };
        let mut m = Mask::new(w, h, false);
        for y in 0..h { for x in 0..w {
            let d = (x as f32 * 0.6 + y as f32 * 0.8 - 60.0).abs();
            let r = ((x as f32 - 110.0).powi(2) + (y as f32 - 40.0).powi(2)).sqrt();
            let on = d < 9.0 || (y > 80 && y < 96 && x > 10 && x < 140) || (r > 14.0 && r < 26.0)
                || rnd() % 37 == 0;
            m.v[y * w + x] = on;
        }}
        assert_eq!(skeletonize(&m).v, skeletonize_reference(&m).v);
        // And an empty and a full mask.
        let e = Mask::new(w, h, false);
        assert_eq!(skeletonize(&e).v, skeletonize_reference(&e).v);
        let f = Mask::new(w, h, true);
        assert_eq!(skeletonize(&f).v, skeletonize_reference(&f).v);
    }
}

#[cfg(test)]
mod fast_tests {
    use super::*;

    // The versions the faster ones replaced, kept to check them against.
    /// 4-connected labelling, matching `scipy.ndimage.label`'s default structure.
    /// Returns (labels, count); label 0 is background.
    fn label_reference(m: &Mask) -> (Vec<u32>, usize) {
        let (w, h) = (m.w, m.h);
        let mut lab = vec![0u32; w * h];
        let mut n = 0u32;
        let mut stack: Vec<usize> = Vec::new();
        for start in 0..w * h {
            if !m.v[start] || lab[start] != 0 {
                continue;
            }
            n += 1;
            lab[start] = n;
            stack.push(start);
            while let Some(p) = stack.pop() {
                let (x, y) = (p % w, p / w);
                let push = |q: usize, stack: &mut Vec<usize>, lab: &mut Vec<u32>| {
                    if m.v[q] && lab[q] == 0 {
                        lab[q] = n;
                        stack.push(q);
                    }
                };
                if x > 0 { push(p - 1, &mut stack, &mut lab) }
                if x + 1 < w { push(p + 1, &mut stack, &mut lab) }
                if y > 0 { push(p - w, &mut stack, &mut lab) }
                if y + 1 < h { push(p + w, &mut stack, &mut lab) }
            }
        }
        (lab, n as usize)
    }

    fn dilate_reference(m: &Mask, se: &[(isize, isize)]) -> Mask {
        // Gathered rather than scattered, so rows can be filled independently:
        // a pixel is set if any pixel of the reflected element is.
        let mut out = Mask::new(m.w, m.h, false);
        let (w, h) = (m.w as isize, m.h as isize);
        out.v.par_chunks_mut(m.w).enumerate().for_each(|(y, row)| {
            crate::gen::throttle::gate();
            for x in 0..m.w {
                row[x] = se.iter().any(|(dx, dy)| {
                    let (nx, ny) = (x as isize - dx, y as isize - dy);
                    nx >= 0 && ny >= 0 && nx < w && ny < h && m.v[ny as usize * m.w + nx as usize]
                });
            }
        });
        out
    }

    fn erode_reference(m: &Mask, se: &[(isize, isize)]) -> Mask {
        let mut out = Mask::new(m.w, m.h, false);
        let (w, h) = (m.w as isize, m.h as isize);
        out.v.par_chunks_mut(m.w).enumerate().for_each(|(y, row)| {
            crate::gen::throttle::gate();
            for x in 0..m.w {
                let mut all = true;
                for (dx, dy) in se {
                    let (nx, ny) = (x as isize + dx, y as isize + dy);
                    // scipy's binary_erosion pads with border_value = 0, so a pixel
                    // whose structuring element hangs off the frame is eroded.
                    if nx < 0 || ny < 0 || nx >= w || ny >= h || !m.at(nx as usize, ny as usize) {
                        all = false;
                        break;
                    }
                }
                row[x] = all;
            }
        });
        out
    }


    fn random_mask(w: usize, h: usize, seed: u64, density: u32) -> Mask {
        let mut s = seed;
        let mut m = Mask::new(w, h, false);
        for i in 0..w * h {
            s = s.wrapping_mul(6364136223846793005).wrapping_add(1442695040888963407);
            m.v[i] = ((s >> 33) as u32 % 100) < density;
        }
        // Some solid shapes as well as speckle, so components and holes of
        // every size turn up.
        for y in 0..h { for x in 0..w {
            if (x / 13 + y / 9) % 3 == 0 && (x + 2 * y) % 31 > 4 { m.v[y * w + x] = true }
        }}
        m
    }

    #[test]
    fn label_matches_flood_fill() {
        for (seed, density) in [(1, 10), (2, 45), (3, 70), (4, 0), (5, 100)] {
            let m = random_mask(97, 61, seed, density);
            assert_eq!(label(&m), label_reference(&m), "seed {seed}");
        }
        let one_row = random_mask(50, 1, 9, 50);
        assert_eq!(label(&one_row), label_reference(&one_row));
    }

    #[test]
    fn morphology_matches_per_pixel() {
        let lopsided: Vec<(isize, isize)> = vec![(0, 0), (2, -1), (-3, 1), (1, 3)];
        for (seed, density) in [(11, 15), (12, 50), (13, 85)] {
            let m = random_mask(83, 57, seed, density);
            for se in [disk(1), disk(2), disk(3), lopsided.clone()] {
                assert_eq!(dilate(&m, &se).v, dilate_reference(&m, &se).v, "dilate seed {seed}");
                assert_eq!(erode(&m, &se).v, erode_reference(&m, &se).v, "erode seed {seed}");
            }
        }
        // An element wider than the image erodes everything.
        let m = random_mask(5, 4, 3, 90);
        assert_eq!(erode(&m, &disk(6)).v, erode_reference(&m, &disk(6)).v);
        assert_eq!(dilate(&m, &disk(6)).v, dilate_reference(&m, &disk(6)).v);
    }
}

#[cfg(test)]
mod quantile_tests {
    use super::*;

    #[test]
    fn quantiles_match_sorting() {
        let mut seed = 7u64;
        let v: Vec<f32> = (0..10007).map(|i| {
            seed = seed.wrapping_mul(6364136223846793005).wrapping_add(1442695040888963407);
            if i % 97 == 0 { f32::NAN } else { ((seed >> 40) as f32 / 1000.0).round() }
        }).collect();
        let s = sorted_copy(&v);
        let qs = [0.0, 0.0005, 0.03, 0.25, 0.5, 0.75, 0.97, 0.9995, 1.0];
        let got = quantiles(&v, &qs);
        for (q, g) in qs.iter().zip(&got) {
            assert_eq!(g.to_bits(), quantile(&s, *q).to_bits(), "q {q}");
        }
        assert!(quantiles(&[], &[0.5])[0].is_nan());
    }
}
