//! Lamplight: a painted map coloured by the zone's own lights.
//!
//! The painted style shows what a zone is made of, as if in daylight. Many
//! zones are never seen that way: Underdocks has no sun at all, and is lit by
//! 2,000-odd lamps, torches and glowing growths, the strongest of them blue
//! and teal. Lamplight sums those lights over the map and colours it with
//! them -- warm streets, blue caverns, green grottos.
//!
//! It enhances; it does not hide. A map is for finding your way, so nothing
//! falls to black: unlit ground keeps half its brightness under a faint cool
//! cast, and light only adds colour and brightness on top.
//!
//! Whether a zone is under the sky cannot be read from its files -- every
//! zone carries the same directional "sun" and no skybox of its own -- so
//! every painted map gets both looks, and the player chooses.
//!
//! Simplifications: light falls off with distance as the game's point lights
//! do, in three dimensions using each pixel's height, but walls do not block
//! it. Spot lights are treated as point lights.

use super::bundle::*;
use super::grid::Grid;
use super::ink::Rgb;
use super::raster::Bbox;

#[derive(Clone, Copy, Debug)]
pub struct Light {
    pub p: [f64; 3],
    pub rgb: [f32; 3],
    pub intensity: f32,
    pub range: f32,
}

const CLASS_LIGHT: i32 = 108;
/// The longest reach the game's own lights use; a guard, not a style choice.
const MAX_RANGE: f32 = 300.0;
const SPOT: i64 = 0;
const POINT: i64 = 2;

/// Every enabled point and spot light in the scenes, where it really is.
pub fn scene_lights(env: &Env, files: &[usize]) -> Vec<Light> {
    let mut hier = super::extract::Hierarchy::default();
    let mut out = Vec::new();
    for &fi in files {
        for oi in 0..env.col.serialized_files()[fi].file.objects.len() {
            if env.class_id(fi, oi) != Some(CLASS_LIGHT) { continue }
            let Some(lt) = env.read(fi, oi) else { continue };
            let ty = as_i64(field(&lt, "m_Type")).unwrap_or(-1);
            if ty != POINT && ty != SPOT { continue }
            if as_i64(field(&lt, "m_Enabled")).unwrap_or(1) == 0 { continue }
            let intensity = as_f64(field(&lt, "m_Intensity")).unwrap_or(0.0) as f32;
            if intensity <= 0.0 { continue }
            let Some(go_at) = as_pptr(field(&lt, "m_GameObject")).and_then(|p| env.resolve(fi, p))
            else { continue };
            let Some(go) = env.read(go_at.0, go_at.1) else { continue };
            if as_i64(field(&go, "m_IsActive")).unwrap_or(1) == 0 { continue }
            let Some(tr) = super::extract::transform_at(env, go_at.0, &go) else { continue };
            let c = field(&lt, "m_Color");
            let g = |k: &str| c.and_then(|c| as_f64(field(c, k))).unwrap_or(0.0) as f32;
            out.push(Light {
                p: hier.world(env, tr).t,
                rgb: [g("r"), g("g"), g("b")],
                intensity,
                range: (as_f64(field(&lt, "m_Range")).unwrap_or(10.0) as f32).clamp(1.0, MAX_RANGE),
            });
        }
    }
    out
}

/// Light reaching each pixel of a raster laid out as `height` is (row 0 the
/// lowest Z), from the light's real distance to the surface there. Where
/// the raster has no surface, the ground is taken to lie a few units under
/// the light.
pub fn light_map(lights: &[Light], height: &Grid, ppu: f64, bbox: Bbox) -> Vec<[f32; 3]> {
    let (w, h) = (height.w, height.h);
    let mut out = vec![[0f32; 3]; w * h];
    for l in lights {
        let r = l.range as f64;
        let col = |x: f64| (((x - bbox.0) * ppu).floor() as isize).clamp(0, w as isize - 1) as usize;
        let row = |z: f64| (((z - bbox.2) * ppu).floor() as isize).clamp(0, h as isize - 1) as usize;
        let (c0, c1) = (col(l.p[0] - r), col(l.p[0] + r));
        let (r0, r1) = (row(l.p[2] - r), row(l.p[2] + r));
        if l.p[0] + r < bbox.0 || l.p[0] - r > bbox.1 || l.p[2] + r < bbox.2 || l.p[2] - r > bbox.3 { continue }
        let rr = (l.range * l.range).max(1.0);
        for y in r0..=r1 {
            let dz = (bbox.2 + (y as f64 + 0.5) / ppu - l.p[2]) as f32;
            for x in c0..=c1 {
                let dx = (bbox.0 + (x as f64 + 0.5) / ppu - l.p[0]) as f32;
                let d2xz = dx * dx + dz * dz;
                if d2xz > rr { continue }
                let i = y * w + x;
                let gy = height.v[i];
                let dy = if gy.is_finite() { (l.p[1] as f32 - gy).abs() } else { 3.0 };
                // Unity's inverse-square falloff, faded to nothing at range,
                // with a floor under the distance so a lamp on the ground
                // does not burn a white hole.
                let d2 = d2xz + dy * dy;
                let win = (1.0 - (d2 / rr).powi(2)).max(0.0).powi(2);
                let e = l.intensity * win / d2.max(4.0);
                for k in 0..3 { out[i][k] += e * l.rgb[k] }
            }
        }
    }
    out
}

/// How a zone's light is read, fixed once for the whole zone from its
/// coarsest level -- computed per level, a zone would change as you zoom.
#[derive(Clone, Copy, Debug)]
pub struct Exposure {
    /// Light every part of the zone gets, per channel: a wash, not a lamp.
    /// It is taken away, so what colours the map is light that varies --
    /// Underdocks' blue caverns stay, and a few lights reaching across all
    /// of Shaded Dunes no longer turn a whole desert green.
    pub base: [f32; 3],
    /// Scale that brings the brighter lit ground near full effect.
    pub k: f32,
}

pub fn exposure(map: &[[f32; 3]], height: &Grid) -> Option<Exposure> {
    let on: Vec<[f32; 3]> = map.iter().zip(&height.v).filter(|(_, y)| y.is_finite()).map(|(c, _)| *c).collect();
    if on.is_empty() { return None }
    let pct = |v: &mut Vec<f32>, q: usize| {
        v.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
        v[(v.len() - 1) * q / 100]
    };
    let base = [0, 1, 2].map(|c| pct(&mut on.iter().map(|p| p[c]).collect(), 25));
    let mut lum: Vec<f32> = on.iter()
        .map(|p| (0..3).map(|c| (p[c] - base[c]).max(0.0)).sum::<f32>() / 3.0)
        .filter(|l| *l > 0.0).collect();
    if lum.is_empty() { return None }
    let p90 = pct(&mut lum, 90);
    (p90 > 1e-6).then_some(Exposure { base, k: 1.0 / p90 })
}

/// How bright unlit ground stays, and the faint cool cast it takes.
const FLOOR: f32 = 0.5;
const AMBIENT: [f32; 3] = [0.88, 0.92, 1.0];
/// How much light can add on top: enough to colour, short of glare.
const GAIN: f32 = 0.8;

/// Colour a painted image by its light map. `img` is north-up (row 0 the
/// highest Z); `map` and `coverage` are laid out as the height raster (row 0
/// the lowest). Coverage is 1 on the map and 0 off it: the parchment round a
/// map is not part of the zone, and dimming it made whole maps look gloomy.
pub fn apply(img: &mut Rgb, map: &[[f32; 3]], coverage: &[f32], e: Exposure) {
    let (w, h) = (img.w, img.h);
    for y in 0..h {
        let src = (h - 1 - y) * w;
        for x in 0..w {
            let l = map[src + x];
            let a = coverage[src + x].clamp(0.0, 1.0);
            let p = &mut img.v[y * w + x];
            for c in 0..3 {
                let glow = 1.0 - (-(l[c] - e.base[c]).max(0.0) * e.k * 1.4).exp();
                let f = FLOOR * AMBIENT[c] + GAIN * glow;
                p[c] = (p[c] * (1.0 + (f - 1.0) * a)).clamp(0.0, 1.0);
            }
        }
    }
}

/// Where a raster has a surface, softened at the edges so the lighting
/// fades out across the map's own fade into parchment.
pub fn coverage(height: &Grid, ppu: f64) -> Vec<f32> {
    let m = Grid { w: height.w, h: height.h,
                   v: height.v.iter().map(|y| if y.is_finite() { 1.0 } else { 0.0 }).collect() };
    super::grid::gaussian(&m, ((2.0 * ppu) as f32).max(0.8)).v
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn nothing_goes_dark_and_light_tints() {
        let mut img = Rgb { w: 1, h: 2, v: vec![[0.8, 0.6, 0.4], [0.8, 0.6, 0.4]] };
        // Bottom pixel unlit; top pixel (row 0 north-up = last raster row) lit blue.
        let map = vec![[0.0, 0.0, 10.0], [0.0; 3]];
        let e = Exposure { base: [0.0; 3], k: 1.0 };
        apply(&mut img, &map, &[1.0, 1.0], e);
        let unlit = img.v[0];
        let lit = img.v[1];
        // Unlit keeps about half its brightness, never less than 40%.
        assert!(unlit[0] >= 0.8 * 0.4 && unlit[2] >= 0.4 * 0.4, "{unlit:?}");
        // Off the map, nothing changes.
        let mut off = Rgb { w: 1, h: 1, v: vec![[0.5, 0.5, 0.5]] };
        apply(&mut off, &[[0.0; 3]], &[0.0], e);
        assert_eq!(off.v[0], [0.5, 0.5, 0.5]);
        // Lit by blue is bluer than unlit.
        assert!(lit[2] > unlit[2] && (lit[0] - unlit[0]).abs() < 1e-6, "{lit:?} vs {unlit:?}");
    }
}
