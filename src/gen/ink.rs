//! The inked-parchment treatment: flat cream floor, heavy outline, hatched
//! rock surround, offset shoreline rings.
//!
//! Style constants here are in PIXELS and never scale with resolution. Every
//! zoom level is viewed at 100%, so a coastline must stay ~2px whether it is
//! drawn at 0.5 or 16 pixels per world unit. Only the geometry thresholds in
//! `analyze` are world-constant, and that split is what lets real detail
//! emerge as you zoom in rather than the same picture getting blurrier.

use super::analyze::{analyze, Analysis};
use super::grid::*;
use rayon::prelude::*;

pub struct Rendered {
    pub img: Rgb,
    /// Dry land, in raster space -- used to drop props standing in water.
    pub dry: Mask,
}

pub struct Rgb {
    pub w: usize,
    pub h: usize,
    pub v: Vec<[f32; 3]>,
}

fn hex(h: u32) -> [f32; 3] {
    [
        ((h >> 16) & 0xff) as f32 / 255.0,
        ((h >> 8) & 0xff) as f32 / 255.0,
        (h & 0xff) as f32 / 255.0,
    ]
}

const PAPER: u32 = 0xefe6cf;
const FLOOR: u32 = 0xfaf4e4;
const INK: u32 = 0x1d1a16;
const WATER: u32 = 0xc3d4d7;
const DEEP: u32 = 0x9fb8bd;
const SHADE: u32 = 0xc3b291;

/// Round off raster stair-stepping so outlines read as drawn curves.
fn soften(m: &Mask, sigma: f32) -> Mask {
    let g = Grid { w: m.w, h: m.h, v: m.v.iter().map(|b| if *b { 1.0 } else { 0.0 }).collect() };
    let b = gaussian(&g, sigma);
    Mask { w: m.w, h: m.h, v: b.v.iter().map(|t| *t > 0.5).collect() }
}

fn drop_small(m: &Mask, min_px: usize) -> Mask {
    let (lab, n) = label(m);
    if n == 0 {
        return m.clone();
    }
    let sz = component_sizes(&lab, n);
    let mut out = m.clone();
    for (i, l) in lab.iter().enumerate() {
        if *l != 0 && sz[*l as usize] < min_px {
            out.v[i] = false;
        }
    }
    out
}

fn normalize01(g: &Grid) -> Grid {
    let mut lo = f32::MAX;
    let mut hi = f32::MIN;
    for t in &g.v {
        lo = lo.min(*t);
        hi = hi.max(*t);
    }
    let span = (hi - lo).max(1e-6);
    Grid { w: g.w, h: g.h, v: g.v.iter().map(|t| (t - lo) / span).collect() }
}

pub struct InkOptions {
    pub ppu: f64,
    pub step_thresh: f32,
    pub sea: Option<f32>,
    pub seed: u32,
}

/// Render a height raster to the finished sheet. Row 0 of the output is NORTH,
/// matching the tile convention (the raster's row 0 is south).
pub fn render(height: &Grid, edges: &[f32], o: &InkOptions) -> Rendered {
    let _t = super::Timer::start("render (total)");
    let a: Analysis = analyze(height, o.ppu, edges, o.step_thresh);
    let (w, h) = (height.w, height.h);
    let ppu = o.ppu;

    let paper = hex(PAPER);
    let floor = hex(FLOOR);
    let inkc = hex(INK);
    let waterc = hex(WATER);
    let deepc = hex(DEEP);
    let shadec = hex(SHADE);

    let grain = normalize(&gaussian(&noise(w, h, o.seed), 0.8));
    let mut img: Vec<[f32; 3]> = (0..w * h)
        .map(|i| {
            let g = grain.v[i];
            [
                (paper[0] * (1.0 + 0.030 * g)).clamp(0.0, 1.0),
                (paper[1] * (1.0 + 0.030 * g)).clamp(0.0, 1.0),
                (paper[2] * (1.0 + 0.030 * g)).clamp(0.0, 1.0),
            ]
        })
        .collect();

    // Specks of stray geometry read as dirt on the sheet, and were never
    // informative at this scale.
    let _t1 = super::Timer::start("  land_s");
    let land_s = drop_small(&soften(&a.land, 1.2), ((90.0 * ppu * ppu) as usize).max(120));

    // Water is simply floor below the ocean surface: zones place their water
    // objects at sea level, so the coastline is a height threshold rather than
    // a separate mesh to find.
    let mut water = match o.sea {
        Some(sea) => Mask {
            w,
            h,
            v: (0..w * h).map(|i| land_s.v[i] && a.hf.v[i] < sea).collect(),
        },
        None => Mask::new(w, h, false),
    };
    let mut dry = land_s.andnot(&water);
    if water.any() {
        // Isolated dry specks out at sea are single pixels of ocean floor
        // poking above the surface. They are not sandbars, and because the
        // wave rings measure distance from dry land each one became a bullseye.
        let keep = drop_small(&dry, ((45.0 * ppu * ppu) as usize).max(60));
        water = water.or(&dry.andnot(&keep));
        dry = keep;
    }
    for i in 0..w * h {
        if land_s.v[i] {
            img[i] = floor;
        }
    }

    // How much elevation shading this zone earns.
    //
    // The interquartile range of walkable heights measures how much of the map
    // sits at DIFFERENT heights -- whether relief carries information here.
    // Architecture has flat floors at a few discrete levels and wants ink to
    // dominate; terrain varies continuously and is unreadable without
    // terraces. Total relief does NOT separate the two, because it is driven
    // by one tall outlier rather than by the spread of the ground you walk on.
    drop(_t1);
    let _t2 = super::Timer::start("  iqr sort");
    let hv = sorted_copy(&a.hf.select(&a.land));
    let iqr = if hv.is_empty() { 0.0 } else { quantile(&hv, 0.75) - quantile(&hv, 0.25) };
    let elev_w = (((iqr - 20.0) / 100.0) as f32).clamp(0.0, 1.0);
    let amount = 0.08 + 0.52 * elev_w;

    let nb = a.layers.iter().map(|(k, _)| *k).max().unwrap_or(0) + 1;
    drop(_t2);
    let _t3 = super::Timer::start("  soften layers");
    // Each band's smoothing is independent of the others.
    let softened: Vec<(usize, Mask)> =
        a.layers.par_iter().map(|(k, m)| (*k, soften(m, 1.2))).collect();
    for (k, m) in &softened {
        let f = *k as f32 / (nb.saturating_sub(1)).max(1) as f32;
        for i in 0..w * h {
            if m.v[i] && dry.v[i] {
                for c in 0..3 {
                    img[i][c] = floor[c] + (shadec[c] - floor[c]) * (1.0 - f) * amount;
                }
            }
        }
    }
    if let (Some(sea), true) = (o.sea, water.any()) {
        let wh: Vec<f32> = a.hf.select(&water);
        let span = {
            let s = sorted_copy(&wh);
            if s.is_empty() { 1.0 } else { (s[s.len() - 1] - s[0]).max(1.0) }
        };
        for i in 0..w * h {
            if water.v[i] {
                let d = ((sea - a.hf.v[i]) / span).clamp(0.0, 1.0);
                for c in 0..3 {
                    img[i][c] = waterc[c] + (deepc[c] - waterc[c]) * d;
                }
            }
        }
    }

    drop(_t3);
    let mut ink = vec![0.0f32; w * h];
    let bump = |ink: &mut Vec<f32>, i: usize, v: f32| {
        if v > ink[i] {
            ink[i] = v;
        }
    };

    // --- elevation contours ------------------------------------------------
    // Terraced fills show broad relief; isolines show its SHAPE, which is what
    // makes terrain legible. Only drawn where relief justifies it.
    let _t4 = super::Timer::start("  contours");
    if elev_w > 0.25 && dry.any() {
        let hv2 = sorted_copy(&a.hf.select(&dry));
        let lo = quantile(&hv2, 0.03);
        let span = quantile(&hv2, 0.97) - lo;
        if span > 1e-6 {
            let step = span / 14.0;
            let q: Vec<f32> = a.hf.v.iter().map(|t| ((t - lo) / step).floor()).collect();
            for y in 0..h {
                for x in 0..w {
                    let i = y * w + x;
                    if !dry.v[i] {
                        continue;
                    }
                    let edge = (y > 0 && q[i] != q[i - w]) || (x > 0 && q[i] != q[i - 1]);
                    if edge {
                        bump(&mut ink, i, 0.20 + 0.18 * elev_w);
                    }
                }
            }
        }
    }

    // --- rock hatching outside the walls ----------------------------------
    drop(_t4);
    let dist = { let _t = super::Timer::start("  edt(hatch)"); edt(&land_s).0 };
    const REACH: f32 = 26.0; // hatch reach, px
    const SP: usize = 5; // hatch spacing, px
    let runs = normalize01(&gaussian_xy(&noise(w, h, o.seed ^ 0x51ED), 3.0, 0.6));
    for y in 0..h {
        for x in 0..w {
            let i = y * w + x;
            let d = dist.v[i];
            if d <= 0.0 || d > REACH {
                continue;
            }
            let fade = (1.0 - d / REACH).clamp(0.0, 1.0);
            // The stroke BREAKS are what make this read as hatching rather
            // than a gradient, so the mask gates whole run-lengths along each
            // line instead of individual pixels.
            // Two directions, the second phase-offset by half the spacing.
            // rem_euclid, not a +h fudge: x-y goes negative, and adding the
            // image height shifts the phase by h % SP, which breaks alignment
            // on any raster whose height is not a multiple of the spacing.
            let sp = SP as isize;
            for ang in [
                (x as isize + y as isize).rem_euclid(sp),
                (x as isize - y as isize + sp / 2).rem_euclid(sp),
            ] {
                if ang == 0 && runs.v[i] < 0.25 + 0.75 * fade {
                    bump(&mut ink, i, (0.30 + 0.70 * fade).clamp(0.0, 1.0));
                }
            }
            // --- offset shoreline rings ---
            for (k, lvl) in [(1.0f32, 0.5f32), (2.0, 0.28)] {
                if (d - k * 7.0).abs() < 0.7 {
                    bump(&mut ink, i, lvl);
                }
            }
        }
    }

    // --- wave lines stepping out from the shore ---------------------------
    // Only near the shore: distance-from-shore peaks in the middle of a large
    // body of water, so unbounded rings become concentric bullseyes around
    // those maxima instead of lines following the coast.
    if water.any() {
        let dcoast = edt(&dry).0;
        for i in 0..w * h {
            if !water.v[i] {
                continue;
            }
            for k in 1..=3 {
                if (dcoast.v[i] - k as f32 * 9.0).abs() < 0.8 {
                    bump(&mut ink, i, 0.32 - 0.07 * k as f32);
                }
            }
        }
    }

    let _t5 = super::Timer::start("  outlines+walls");
    // --- architecture + heavy outline -------------------------------------
    for (k, m) in &softened {
        if *k == 0 {
            continue;
        }
        let o = outline(m, 1);
        for i in 0..w * h {
            if o.v[i] {
                bump(&mut ink, i, 0.30);
            }
        }
    }
    for i in 0..w * h {
        if a.walls.v[i] && dry.v[i] {
            ink[i] = 0.9;
        }
    }

    if water.any() {
        let coast = outline(&dry, 3);
        for i in 0..w * h {
            if coast.v[i] {
                ink[i] = 1.0;
            }
        }
        // The sea does not end -- the ocean-floor MESH ends, at a hard
        // rectangular border. Fade the water out where it meets the void so
        // the sheet reads as "ocean continues" rather than a cut edge.
        // World-scaled and clamped: a fixed pixel fade is fine on a big sheet
        // but swallows the entire ocean at low zoom, where the whole zone is
        // only a few hundred pixels across.
        let fade_px = (70.0 * ppu).clamp(10.0, 90.0) as f32;
        let dl = distance_to_background(&land_s);
        let fade: Vec<f32> = dl.v.iter().map(|t| (t / fade_px).clamp(0.0, 1.0)).collect();
        for i in 0..w * h {
            if water.v[i] && fade[i] < 1.0 {
                for c in 0..3 {
                    img[i][c] = img[i][c] * fade[i] + paper[c] * (1.0 - fade[i]);
                }
            }
            if water.v[i] {
                ink[i] *= fade[i];
            }
        }
        let o2 = outline(&land_s, 2);
        for i in 0..w * h {
            if o2.v[i] {
                bump(&mut ink, i, 0.65 * fade[i]);
            }
        }
    } else {
        let o = outline(&land_s, 3);
        for i in 0..w * h {
            if o.v[i] {
                ink[i] = 1.0;
            }
        }
    }

    drop(_t5);
    let _t6 = super::Timer::start("  pressure+composite");
    // Pen pressure, then composite.
    let press = normalize01(&smooth_noise(w, h, 5.0, o.seed ^ 0x9D21));
    img.par_iter_mut()
        .zip(press.v.par_iter())
        .zip(ink.par_iter())
        .for_each(|((px, pr), ik)| {
            let k = (ik * (0.80 + 0.30 * pr)).clamp(0.0, 1.0);
            for c in 0..3 {
                px[c] = px[c] * (1.0 - k) + inkc[c] * k;
            }
        });

    // A gentle waver so the linework is not mechanically straight.
    drop(_t6);
    let _tw = super::Timer::start("  waver warp");
    let mut planes: Vec<Grid> = (0..3)
        .map(|c| Grid { w, h, v: img.iter().map(|p| p[c]).collect() })
        .collect();
    let mk = |seed: u32| {
        let b = smooth_noise(w, h, 26.0, seed);
        // Centre before scaling. The field is a displacement, so any DC offset
        // that survives is a constant shift of the whole sheet, not a wobble --
        // and dividing by the small standard deviation of a heavily blurred
        // field magnifies that offset into tens of pixels.
        let mean = b.v.iter().sum::<f32>() / b.v.len() as f32;
        let sd = (b.v.iter().map(|t| (t - mean).powi(2)).sum::<f32>() / b.v.len() as f32)
            .sqrt()
            + 1e-6;
        Grid { w, h, v: b.v.iter().map(|t| (t - mean) * 0.7 / sd).collect() }
    };
    let (dx, dy) = (mk(o.seed ^ 0x1111), mk(o.seed ^ 0x2222));
    planes.par_iter_mut().for_each(|p| *p = warp(p, &dx, &dy));

    // Row 0 of the raster is south; the sheet wants north at the top.
    let mut out = Vec::with_capacity(w * h);
    for y in (0..h).rev() {
        for x in 0..w {
            let i = y * w + x;
            out.push([
                planes[0].v[i].clamp(0.0, 1.0),
                planes[1].v[i].clamp(0.0, 1.0),
                planes[2].v[i].clamp(0.0, 1.0),
            ]);
        }
    }
    Rendered { img: Rgb { w, h, v: out }, dry }
}

// ---------------------------------------------------------- prop symbols

const PROP_INK: [f32; 3] = [46.0 / 255.0, 38.0 / 255.0, 28.0 / 255.0];

impl Rgb {
    fn plot(&mut self, x: f32, y: f32, c: [f32; 3]) {
        if x < 0.0 || y < 0.0 {
            return;
        }
        let (xi, yi) = (x as usize, y as usize);
        if xi < self.w && yi < self.h {
            self.v[yi * self.w + xi] = c;
        }
    }
    fn line(&mut self, x0: f32, y0: f32, x1: f32, y1: f32, c: [f32; 3]) {
        let steps = ((x1 - x0).abs().max((y1 - y0).abs()).ceil() as usize).max(1);
        for i in 0..=steps {
            let t = i as f32 / steps as f32;
            self.plot(x0 + (x1 - x0) * t, y0 + (y1 - y0) * t, c);
        }
    }
    fn ellipse(&mut self, cx: f32, cy: f32, r: f32, c: [f32; 3]) {
        let steps = ((r * 8.0) as usize).max(12);
        for i in 0..steps {
            let a = i as f32 / steps as f32 * std::f32::consts::TAU;
            self.plot(cx + r * a.cos(), cy + r * a.sin(), c);
        }
    }
    fn fill_tri(&mut self, p: [(f32, f32); 3], c: [f32; 3]) {
        let miny = p.iter().map(|q| q.1).fold(f32::MAX, f32::min).floor().max(0.0) as usize;
        let maxy = (p.iter().map(|q| q.1).fold(f32::MIN, f32::max).ceil() as usize).min(self.h);
        for y in miny..maxy {
            let yc = y as f32 + 0.5;
            let mut xs: Vec<f32> = Vec::new();
            for i in 0..3 {
                let (a, b) = (p[i], p[(i + 1) % 3]);
                if (a.1 <= yc) != (b.1 <= yc) {
                    xs.push(a.0 + (yc - a.1) / (b.1 - a.1) * (b.0 - a.0));
                }
            }
            xs.sort_by(|m, n| m.partial_cmp(n).unwrap());
            if xs.len() >= 2 {
                let (x0, x1) = (xs[0].max(0.0) as usize, (xs[1].ceil() as usize).min(self.w));
                for x in x0..x1 {
                    self.v[y * self.w + x] = c;
                }
            }
        }
    }
}

/// Palm, scrub, tent and campfire symbols at their real positions. Drawn on
/// the finished sheet, where row 0 is north.
pub fn draw_props(
    img: &mut Rgb,
    extent: (f64, f64, f64, f64),
    ppu: f64,
    props: &std::collections::BTreeMap<String, Vec<(f64, f64)>>,
) {
    let (minx, maxx, minz, maxz) = extent;
    let (w, h) = (img.w as f64, img.h as f64);
    let to_px = |x: f64, z: f64| {
        (
            ((x - minx) / (maxx - minx).max(1e-9) * w) as f32,
            ((maxz - z) / (maxz - minz).max(1e-9) * h) as f32,
        )
    };
    let r = (4.2 * ppu.min(2.0)).max(3.0) as f32;
    let ink = PROP_INK;

    for (x, z) in props.get("tree").map(|v| v.as_slice()).unwrap_or(&[]) {
        let (px, py) = to_px(*x, *z);
        img.line(px, py, px, py - r * 1.5, ink); // trunk
        for a in [-150.0f32, -115.0, -65.0, -30.0] {
            let t = a.to_radians();
            img.line(px, py - r * 1.5, px + r * 1.15 * t.cos(), py - r * 1.5 + r * 1.15 * t.sin(), ink);
        }
    }
    for (x, z) in props.get("scrub").map(|v| v.as_slice()).unwrap_or(&[]) {
        let (px, py) = to_px(*x, *z);
        img.ellipse(px, py, r * 0.5, ink);
    }
    for (x, z) in props.get("tent").map(|v| v.as_slice()).unwrap_or(&[]) {
        let (px, py) = to_px(*x, *z);
        let p = [(px, py - r * 1.5), (px + r, py + r * 0.5), (px - r, py + r * 0.5)];
        img.fill_tri(p, [238.0 / 255.0, 229.0 / 255.0, 206.0 / 255.0]);
        for i in 0..3 {
            let (a, b) = (p[i], p[(i + 1) % 3]);
            img.line(a.0, a.1, b.0, b.1, ink);
        }
        img.line(px, py - r * 1.5, px, py + r * 0.5, ink);
    }
    for (x, z) in props.get("fire").map(|v| v.as_slice()).unwrap_or(&[]) {
        let (px, py) = to_px(*x, *z);
        for a in [-90.0f32, -140.0, -40.0] {
            let t = a.to_radians();
            img.line(px, py, px + r * 0.8 * t.cos(), py + r * 0.8 * t.sin(), ink);
        }
    }
}

/// Keep only props standing on dry land: scene objects include ones placed
/// over water or outside the playable area.
pub fn props_on_land(
    props: &std::collections::BTreeMap<String, Vec<(f64, f64)>>,
    dry: &Mask,
    extent: (f64, f64, f64, f64),
) -> std::collections::BTreeMap<String, Vec<(f64, f64)>> {
    let (minx, maxx, minz, maxz) = extent;
    let mut out: std::collections::BTreeMap<String, Vec<(f64, f64)>> = Default::default();
    for (kind, pts) in props {
        let keep: Vec<(f64, f64)> = pts
            .iter()
            .copied()
            .filter(|(x, z)| {
                let c = ((x - minx) / (maxx - minx).max(1e-9) * dry.w as f64) as isize;
                // dry is in raster space, row 0 = south
                let rr = ((z - minz) / (maxz - minz).max(1e-9) * dry.h as f64) as isize;
                c >= 0 && rr >= 0 && (c as usize) < dry.w && (rr as usize) < dry.h
                    && dry.at(c as usize, rr as usize)
            })
            .collect();
        if !keep.is_empty() {
            out.insert(kind.clone(), keep);
        }
    }
    out
}
