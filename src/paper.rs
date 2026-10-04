//! A seamless parchment texture for the area around the map.
//!
//! Generated rather than shipped: it is a few lines of noise, keeps the binary
//! free of asset files, and can be matched exactly to the paper the maps are
//! rendered on. Tiling it means a map of any shape sits on continuous paper
//! instead of a black letterbox.

pub const SIZE: usize = 256;

/// Base paper tone, matching the un-vignetted edge of a rendered sheet.
const BASE: [f32; 3] = [246.0, 239.0, 226.0];

/// Deterministic hash -> [0,1). Same texture every run, no rng dependency.
fn hash01(x: i32, y: i32, seed: u32) -> f32 {
    let mut h = (x as u32).wrapping_mul(0x27d4_eb2d)
        ^ (y as u32).wrapping_mul(0x1656_67b1)
        ^ seed.wrapping_mul(0x9e37_79b9);
    h ^= h >> 15;
    h = h.wrapping_mul(0x85eb_ca6b);
    h ^= h >> 13;
    (h & 0x00ff_ffff) as f32 / 16_777_216.0
}

fn smoothstep(t: f32) -> f32 { t * t * (3.0 - 2.0 * t) }

/// Value noise on a `period`-cell grid, wrapping so the tile is seamless.
fn noise(x: f32, y: f32, period: i32, seed: u32) -> f32 {
    let (xi, yi) = (x.floor() as i32, y.floor() as i32);
    let (fx, fy) = (smoothstep(x - xi as f32), smoothstep(y - yi as f32));
    let w = |a: i32| a.rem_euclid(period);
    let (x0, x1) = (w(xi), w(xi + 1));
    let (y0, y1) = (w(yi), w(yi + 1));
    let a = hash01(x0, y0, seed);
    let b = hash01(x1, y0, seed);
    let c = hash01(x0, y1, seed);
    let d = hash01(x1, y1, seed);
    let top = a + (b - a) * fx;
    let bot = c + (d - c) * fx;
    top + (bot - top) * fy
}

/// RGBA bytes for one seamless tile.
pub fn texture() -> Vec<u8> {
    let mut out = vec![0u8; SIZE * SIZE * 4];
    for y in 0..SIZE {
        for x in 0..SIZE {
            let (fx, fy) = (x as f32, y as f32);
            // Two scales, mirroring how the maps are rendered: fine grain over
            // broad fibre. Octaves all divide SIZE so the tile still wraps.
            let fibre = noise(fx / 32.0, fy / 32.0, SIZE as i32 / 32, 11) - 0.5;
            let mid = noise(fx / 8.0, fy / 8.0, SIZE as i32 / 8, 23) - 0.5;
            let grain = hash01(x as i32, y as i32, 97) - 0.5;
            // Amplitudes matched to the paper of a real rendered sheet, which
            // measures std (1.8, 2.1, 5.8) per channel. Blue varies ~3.2x more
            // than red: the mottling shifts warmth, not just brightness, so the
            // modulation is weighted per channel.
            let d = 1.0 + 0.022 * fibre + 0.013 * mid + 0.017 * grain;
            let warm = [1.0f32, 1.15, 3.2];
            let i = (y * SIZE + x) * 4;
            for c in 0..3 {
                let dc = 1.0 + (d - 1.0) * warm[c];
                out[i + c] = (BASE[c] * dc).clamp(0.0, 255.0) as u8;
            }
            out[i + 3] = 255;
        }
    }
    out
}
