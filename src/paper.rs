//! The page around the map: the same paper the maps are drawn on.
//!
//! A map's own background -- everything beyond its play area -- is page
//! colour with a fine grain, one speck per map pixel. The backdrop must be
//! that same page, or the map reads as a card laid on a different sheet, its
//! edge plain to see and the card swinging round over it when the map turns.
//! So this is generated exactly as the renderer makes its page (gen::ink and
//! gen::paint), and the viewer lays it in the map's own pixel grid: turned
//! with the map, one texel per map pixel at whatever zoom is showing.
//!
//! Generated rather than shipped: it is a few lines of noise and keeps the
//! binary free of asset files.

pub const SIZE: usize = 256;

/// The page colour: gen::ink's PAPER.
pub const PAGE: [f32; 3] = [239.0, 230.0, 207.0];
/// How strongly the grain modulates it: gen::ink's, per unit of deviation.
pub const GRAIN: f32 = 0.030;

/// White noise in [0, 1), wrapping every SIZE pixels.
fn hash01(x: usize, y: usize) -> f32 {
    let (x, y) = ((x % SIZE) as u32, (y % SIZE) as u32);
    let mut s = x.wrapping_mul(0x9E37_79B9).wrapping_add(y.wrapping_mul(0x85EB_CA6B))
        .wrapping_add(5u32.wrapping_mul(0xC2B2_AE35));
    s ^= s >> 15;
    s = s.wrapping_mul(0x2545_F491);
    s ^= s >> 13;
    s = s.wrapping_mul(0x3C79_AC49);
    s ^= s >> 16;
    (s >> 8) as f32 / 16_777_216.0
}

/// The grain, as the renderer makes it: white noise blurred at sigma 0.8
/// and normalised to zero mean, unit deviation -- here wrapping, so the tile
/// repeats without a seam.
pub fn grain() -> Vec<f32> {
    let sigma = 0.8f32;
    let r = 3isize;
    let k: Vec<f32> = (-r..=r).map(|d| (-(d * d) as f32 / (2.0 * sigma * sigma)).exp()).collect();
    let ks: f32 = k.iter().sum();
    let n = SIZE as isize;
    let wrap = |v: isize| v.rem_euclid(n) as usize;
    let raw: Vec<f32> = (0..SIZE * SIZE).map(|i| hash01(i % SIZE, i / SIZE)).collect();
    let mut tmp = vec![0f32; SIZE * SIZE];
    for y in 0..n {
        for x in 0..n {
            tmp[y as usize * SIZE + x as usize] = (-r..=r)
                .map(|d| raw[y as usize * SIZE + wrap(x + d)] * k[(d + r) as usize]).sum::<f32>() / ks;
        }
    }
    let mut out = vec![0f32; SIZE * SIZE];
    for y in 0..n {
        for x in 0..n {
            out[y as usize * SIZE + x as usize] = (-r..=r)
                .map(|d| tmp[wrap(y + d) * SIZE + x as usize] * k[(d + r) as usize]).sum::<f32>() / ks;
        }
    }
    let mean = out.iter().sum::<f32>() / out.len() as f32;
    let sd = (out.iter().map(|v| (v - mean).powi(2)).sum::<f32>() / out.len() as f32).sqrt() + 1e-6;
    out.iter().map(|v| (v - mean) / sd).collect()
}

/// RGBA bytes for one seamless tile of page.
pub fn texture() -> Vec<u8> {
    let g = grain();
    let mut out = vec![0u8; SIZE * SIZE * 4];
    for (i, gv) in g.iter().enumerate() {
        for c in 0..3 {
            out[i * 4 + c] = (PAGE[c] * (1.0 + GRAIN * gv)).clamp(0.0, 255.0) as u8;
        }
        out[i * 4 + 3] = 255;
    }
    out
}
