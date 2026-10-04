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

pub fn sorted_copy(v: &[f32]) -> Vec<f32> {
    let mut s: Vec<f32> = v.iter().copied().filter(|t| t.is_finite()).collect();
    s.sort_by(|a, b| a.partial_cmp(b).unwrap());
    s
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
    let kx = gauss_kernel(sigma_x);
    let ky = gauss_kernel(sigma_y);
    let (w, h) = (g.w as isize, g.h as isize);
    let mut tmp = Grid::new(g.w, g.h, 0.0);
    let rx = (kx.len() / 2) as isize;
    for y in 0..g.h {
        for x in 0..g.w {
            let mut acc = 0.0;
            for (j, kv) in kx.iter().enumerate() {
                let sx = reflect(x as isize + j as isize - rx, w);
                acc += g.at(sx, y) * kv;
            }
            tmp.set(x, y, acc);
        }
    }
    let mut out = Grid::new(g.w, g.h, 0.0);
    let ry = (ky.len() / 2) as isize;
    for y in 0..g.h {
        for x in 0..g.w {
            let mut acc = 0.0;
            for (j, kv) in ky.iter().enumerate() {
                let sy = reflect(y as isize + j as isize - ry, h);
                acc += tmp.at(x, sy) * kv;
            }
            out.set(x, y, acc);
        }
    }
    out
}

pub fn gaussian(g: &Grid, sigma: f32) -> Grid {
    gaussian_xy(g, sigma, sigma)
}

/// Bilinear upsample to an exact target size.
pub fn upsample(g: &Grid, w: usize, h: usize) -> Grid {
    let mut out = Grid::new(w, h, 0.0);
    let sx = (g.w.max(2) - 1) as f32 / (w.max(2) - 1) as f32;
    let sy = (g.h.max(2) - 1) as f32 / (h.max(2) - 1) as f32;
    for y in 0..h {
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
            out.set(x, y, v);
        }
    }
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
    let mut tmp = Grid::new(g.w, g.h, 0.0);
    let norm = 1.0 / (2 * r + 1) as f32;
    for y in 0..g.h {
        let mut acc = 0.0f32;
        for i in -(r as isize)..=(r as isize) {
            acc += g.at(reflect(i, g.w as isize), y);
        }
        for x in 0..g.w {
            tmp.set(x, y, acc * norm);
            let out_x = reflect(x as isize - r as isize, g.w as isize);
            let in_x = reflect(x as isize + r as isize + 1, g.w as isize);
            acc += g.at(in_x, y) - g.at(out_x, y);
        }
    }
    let mut out = Grid::new(g.w, g.h, 0.0);
    for x in 0..g.w {
        let mut acc = 0.0f32;
        for i in -(r as isize)..=(r as isize) {
            acc += tmp.at(x, reflect(i, g.h as isize));
        }
        for y in 0..g.h {
            out.set(x, y, acc * norm);
            let out_y = reflect(y as isize - r as isize, g.h as isize);
            let in_y = reflect(y as isize + r as isize + 1, g.h as isize);
            acc += tmp.at(x, in_y) - tmp.at(x, out_y);
        }
    }
    out
}

// ------------------------------------------------------ distance transform

/// Exact squared Euclidean distance transform (Felzenszwalb & Huttenlocher),
/// plus the index of the nearest true pixel -- scipy's `return_indices`.
///
/// Distance is measured to the nearest **true** pixel of `src`.
pub fn edt(src: &Mask) -> (Grid, Vec<u32>) {
    let (w, h) = (src.w, src.h);
    const INF: f32 = 1e20;
    let mut f = vec![INF; w * h];
    let mut idx = vec![u32::MAX; w * h];
    for i in 0..w * h {
        if src.v[i] {
            f[i] = 0.0;
            idx[i] = i as u32;
        }
    }
    // columns, then rows: the 1-D transform is applied along each axis.
    let mut d = vec![0.0f32; w.max(h)];
    let mut di = vec![0u32; w.max(h)];
    let mut vpos = vec![0usize; w.max(h)];
    let mut z = vec![0.0f32; w.max(h) + 1];

    let mut pass = |n: usize, stride: usize, base: usize,
                    f: &mut Vec<f32>, idx: &mut Vec<u32>| {
        for q in 0..n {
            d[q] = f[base + q * stride];
            di[q] = idx[base + q * stride];
        }
        let mut k = 0usize;
        vpos[0] = 0;
        z[0] = -INF;
        z[1] = INF;
        for q in 1..n {
            loop {
                let p = vpos[k];
                let s = ((d[q] + (q * q) as f32) - (d[p] + (p * p) as f32))
                    / (2.0 * q as f32 - 2.0 * p as f32);
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
            while z[k + 1] < q as f32 {
                k += 1;
            }
            let p = vpos[k];
            let dq = (q as f32 - p as f32).powi(2) + d[p];
            f[base + q * stride] = dq;
            idx[base + q * stride] = di[p];
        }
    };

    for x in 0..w {
        pass(h, w, x, &mut f, &mut idx);
    }
    for y in 0..h {
        pass(w, 1, y * w, &mut f, &mut idx);
    }
    let g = Grid { w, h, v: f.iter().map(|t| t.max(0.0).sqrt()).collect() };
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
    let mut out = Mask::new(m.w, m.h, false);
    let (w, h) = (m.w as isize, m.h as isize);
    for y in 0..m.h {
        for x in 0..m.w {
            if !m.at(x, y) {
                continue;
            }
            for (dx, dy) in se {
                let (nx, ny) = (x as isize + dx, y as isize + dy);
                if nx >= 0 && ny >= 0 && nx < w && ny < h {
                    out.v[ny as usize * m.w + nx as usize] = true;
                }
            }
        }
    }
    out
}

pub fn erode(m: &Mask, se: &[(isize, isize)]) -> Mask {
    let mut out = Mask::new(m.w, m.h, false);
    let (w, h) = (m.w as isize, m.h as isize);
    for y in 0..m.h {
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
            out.v[y * m.w + x] = all;
        }
    }
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
/// Returns (labels, count); label 0 is background.
pub fn label(m: &Mask) -> (Vec<u32>, usize) {
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
    let mut tmp = Grid::new(g.w, g.h, 0.0);
    let mut row = vec![0.0f32; g.w + 2 * r];
    let mut pre = vec![0.0f32; g.w + 2 * r];
    let mut suf = vec![0.0f32; g.w + 2 * r];
    for y in 0..g.h {
        for i in 0..row.len() {
            let sx = reflect(i as isize - r as isize, g.w as isize);
            row[i] = g.at(sx, y);
        }
        line_rank(&row, win, want_max, &mut pre, &mut suf);
        for x in 0..g.w {
            tmp.set(x, y, pre[x]);
        }
    }
    let mut out = Grid::new(g.w, g.h, 0.0);
    let mut col = vec![0.0f32; g.h + 2 * r];
    let mut pre2 = vec![0.0f32; g.h + 2 * r];
    let mut suf2 = vec![0.0f32; g.h + 2 * r];
    for x in 0..g.w {
        for i in 0..col.len() {
            let sy = reflect(i as isize - r as isize, g.h as isize);
            col[i] = tmp.at(x, sy);
        }
        line_rank(&col, win, want_max, &mut pre2, &mut suf2);
        for y in 0..g.h {
            out.set(x, y, pre2[y]);
        }
    }
    out
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
    let mut buf = [0.0f32; 9];
    for y in 0..g.h {
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
            out.set(x, y, s[n / 2]);
        }
    }
    out
}

/// Central-difference gradient, as numpy's `gradient`: (d/dy, d/dx).
pub fn gradient(g: &Grid) -> (Grid, Grid) {
    let mut gy = Grid::new(g.w, g.h, 0.0);
    let mut gx = Grid::new(g.w, g.h, 0.0);
    for y in 0..g.h {
        for x in 0..g.w {
            let a = g.at(x, if y + 1 < g.h { y + 1 } else { y });
            let b = g.at(x, y.saturating_sub(1));
            let span = if y + 1 < g.h && y > 0 { 2.0 } else { 1.0 };
            gy.set(x, y, (a - b) / span);
            let c = g.at(if x + 1 < g.w { x + 1 } else { x }, y);
            let d = g.at(x.saturating_sub(1), y);
            let span = if x + 1 < g.w && x > 0 { 2.0 } else { 1.0 };
            gx.set(x, y, (c - d) / span);
        }
    }
    (gy, gx)
}

// ------------------------------------------------------------ skeletonize

/// Zhang-Suen thinning -- skimage's default 2-D skeletonize.
///
/// The wall detector flags a band as wide as its window, so the raw mask is a
/// slab tens of pixels across. Reducing it to a centreline is what lets a wall
/// be drawn at a fixed weight regardless of zoom.
pub fn skeletonize(m: &Mask) -> Mask {
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

// --------------------------------------------------------------- warping

/// Bilinear resample at arbitrary coordinates, clamped at the edges --
/// `map_coordinates(order=1, mode="nearest")`.
pub fn warp(g: &Grid, dx: &Grid, dy: &Grid) -> Grid {
    let mut out = Grid::new(g.w, g.h, 0.0);
    for y in 0..g.h {
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
            out.set(x, y, v);
        }
    }
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
