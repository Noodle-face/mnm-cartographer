//! Building a zone's tile pyramid straight into its SQLite container.
//!
//! Every zoom level is rendered NATIVELY rather than downsampled from one big
//! sheet. Style constants are in pixels and geometry thresholds in world units,
//! so a natively-rendered coarse level gets a sensible 2px coastline where a
//! downsampled one would have hairlines that vanish. Band breaks are computed
//! once for the whole zone so terrain colour does not shift between levels.
//!
//! Tiles go directly into the database. The Python wrote loose files and packed
//! them afterwards, which cost ~50% to 4 KB block granularity on the way.

use super::extract::Tri;
use super::{ink, raster, scene};
use anyhow::{Context, Result};
use rusqlite::Connection;
use std::collections::BTreeMap;
use std::path::Path;

pub const TILE: usize = 256;
const SCHEMA: &str = "
PRAGMA page_size = 4096;
CREATE TABLE IF NOT EXISTS metadata (name TEXT PRIMARY KEY, value TEXT);
CREATE TABLE IF NOT EXISTS tiles (
    zoom_level  INTEGER NOT NULL,
    tile_column INTEGER NOT NULL,
    tile_row    INTEGER NOT NULL,
    tile_data   BLOB    NOT NULL,
    PRIMARY KEY (zoom_level, tile_column, tile_row)
);";

pub struct Settings {
    pub zooms: usize,
    pub budget_mp: f64,
    pub max_ppu: f64,
    pub bands: usize,
    pub step_thresh: f32,
    pub quality: f32,
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            zooms: 4,
            // Zone areas vary ~100x, so a fixed resolution makes big outdoor
            // zones explode. The finest level gets a pixel budget instead.
            budget_mp: 16.0,
            // The measured geometric limit is ~12-16 px/unit (median triangle
            // edge 0.5 units); past that you just magnify facets.
            max_ppu: 8.0,
            bands: 8,
            step_thresh: 3.2,
            quality: 85.0,
        }
    }
}

/// Pin ONE world bbox and render every level into it. Letting each level
/// derive its own extent drifts the world bounds by ~1.5 units between the
/// coarsest and finest -- enough that a marker placed at one zoom lands
/// visibly off at another. Markers live in world space, so levels must agree
/// on world space exactly.
pub fn plan(tris: &[Tri], s: &Settings) -> (raster::Bbox, f64) {
    let bbox = raster::auto_bbox(tris);
    let area = ((bbox.1 - bbox.0) * (bbox.3 - bbox.2)).max(1.0);
    let top = (s.budget_mp * 1e6 / area).sqrt();
    let top = s.max_ppu.min(2f64.powf(top.max(1e-6).log2().round()));
    (bbox, top / 2f64.powi(s.zooms as i32 - 1))
}

pub struct ZoneInput<'a> {
    pub name: &'a str,
    pub tris: &'a [Tri],
    pub sea: Option<f32>,
    pub props: &'a BTreeMap<String, Vec<(f64, f64)>>,
}

/// Build the pyramid. `on_level` is called as each level completes.
pub fn build(
    out: &Path,
    z: &ZoneInput,
    s: &Settings,
    mut on_level: impl FnMut(usize, usize),
) -> Result<(usize, u64)> {
    let (bbox, base_ppu) = plan(z.tris, s);
    let edges = raster::global_band_edges(z.tris, s.bands);

    if let Some(p) = out.parent() {
        std::fs::create_dir_all(p).ok();
    }
    // Build into a temporary file and rename at the end. A container that
    // exists is then always a complete one, so an interrupted run leaves no
    // half-written zone that a later run would mistake for finished.
    let tmp = out.with_extension("mbtiles.part");
    std::fs::remove_file(&tmp).ok();
    let mut db = Connection::open(&tmp).with_context(|| format!("create {}", tmp.display()))?;
    db.execute_batch(SCHEMA)?;

    let mut zooms_meta = Vec::new();
    let mut total_tiles = 0usize;
    let mut total_bytes = 0u64;

    for zi in 0..s.zooms {
        let ppu = base_ppu * 2f64.powi(zi as i32);
        if super::profiling() { eprintln!("    level {zi} (ppu {ppu})") }
        let height = { let _t = super::Timer::start("rasterize");
            raster::rasterize(z.tris, ppu, bbox) };
        let r = ink::render(&height, &edges, &ink::InkOptions {
            ppu,
            step_thresh: s.step_thresh,
            sea: z.sea,
            seed: 5,
        });
        let mut img = r.img;
        let on_land = ink::props_on_land(z.props, &r.dry, bbox);
        ink::draw_props(&mut img, bbox, ppu, &on_land);

        let (w, h) = (img.w, img.h);
        let nx = (w + TILE - 1) / TILE;
        let ny = (h + TILE - 1) / TILE;
        let _te = super::Timer::start("tile encode+write");
        let tx_db = db.transaction()?;
        {
            let mut stmt = tx_db.prepare("INSERT OR REPLACE INTO tiles VALUES (?,?,?,?)")?;
            for tx in 0..nx {
                for ty in 0..ny {
                    // Partial edge tiles are padded to a full tile with paper,
                    // so every stored tile is exactly TILE square.
                    let mut buf = vec![246u8, 240, 226].repeat(TILE * TILE);
                    for yy in 0..TILE {
                        let sy = ty * TILE + yy;
                        if sy >= h {
                            break;
                        }
                        for xx in 0..TILE {
                            let sx = tx * TILE + xx;
                            if sx >= w {
                                break;
                            }
                            let p = img.v[sy * w + sx];
                            let o = (yy * TILE + xx) * 3;
                            buf[o] = (p[0] * 255.0).clamp(0.0, 255.0) as u8;
                            buf[o + 1] = (p[1] * 255.0).clamp(0.0, 255.0) as u8;
                            buf[o + 2] = (p[2] * 255.0).clamp(0.0, 255.0) as u8;
                        }
                    }
                    let enc = webp::Encoder::from_rgb(&buf, TILE as u32, TILE as u32);
                    let blob = enc.encode(s.quality);
                    total_bytes += blob.len() as u64;
                    total_tiles += 1;
                    stmt.execute(rusqlite::params![zi as i64, tx as i64, ty as i64, &*blob])?;
                }
            }
        }
        tx_db.commit()?;

        zooms_meta.push(serde_json::json!({
            "z": zi, "ppu": ppu, "px": [w, h], "tiles": [nx, ny],
            "extent": [bbox.0, bbox.1, bbox.2, bbox.3],
        }));
        on_level(zi + 1, s.zooms);
    }

    let meta = [
        ("name".to_string(), z.name.to_string()),
        // Which build produced this map. Lets a later version tell that a
        // container predates a rendering change without re-reading its tiles.
        ("generator".to_string(), env!("CARGO_PKG_VERSION").to_string()),
        ("format".to_string(), "webp".to_string()),
        ("tileSize".to_string(), TILE.to_string()),
        ("extent".to_string(), serde_json::to_string(&[bbox.0, bbox.1, bbox.2, bbox.3])?),
        ("zooms".to_string(), serde_json::to_string(&zooms_meta)?),
    ];
    for (k, v) in meta {
        db.execute("INSERT OR REPLACE INTO metadata VALUES (?,?)", rusqlite::params![k, v])?;
    }
    db.execute_batch("VACUUM")?;
    drop(db);
    std::fs::rename(&tmp, out)
        .with_context(|| format!("finalise {}", out.display()))?;
    Ok((total_tiles, total_bytes))
}

/// Everything needed to build one zone, read from its bundle.
pub fn load_zone(
    env: &super::bundle::Env,
    group: &super::zones::Group,
) -> (Vec<Tri>, Option<f32>, BTreeMap<String, Vec<(f64, f64)>>) {
    let idx = env.scene_index();
    let mut files: Vec<usize> = Vec::new();
    for path in group.scenes() {
        if let Some(cab) = idx.get(path) {
            if let Some(fi) = env.file_index(cab) {
                files.push(fi);
            }
        }
    }
    files.sort_unstable();
    files.dedup();
    let floors = super::extract::extract_floors(env, &files);
    let sea = scene::find_sea_level(env, &files);
    let props = scene::scene_props(env, &files)
        .into_iter()
        .map(|(k, v)| (k, scene::thin(&v, 55.0)))
        .collect();
    (floors.tris, sea, props)
}
