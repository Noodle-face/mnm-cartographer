//! The painted style: a full-colour rendering of a zone, an experiment.
//!
//! Golden sand with wind dunes and ripples, water that lightens over the
//! shallows with a foam line at the shore, cracked sandstone on steep and
//! raised ground casting shadows, stone walls where the floor steps sharply,
//! and the zone's palms, bushes, tents and fires.
//!
//! Every texture is computed from WORLD coordinates -- value noise and
//! Voronoi cells keyed on world position -- so each zoom level is the same
//! picture at a different resolution, not a fresh random one. The palette is
//! desert; other kinds of zone would want their own.

use super::grid::{distance_to_background, edt, gaussian_approx, gradient, label, max_filter,
                  min_filter, Grid, Mask};
use super::ink::Rgb;
use super::raster::Bbox;
use rayon::prelude::*;
use std::collections::BTreeMap;

pub struct PaintOptions<'a> {
    pub ppu: f64,
    pub sea: Option<f32>,
    pub bbox: Bbox,
    /// Per pixel (same layout as the height raster), an index into the
    /// table: what the surface there is made of. None paints it all as sand.
    pub classes: Option<(&'a [u16], &'a [MatClass])>,
}

use super::materials::{self as mc, MatClass};

/// The class painted at a world point, from the surface's layers and the
/// ground's slope (rise per world unit).
///
/// The game blends a surface's layers through masks the map does not read,
/// but its terrain materials follow one pattern: a cliff texture in one layer
/// and ground textures (grass, sand, dirt) in the others, the cliff showing
/// on steep ground and the ground layers on the flat. So rock goes where it
/// is steep, the first ground layer covers the rest, and any further layers
/// appear as patches placed by noise -- the right materials in roughly the
/// right places.
fn class_at(t: MatClass, x: f64, z: f64, slope: f32, fallback: u8) -> u8 {
    let layers: Vec<u8> = [t.base, t.blend[0], t.blend[1]].into_iter()
        .filter(|&c| c != mc::UNKNOWN).collect();
    if layers.is_empty() { return fallback }
    let rocky = layers.contains(&mc::ROCK);
    let ground: Vec<u8> = layers.iter().copied().filter(|&c| c != mc::ROCK).collect();
    if ground.is_empty() { return mc::ROCK }
    if rocky && slope > 0.75 { return mc::ROCK }
    if ground.len() > 1 && fbm(x, z, 45.0, 31) > 0.3 { return ground[1] }
    if ground.len() > 2 && fbm(x, z, 60.0, 37) > 0.45 { return ground[2] }
    if rocky && fbm(x, z, 30.0, 39) > 0.62 { return mc::ROCK }
    ground[0]
}

// ------------------------------------------------------------------ noise

fn hash(x: i64, z: i64, seed: u32) -> u32 {
    let mut h = (x as u32).wrapping_mul(0x8da6_b343) ^ (z as u32).wrapping_mul(0xd816_3841)
        ^ seed.wrapping_mul(0xcb1a_b31f);
    h ^= h >> 13;
    h = h.wrapping_mul(0x5bd1_e995);
    h ^ (h >> 15)
}

fn rand01(x: i64, z: i64, seed: u32) -> f32 {
    (hash(x, z, seed) & 0xffffff) as f32 / 0xffffff as f32
}

/// Smooth value noise in [-1, 1] at a world position, `scale` world units
/// per cell.
fn vnoise(x: f64, z: f64, scale: f64, seed: u32) -> f32 {
    let (fx, fz) = (x / scale, z / scale);
    let (ix, iz) = (fx.floor() as i64, fz.floor() as i64);
    let (tx, tz) = ((fx - ix as f64) as f32, (fz - iz as f64) as f32);
    let s = |t: f32| t * t * (3.0 - 2.0 * t);
    let (sx, sz) = (s(tx), s(tz));
    let v = |a, b| rand01(ix + a, iz + b, seed) * 2.0 - 1.0;
    let top = v(0, 0) + (v(1, 0) - v(0, 0)) * sx;
    let bot = v(0, 1) + (v(1, 1) - v(0, 1)) * sx;
    top + (bot - top) * sz
}

fn fbm(x: f64, z: f64, scale: f64, seed: u32) -> f32 {
    let (mut sum, mut amp, mut tot, mut sc) = (0.0, 1.0, 0.0, scale);
    for o in 0..3 {
        sum += vnoise(x, z, sc, seed + o) * amp;
        tot += amp;
        amp *= 0.5;
        sc *= 0.5;
    }
    sum / tot
}

/// Distance to the nearest and second-nearest of a jittered grid of points,
/// `cell` world units apart: the Voronoi cells that crack the rock.
fn voronoi(x: f64, z: f64, cell: f64, seed: u32) -> (f64, f64) {
    let (ix, iz) = ((x / cell).floor() as i64, (z / cell).floor() as i64);
    let (mut d1, mut d2) = (f64::MAX, f64::MAX);
    for dz in -1..=1 {
        for dx in -1..=1 {
            let (cx, cz) = (ix + dx, iz + dz);
            let px = (cx as f64 + rand01(cx, cz, seed) as f64) * cell;
            let pz = (cz as f64 + rand01(cx, cz, seed + 7) as f64) * cell;
            let d = (px - x).hypot(pz - z);
            if d < d1 { d2 = d1; d1 = d } else if d < d2 { d2 = d }
        }
    }
    (d1, d2)
}

// ---------------------------------------------------------------- helpers

/// True where within `r` pixels of a true pixel of `m`.
fn grow(m: &Mask, r: f32) -> Mask {
    let d = edt(m).0;
    Mask { w: m.w, h: m.h, v: d.v.iter().map(|&t| t <= r).collect() }
}

/// True where at least `r` pixels inside `m`.
fn shrink(m: &Mask, r: f32) -> Mask {
    let d = distance_to_background(m);
    Mask { w: m.w, h: m.h, v: d.v.iter().map(|&t| t > r).collect() }
}

fn close(m: &Mask, r: f32) -> Mask { shrink(&grow(m, r), r) }
fn open(m: &Mask, r: f32) -> Mask { grow(&shrink(m, r), r) }

/// Drop connected pieces of `m` smaller than `min` pixels.
fn drop_small(m: &Mask, min: usize) -> Mask {
    let (lab, n) = label(m);
    let mut size = vec![0usize; n + 1];
    for &l in &lab { size[l as usize] += 1 }
    Mask { w: m.w, h: m.h, v: lab.iter().map(|&l| l != 0 && size[l as usize] >= min).collect() }
}

fn blur(g: &Grid, sigma: f32) -> Grid {
    if sigma < 0.3 { g.clone() } else { gaussian_approx(g, sigma) }
}

fn as_grid(m: &Mask) -> Grid {
    Grid { w: m.w, h: m.h, v: m.v.iter().map(|&b| if b { 1.0 } else { 0.0 }).collect() }
}

/// Lambertian shading of a height field (world units) lit from the
/// north-west, in raster space where row 0 is -Z. North is world +X and west
/// +Z, so north-west is up both raster axes: azimuth 45 in raster terms.
fn hillshade(z: &Grid, ppu: f64, alt_deg: f32) -> Grid {
    let (gy, gx) = gradient(z);
    let k = ppu as f32; // per pixel -> per world unit
    let (alt, az) = (alt_deg.to_radians(), 45f32.to_radians());
    let light = [az.sin() * alt.cos(), az.cos() * alt.cos(), alt.sin()]; // (+X, +Z, up)
    let v = gx.v.par_iter().zip(&gy.v).map(|(&dx, &dz)| {
        super::throttle::gate();
        let n = [-dx * k, -dz * k, 1.0];
        let l = (n[0] * n[0] + n[1] * n[1] + 1.0).sqrt();
        ((n[0] * light[0] + n[1] * light[1] + n[2] * light[2]) / l).clamp(0.0, 1.0)
    }).collect();
    Grid { w: z.w, h: z.h, v }
}

fn mix(a: [f32; 3], b: [f32; 3], t: f32) -> [f32; 3] {
    [a[0] + (b[0] - a[0]) * t, a[1] + (b[1] - a[1]) * t, a[2] + (b[2] - a[2]) * t]
}
fn scale(a: [f32; 3], k: f32) -> [f32; 3] { [a[0] * k, a[1] * k, a[2] * k] }
fn rgb(r: u8, g: u8, b: u8) -> [f32; 3] { [r as f32, g as f32, b as f32] }

// ----------------------------------------------------------------- render

pub fn render(height: &Grid, props: &BTreeMap<String, Vec<(f64, f64)>>, o: &PaintOptions<'_>) -> Rgb {
    let _t = super::Timer::start("paint (total)");
    let (w, h, ppu) = (height.w, height.h, o.ppu);
    let p = ppu as f32;
    let px = |world: f64| (world * ppu) as f32;
    let n = w * h;
    let world_of = |i: usize| -> (f64, f64) {
        (o.bbox.0 + ((i % w) as f64 + 0.5) / ppu, o.bbox.2 + ((i / w) as f64 + 0.5) / ppu)
    };

    // ---- heights, filled so slopes are continuous -----------------------
    let have = height.finite();
    let (dist_empty, nearest) = edt(&have);
    let hf = Grid { w, h, v: (0..n).map(|i| {
        let j = nearest[i];
        if j == u32::MAX { 0.0 } else { height.v[j as usize] }
    }).collect() };
    let void = Mask { w, h, v: dist_empty.v.iter().map(|&d| d > px(5.0)).collect() };
    let hs = blur(&hf, px(1.5));

    // ---- what each pixel is made of --------------------------------------
    // Pixels nothing was rasterised at take the class of the nearest that
    // was, as their heights do.
    let class: Vec<u8> = match o.classes {
        None => vec![mc::SAND; n],
        Some((tags, table)) => {
            // Unknown surfaces (placeholders) take the zone's commonest class.
            let mut count = [0usize; 16];
            for &t in tags.iter() {
                if let Some(c) = table.get(t as usize) { count[c.base as usize] += 1 }
            }
            count[mc::UNKNOWN as usize] = 0;
            let fallback = (0..16).max_by_key(|&k| count[k]).filter(|&k| count[k] > 0)
                .unwrap_or(mc::SAND as usize) as u8;
            let (gy, gx) = gradient(&blur(&hf, px(1.2)));
            (0..n).into_par_iter().map(|i| {
                super::throttle::gate();
                let j = nearest[i];
                let t = if j == u32::MAX { u16::MAX } else { tags[j as usize] };
                let (x, z) = world_of(i);
                let slope = gx.v[i].hypot(gy.v[i]) * p;
                match table.get(t as usize) {
                    Some(&tc) => class_at(tc, x, z, slope, fallback),
                    None => fallback,
                }
            }).collect()
        }
    };
    let is = |k: u8| Mask { w, h, v: class.iter().map(|&c| c == k).collect() };

    // ---- land and water --------------------------------------------------
    let sea = o.sea.unwrap_or(f32::MIN);
    // Ground made of water is water, whatever its height.
    let land0 = Mask { w, h, v: (0..n).map(|i| hs.v[i] > sea + 0.3 && !void.v[i] && class[i] != mc::WATER).collect() };
    let land0 = open(&close(&land0, px(3.0).max(1.0)), px(2.0).max(1.0));
    let mut land = drop_small(&land0, (150.0 * ppu * ppu) as usize + 1);
    // Small enclosed water is a dip in the ground, not a lake.
    let holes = drop_small(&land.not(), (120.0 * ppu * ppu) as usize + 1);
    land = holes.not();
    // Water is ground below the sea surface. Where there is no geometry at
    // all -- beyond the play area, or a part of the frame the zone does not
    // reach -- there is nothing to paint: calling it sea put an ocean across
    // the top of Shaded Dunes, where the land runs on into Fallen Pass.
    let water = Mask { w, h, v: (0..n).map(|i| !land.v[i] && !void.v[i]).collect() };
    let beyond = blur(&as_grid(&void), px(4.0).max(1.0));

    // ---- slope, rock, walls ---------------------------------------------
    let (gy, gx) = gradient(&blur(&hf, px(1.2)));
    let slope: Vec<f32> = gx.v.iter().zip(&gy.v).map(|(a, b)| a.hypot(*b) * p).collect();
    let (lo, hi) = {
        let mut v: Vec<f32> = (0..n).filter(|&i| land.v[i]).map(|i| hs.v[i]).collect();
        v.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
        if v.is_empty() { (0.0, 1.0) } else { (v[v.len() * 5 / 100], v[(v.len() - 1) * 99 / 100]) }
    };
    let rel: Vec<f32> = hs.v.iter().map(|&t| ((t - lo) / (hi - lo).max(1.0)).clamp(0.0, 1.0)).collect();
    let steep = Mask { w, h, v: (0..n).map(|i| land.v[i] && slope[i] > 0.9).collect() };
    let steep = close(&steep, px(2.0).max(1.0));
    let near_steep = grow(&steep, px(18.0));
    // Rock: what is made of rock, and steep natural ground whatever it is
    // made of -- a cliff faced with sand still reads as a cliff. Built
    // surfaces are never rock, however steep.
    let built = |c: u8| matches!(c, mc::WOOD | mc::STONE | mc::METAL | mc::CLOTH);
    // With classes known, steepness has already decided rock per surface;
    // without, fall back to judging by the terrain alone.
    let known = o.classes.is_some();
    let rock = Mask { w, h, v: (0..n).map(|i| land.v[i] && !built(class[i])
        && (class[i] == mc::ROCK
            || (!known && (steep.v[i] || (rel[i] > 0.55 && near_steep.v[i]))))).collect() };
    let rock = drop_small(&open(&rock, 1.5), (120.0 * ppu * ppu) as usize + 1);

    // A wall is a big rise over a short run on otherwise level ground.
    let win = ((1.6 * ppu).round() as usize).max(3) | 1;
    let (mx, mn) = (max_filter(&hf, win), min_filter(&hf, win));
    let (bgy, bgx) = gradient(&blur(&hf, px(5.0)));
    // At coarse levels the window spans several world units, and a gentle
    // slope rises 1.4 over it; the rise has to be steep for the window's
    // real width too.
    let min_step = 1.4f32.max(0.9 * win as f32 / p);
    let walls = Mask { w, h, v: (0..n).map(|i| {
        let step = mx.v[i] - mn.v[i];
        let broad = bgx.v[i].hypot(bgy.v[i]) * p;
        land.v[i] && !rock.v[i] && step > min_step && step < 14.0 && broad < 0.25
    }).collect() };
    let thin = distance_to_background(&walls);
    let walls = Mask { w, h, v: (0..n).map(|i| walls.v[i] && thin.v[i] <= p.max(1.5)).collect() };
    let walls = drop_small(&walls, (6.0 * ppu) as usize + 1);

    // ---- distances -------------------------------------------------------
    let dist_l = distance_to_background(&land);   // inside land, to water
    let dist_w = distance_to_background(&water);  // inside water, to land
    let rock_in = distance_to_background(&rock);
    // Dunes only form on sand.
    let calm = blur(&as_grid(&water.or(&rock).or(&walls).or(&is(mc::SAND).not())), px(14.0));
    let wsoft = blur(&as_grid(&water), 0.9);
    let rsoft = blur(&as_grid(&rock), 0.8);
    let wallsoft = blur(&as_grid(&walls), 0.6);

    // Cast shadows to the south-east (raster: +x, -row).
    let shift = |m: &Mask, d: f32| -> Grid {
        let k = d.round() as isize;
        let mut g = Grid::new(w, h, 0.0);
        for y in 0..h as isize {
            for x in 0..w as isize {
                let (sx, sy) = (x - k, y + k);
                if sx >= 0 && sy >= 0 && (sx as usize) < w && (sy as usize) < h && m.v[sy as usize * w + sx as usize] {
                    g.v[y as usize * w + x as usize] = 1.0;
                }
            }
        }
        g
    };
    let rshadow = {
        let (a, b) = (shift(&rock, px(4.0)), shift(&rock, px(9.0)));
        let g = Grid { w, h, v: a.v.iter().zip(&b.v).map(|(x, y)| x.max(y * 0.6)).collect() };
        blur(&g, px(2.5))
    };
    let wshadow = shift(&walls, px(2.2).max(1.0));

    // ---- dune field: a height of wind-shaped ridges ----------------------
    let th = 28f64.to_radians();
    let duneh = Grid { w, h, v: (0..n).into_par_iter().map(|i| {
        super::throttle::gate();
        let (x, z) = world_of(i);
        let warp = fbm(x, z, 220.0, 1) as f64 * 60.0;
        let c = x * th.cos() - z * th.sin() + warp;
        let ph = (c / 34.0).rem_euclid(1.0) as f32;
        let mut d = if ph < 0.78 { ph / 0.78 } else { (1.0 - ph) / 0.22 }.powf(1.3);
        d *= 0.6 + 0.4 * (fbm(x, z, 140.0, 3) * 0.5 + 0.5);
        let w2 = fbm(x, z, 90.0, 2) as f64 * 18.0;
        let c2 = x * (th + 0.25).cos() - z * (th + 0.25).sin() + w2;
        let rip = ((c2 / 3.2) * std::f64::consts::TAU).sin() as f32
            * (fbm(x, z, 40.0, 4) * 0.5 + 0.5).clamp(0.0, 1.0);
        (d * 6.0 + rip * 0.35) * (1.0 - 2.2 * calm.v[i]).clamp(0.0, 1.0)
    }).collect() };
    let shade_d = hillshade(&duneh, ppu, 35.0);
    let shade_t = hillshade(&Grid { w, h, v: hs.v.iter().map(|t| t * 0.5).collect() }, ppu, 45.0);
    let rock_relief = Grid { w, h, v: (0..n).into_par_iter().map(|i| {
        super::throttle::gate();
        let (x, z) = world_of(i);
        hs.v[i] + fbm(x, z, 25.0, 7) * 3.0
    }).collect() };
    let shade_r = hillshade(&Grid { w, h, v: rock_relief.v.iter().map(|t| t * 0.6).collect() }, ppu, 40.0);

    // ---- colour every pixel ----------------------------------------------
    let (sand_l, sand_d) = (rgb(243, 205, 132), rgb(196, 140, 68));
    let (rock_l, rock_d) = (rgb(226, 190, 138), rgb(150, 108, 66));
    let wall_c = rgb(168, 140, 104);
    let (deep, shallow) = (rgb(12, 118, 160), rgb(58, 196, 205));
    let (foam_c, foam2_c) = (rgb(235, 245, 240), rgb(215, 238, 236));
    let mut img: Vec<[f32; 3]> = (0..n).into_par_iter().map(|i| {
        super::throttle::gate();
        let (x, z) = world_of(i);
        // ground, by what it is made of
        let tone = fbm(x, z, 300.0, 5);
        let grain = (rand01((x * 4.0) as i64, (z * 4.0) as i64, 9) - 0.5) * 9.0;
        let lit = 0.72 + 0.42 * shade_t.v[i];
        let mut c = match class[i] {
            mc::GRASS => {
                let b = fbm(x, z, 18.0, 41) * 0.5 + 0.5;
                let tuft = if rand01((x * 2.0) as i64, (z * 2.0) as i64, 43) > 0.93 { 0.82 } else { 1.0 };
                scale(mix(rgb(74, 112, 46), rgb(132, 168, 78), (b + tone * 0.2).clamp(0.0, 1.0)), lit * tuft)
            }
            mc::DIRT => {
                let b = fbm(x, z, 12.0, 47) * 0.5 + 0.5;
                let pebble = if rand01((x * 3.0) as i64, (z * 3.0) as i64, 49) > 0.95 { 0.8 } else { 1.0 };
                scale(mix(rgb(122, 92, 62), rgb(176, 142, 102), b), lit * pebble)
            }
            mc::MUD => {
                let b = fbm(x, z, 10.0, 53) * 0.5 + 0.5;
                let sheen = ((fbm(x, z, 6.0, 55) - 0.55).max(0.0) * 60.0).min(18.0);
                let m = scale(mix(rgb(66, 52, 38), rgb(108, 88, 64), b), lit);
                [m[0] + sheen, m[1] + sheen, m[2] + sheen]
            }
            mc::SNOW => {
                let b = shade_t.v[i];
                mix(rgb(178, 196, 218), rgb(246, 248, 252), (0.35 + 0.65 * b + tone * 0.1).clamp(0.0, 1.0))
            }
            mc::LAVA => {
                let (d1, d2) = voronoi(x, z, 9.0, 61);
                let crack = (1.0 - ((d2 - d1) / 1.4) as f32).clamp(0.0, 1.0).powf(1.5);
                mix(scale(rgb(56, 44, 40), lit), rgb(255, 120, 30), crack)
            }
            mc::WOOD => {
                // Planks a world unit and a bit wide, running east-west.
                let row = (z / 1.3).floor() as i64;
                let seam = (z / 1.3).rem_euclid(1.0) < 0.08;
                let v = rand01(row, 0, 67) * 0.3;
                let grain2 = fbm(x * 0.3, z * 4.0, 2.0, 69) * 0.08;
                let p = scale(mix(rgb(108, 76, 46), rgb(158, 116, 72), v + 0.35 + grain2), lit);
                if seam { scale(p, 0.6) } else { p }
            }
            mc::STONE => {
                // Flagstones about two and a half units across.
                let (gx, gz) = ((x / 2.6).floor() as i64, (z / 2.6).floor() as i64);
                let (fx, fz) = ((x / 2.6).rem_euclid(1.0), (z / 2.6).rem_euclid(1.0));
                let mortar = fx < 0.06 || fz < 0.06;
                let v = rand01(gx, gz, 71) * 0.35;
                let p = scale(mix(rgb(124, 118, 110), rgb(176, 170, 158), v + 0.3 + tone * 0.1), lit);
                if mortar { scale(p, 0.62) } else { p }
            }
            mc::METAL => scale(rgb(138, 138, 142), lit * (0.92 + 0.08 * fbm(x * 0.2, z * 3.0, 3.0, 73))),
            mc::CLOTH => scale(rgb(142, 62, 56), lit),
            _ => {
                let t = (0.25 + 0.75 * (shade_d.v[i] * 0.7 + shade_t.v[i] * 0.45) - 0.18 + tone * 0.08).clamp(0.0, 1.0);
                mix(sand_d, sand_l, t)
            }
        };
        c = [c[0] + grain, c[1] + grain, c[2] + grain];
        // rock
        if rsoft.v[i] > 0.0 {
            let (d1, d2) = voronoi(x, z, 28.0, 11);
            let crack = (1.0 - ((d2 - d1) / 3.5) as f32).clamp(0.0, 1.0);
            let rt = (0.35 + 0.65 * shade_r.v[i] + fbm(x, z, 60.0, 8) * 0.12).clamp(0.0, 1.0);
            let mut r = scale(mix(rock_d, rock_l, rt), 1.0 - 0.45 * crack);
            r = scale(r, 1.0 - 0.35 * (-(rock_in.v[i] / p) / 2.5).exp());
            c = mix(c, r, rsoft.v[i]);
        }
        c = scale(c, 1.0 - 0.45 * rshadow.v[i] * (1.0 - rsoft.v[i]));
        // walls
        c = scale(c, 1.0 - 0.35 * (wshadow.v[i] - wallsoft.v[i]).clamp(0.0, 1.0));
        c = mix(c, scale(wall_c, 0.85 + 0.3 * shade_t.v[i]), wallsoft.v[i]);
        // wet sand at the shore
        if land.v[i] && dist_l.v[i] < px(4.0) { c = scale(c, 0.86) }
        // water
        if wsoft.v[i] > 0.0 {
            let mut depth = ((dist_w.v[i] / px(55.0)).clamp(0.0, 1.0)).powf(0.6);
            depth = (depth + fbm(x, z, 70.0, 9) * 0.12).clamp(0.0, 1.0);
            let mut wc = scale(mix(shallow, deep, depth), 1.0 + 0.06 * fbm(x, z, 18.0, 10));
            let cw = fbm(x, z, 50.0, 12) as f64 * 40.0;
            let caus = ((x * 0.7 + z * 0.3 + cw) / 7.0).sin().abs() as f32;
            let add = (1.0 - caus).powi(6) * 14.0 * (1.0 - depth);
            wc = [wc[0] + add, wc[1] + add, wc[2] + add];
            c = mix(c, wc, wsoft.v[i]);
            let dw = dist_w.v[i] / p;
            if water.v[i] && dw < 2.2 { c = mix(c, foam_c, 0.65) }
            let ring = 5.0 + 2.5 * fbm(x, z, 30.0, 13);
            if water.v[i] && (dw - ring).abs() < 0.9 { c = mix(c, foam2_c, 0.3) }
        }
        // Off the map: a plain, darkened parchment.
        if beyond.v[i] > 0.0 {
            let g = 1.0 + 0.05 * fbm(x, z, 40.0, 14);
            c = mix(c, scale(rgb(122, 104, 80), g), beyond.v[i]);
        }
        [c[0] / 255.0, c[1] / 255.0, c[2] / 255.0]
    }).collect();

    // ---- north-up ---------------------------------------------------------
    let mut out = vec![[0.0f32; 3]; n];
    for y in 0..h {
        out[(h - 1 - y) * w..(h - y) * w].copy_from_slice(&img[y * w..(y + 1) * w]);
    }
    img = out;
    let mut rgbimg = Rgb { w, h, v: img };
    draw_props(&mut rgbimg, props, &land, o);
    rgbimg
}

// ------------------------------------------------------------------ props

fn blend(img: &mut Rgb, x: i64, y: i64, c: [f32; 3], a: f32) {
    if x < 0 || y < 0 || x as usize >= img.w || y as usize >= img.h { return }
    let p = &mut img.v[y as usize * img.w + x as usize];
    for k in 0..3 { p[k] += (c[k] / 255.0 - p[k]) * a }
}

fn fill_tri(img: &mut Rgb, a: (f32, f32), b: (f32, f32), c: (f32, f32), col: [f32; 3], al: f32) {
    let (x0, x1) = (a.0.min(b.0).min(c.0).floor() as i64, a.0.max(b.0).max(c.0).ceil() as i64);
    let (y0, y1) = (a.1.min(b.1).min(c.1).floor() as i64, a.1.max(b.1).max(c.1).ceil() as i64);
    let e = |p: (f32, f32), q: (f32, f32), x: f32, y: f32| (q.0 - p.0) * (y - p.1) - (q.1 - p.1) * (x - p.0);
    let area = e(a, b, c.0, c.1);
    if area.abs() < 1e-6 { return }
    for y in y0..=y1 {
        for x in x0..=x1 {
            let (fx, fy) = (x as f32 + 0.5, y as f32 + 0.5);
            let (w0, w1, w2) = (e(b, c, fx, fy) / area, e(c, a, fx, fy) / area, e(a, b, fx, fy) / area);
            if w0 >= 0.0 && w1 >= 0.0 && w2 >= 0.0 { blend(img, x, y, col, al) }
        }
    }
}

fn fill_ellipse(img: &mut Rgb, cx: f32, cy: f32, rx: f32, ry: f32, col: [f32; 3], al: f32) {
    for y in (cy - ry).floor() as i64..=(cy + ry).ceil() as i64 {
        for x in (cx - rx).floor() as i64..=(cx + rx).ceil() as i64 {
            let (dx, dy) = ((x as f32 + 0.5 - cx) / rx, (y as f32 + 0.5 - cy) / ry);
            if dx * dx + dy * dy <= 1.0 { blend(img, x, y, col, al) }
        }
    }
}

/// Palms, bushes, tents and fires where the zone's scene places them, on
/// dry land only, upright once the viewer turns the map north-up (see
/// `ink::upright`). Drawn north first -- highest world X -- so southern ones
/// overlap them.
fn draw_props(img: &mut Rgb, props: &BTreeMap<String, Vec<(f64, f64)>>, land: &Mask, o: &PaintOptions) {
    let ppu = o.ppu as f32;
    let mut all: Vec<(&str, f64, f64)> = props.iter()
        .flat_map(|(k, v)| v.iter().map(move |&(x, z)| (k.as_str(), x, z))).collect();
    all.sort_by(|a, b| b.1.partial_cmp(&a.1).unwrap_or(std::cmp::Ordering::Equal));
    let up = super::ink::upright;
    for (kind, wx, wz) in all {
        let col = ((wx - o.bbox.0) * o.ppu) as f32;
        let row_s = ((wz - o.bbox.2) * o.ppu) as f32; // raster row, south = 0
        if col < 0.0 || row_s < 0.0 || col >= img.w as f32 || row_s >= img.h as f32 { continue }
        if !land.at(col as usize, row_s as usize) { continue }
        let (x, y) = (col, img.h as f32 - 1.0 - row_s);
        let r01 = rand01((wx * 10.0) as i64, (wz * 10.0) as i64, 21);
        match kind {
            "tree" => {
                let r = (9.0 + r01 * 4.0) * ppu;
                if r < 2.5 { blend(img, x as i64, y as i64, rgb(40, 120, 45), 1.0); continue }
                // Shadow down and to the right on screen; turned, its
                // ellipse swaps axes.
                let (sx, sy) = up(x, y, r * 0.7, r * 0.8);
                fill_ellipse(img, sx, sy, r * 0.35, r * 0.6, rgb(60, 40, 15), 0.27);
                let a0 = r01 * std::f32::consts::TAU;
                for j in 0..7 {
                    let a = a0 + j as f32 * std::f32::consts::TAU / 7.0;
                    let tip = (x + r * a.cos(), y + r * a.sin());
                    let s1 = (x + r * 0.45 * (a + 0.5).cos(), y + r * 0.45 * (a + 0.5).sin());
                    let s2 = (x + r * 0.45 * (a - 0.5).cos(), y + r * 0.45 * (a - 0.5).sin());
                    let g = 110.0 + rand01(j, (wx * 10.0) as i64, 22) * 40.0;
                    let leaf = [40.0, g, 45.0];
                    fill_tri(img, (x, y), s1, tip, leaf, 1.0);
                    fill_tri(img, (x, y), tip, s2, scale(leaf, 0.8), 1.0);
                }
                fill_ellipse(img, x, y, 1.2 * ppu, 1.2 * ppu, rgb(110, 80, 40), 1.0);
            }
            "scrub" => {
                let r = (3.5 + r01 * 1.5) * ppu;
                let (sx, sy) = up(x, y, 1.5, 1.5);
                fill_ellipse(img, sx, sy, r, r, rgb(60, 40, 15), 0.24);
                fill_ellipse(img, x, y, r, r, rgb(96, 120, 52), 1.0);
            }
            "tent" => {
                let r = 6.0 * ppu;
                let p = |dx: f32, dy: f32| up(x, y, dx, dy);
                fill_tri(img, p(-r + 3.0, r * 0.6 + 3.0), p(r + 3.0, r * 0.6 + 3.0), p(3.0, -r * 0.8 + 3.0), rgb(50, 30, 10), 0.27);
                fill_tri(img, p(-r, r * 0.6), p(r, r * 0.6), p(0.0, -r * 0.8), rgb(214, 196, 160), 1.0);
                fill_tri(img, p(0.0, -r * 0.8), p(r, r * 0.6), p(0.0, r * 0.6), rgb(184, 166, 130), 1.0);
            }
            "fire" => {
                let r = 1.8 * ppu;
                fill_ellipse(img, x, y, r * 2.0, r * 2.0, rgb(255, 170, 60), 0.2);
                fill_ellipse(img, x, y, r, r, rgb(240, 120, 30), 1.0);
            }
            _ => {}
        }
    }
}
