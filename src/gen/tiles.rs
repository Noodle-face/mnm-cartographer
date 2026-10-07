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
    /// Build a zone's floor maps along with its top-level map.
    pub floors: bool,
    /// Floors (numbered from 1) to leave out.
    pub skip_floors: Vec<usize>,
    /// Rebuild only the floors, keeping the top-level map already built.
    pub floors_only: bool,
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
            floors: true,
            skip_floors: Vec::new(),
            floors_only: false,
        }
    }
}

/// Pin ONE world bbox and render every level into it. Letting each level
/// derive its own extent drifts the world bounds by ~1.5 units between the
/// coarsest and finest -- enough that a marker placed at one zoom lands
/// visibly off at another. Markers live in world space, so levels must agree
/// on world space exactly.
pub fn plan(tris: &[Tri], frame: Option<raster::Bbox>, s: &Settings) -> (raster::Bbox, f64) {
    let bbox = frame.unwrap_or_else(|| raster::auto_bbox(tris));
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
    /// The world box to frame the map on, or None for the whole of `tris`.
    pub bbox: Option<raster::Bbox>,
    /// Per triangle, what it is made of (see [`LoadedZone::mats`]).
    pub tags: Option<(&'a [u16], &'a [super::materials::MatClass])>,
    /// The zone's lights: a painted map also gets a Lamplight version.
    pub lights: Option<&'a [super::light::Light]>,
}

/// Build a zone's map, and a map per floor if it has floors (see
/// [`super::floors`]). `on_level` is called as each zoom level of each map
/// completes, with the count done and the count in all.
///
/// The maps are built side by side. Inking one keeps under two cores busy,
/// so five in a row took 140s where side by side they take little more than
/// the slowest. They run on the caller's thread pool, whose size is what
/// bounds memory, so floors add work but not peak memory.
pub fn build_zone(
    out: &Path,
    z: &ZoneInput,
    s: &Settings,
    on_level: impl FnMut(usize, usize) + Send,
) -> Result<(usize, u64)> {
    // Floors only: reuse the frame of the map already there, so every floor
    // lines up with it exactly. Without one there is nothing to line up with.
    let existing = if s.floors_only { frame_of(out) } else { None };
    if s.floors_only && existing.is_none() {
        anyhow::bail!("no top-level map to build floors for; build the zone first");
    }
    let frame = existing.unwrap_or_else(|| plan(z.tris, z.bbox, s));
    let floors = if s.floors {
        super::floors::split(z.tris, super::floors::plan(z.name))
    } else {
        Vec::new()
    };

    // Floors are rebuilt with their zone or not at all, so a zone that loses
    // a floor does not keep a stale one.
    let dir = super::floors::dir_for(out);
    if s.floors {
        // A rebuilt top-level map may sit in a new frame; any floor not
        // rebuilt with it would no longer line up, so it goes. Rebuilding
        // only floors keeps the frame, and the floors left out stay.
        if !s.floors_only { std::fs::remove_dir_all(&dir).ok(); }
    }
    let no_props = BTreeMap::new();
    // Every floor uses the top-level map's frame: they must agree on world
    // space and zoom levels, or markers and the view would shift when
    // switching floor. Scene props are left off floors; they carry no height
    // to say which floor they stand on.
    let mut jobs = Vec::new();
    if !s.floors_only {
        jobs.push((out.to_path_buf(), ZoneInput { ..*z }, Style::Ink));
    }
    for (i, (f, tris)) in floors.iter().enumerate() {
        if tris.is_empty() || s.skip_floors.contains(&(i + 1)) { continue }
        jobs.push((
            dir.join(super::floors::file_name(i)),
            ZoneInput { name: z.name, tris, sea: z.sea, props: &no_props, bbox: z.bbox, tags: None, lights: None },
            Style::Floor(i, f.name),
        ));
    }

    let total = s.zooms * jobs.len();
    let progress = std::sync::Mutex::new((0usize, on_level));
    // One thread per map, for the same reason levels get their own: long
    // jobs must not occupy the shared pool.
    let results: Vec<Result<(usize, u64)>> = std::thread::scope(|sc| {
        let hs: Vec<_> = jobs.iter().map(|(path, zi, style)| {
            let progress = &progress;
            sc.spawn(move || build(path, zi, frame, *style, s, &mut |_, _| {
                let mut p = progress.lock().unwrap();
                p.0 += 1;
                let done = p.0;
                (p.1)(done, total);
            }))
        }).collect();
        hs.into_iter().map(|h| h.join().expect("map build panicked")).collect()
    });
    let (mut n, mut b) = (0, 0);
    for r in results {
        let (rn, rb) = r?;
        n += rn;
        b += rb;
    }
    Ok((n, b))
}

/// The frame (world box, coarsest pixels per unit) and zoom count of the map
/// at `path`, read back from its metadata.
pub fn frame_of(path: &Path) -> Option<(raster::Bbox, f64)> {
    let db = Connection::open_with_flags(path, rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY).ok()?;
    let get = |k: &str| db.query_row("SELECT value FROM metadata WHERE name=?1", [k],
                                     |r| r.get::<_, String>(0)).ok();
    let e: [f64; 4] = serde_json::from_str(&get("extent")?).ok()?;
    let zooms: Vec<serde_json::Value> = serde_json::from_str(&get("zooms")?).ok()?;
    let ppu = zooms.iter().filter_map(|z| z.get("ppu")?.as_f64())
        .fold(f64::MAX, f64::min);
    (ppu.is_finite() && ppu < f64::MAX).then_some(((e[0], e[1], e[2], e[3]), ppu))
}

/// How a pyramid is drawn.
#[derive(Clone, Copy)]
pub enum Style<'a> {
    /// The ordinary inked map.
    Ink,
    /// One floor of a multi-storey zone: its index and name.
    Floor(usize, &'a str),
    /// The experimental painted style; see [`super::paint`].
    Painted,
    /// The painted style of one floor: its geometry only, drawn as a floor
    /// plan -- the lowest surface -- as the floor's own map is.
    PaintedFloor,
}

/// Paint a zone into the frame of a map it already has -- the same world
/// box and zoom levels -- so the two line up exactly and markers sit in the
/// same place on both.
pub fn build_painted(
    out: &Path,
    z: &ZoneInput,
    frame: (raster::Bbox, f64),
    zooms: usize,
    floor: bool,
    on_level: &mut (impl FnMut(usize, usize) + Send),
) -> Result<(usize, u64)> {
    let s = Settings { zooms, ..Settings::default() };
    build(out, z, frame, if floor { Style::PaintedFloor } else { Style::Painted }, &s, on_level)
}

/// Build one pyramid into `frame` (world bbox, coarsest pixels per unit).
fn build(
    out: &Path,
    z: &ZoneInput,
    frame: (raster::Bbox, f64),
    style: Style,
    s: &Settings,
    on_level: &mut (impl FnMut(usize, usize) + Send),
) -> Result<(usize, u64)> {
    let (bbox, base_ppu) = frame;
    let edges = raster::global_band_edges(z.tris, s.bands);

    if let Some(p) = out.parent() {
        std::fs::create_dir_all(p).ok();
    }
    let painted = matches!(style, Style::Painted | Style::PaintedFloor);
    // Lamplight: one exposure for the whole zone, from its coarsest level.
    let lamp = match (painted, z.lights) {
        (true, Some(ls)) if !ls.is_empty() => {
            let h0 = match style {
                Style::PaintedFloor => raster::rasterize_floor(z.tris, base_ppu, bbox, 2.5, 20.0),
                _ => raster::rasterize(z.tris, base_ppu, bbox),
            };
            super::light::exposure(&super::light::light_map(ls, &h0, base_ppu, bbox), &h0).map(|e| (ls, e))
        }
        _ => None,
    };

    // The zoom levels are independent renders of the same geometry: make
    // them side by side, then write them in order. Each level gets its own
    // thread rather than a task on the shared pool: a level is one long job,
    // and long jobs parked on pool threads starve the short parallel work
    // inside every other zone -- one zone waited 184s for 9s of work.
    let progress = std::sync::Mutex::new((0usize, on_level));
    let render_level = |zi: usize| {
        let ppu = base_ppu * 2f64.powi(zi as i32);
        if super::profiling() { eprintln!("    level {zi} (ppu {ppu})") }
        let height = { let _t = super::Timer::start("rasterize");
            match style {
                // Roofs sit 4-14 units over their rooms, bridges ~30 over the
                // hall they cross; see rasterize_floor.
                Style::Floor(..) | Style::PaintedFloor => raster::rasterize_floor(z.tris, ppu, bbox, 2.5, 20.0),
                _ => raster::rasterize(z.tris, ppu, bbox),
            } };
        let img = match style {
            Style::Painted | Style::PaintedFloor => {
                let tags = z.tags.map(|(t, table)| (raster::rasterize_tagged(z.tris, t, ppu, bbox).1, table));
                let ground = raster::rasterize_low(z.tris, ppu, bbox);
                let day = super::paint::render(&height, z.props, &super::paint::PaintOptions {
                    ppu, sea: z.sea, bbox, ground: Some(&ground),
                    classes: tags.as_ref().map(|(t, table)| (t.as_slice(), *table)),
                });
                let lit = lamp.map(|(ls, k)| {
                    let mut img = day.clone();
                    super::light::apply(&mut img, &super::light::light_map(ls, &height, ppu, bbox),
                                        &super::light::coverage(&height, ppu), k);
                    img
                });
                (day, lit)
            }
            _ => {
                let r = ink::render(&height, &edges, &ink::InkOptions {
                    ppu,
                    step_thresh: s.step_thresh,
                    sea: z.sea,
                    seed: 5,
                });
                let mut img = r.img;
                let on_land = ink::props_on_land(z.props, &r.dry, bbox);
                ink::draw_props(&mut img, bbox, ppu, &on_land);
                (img, None)
            }
        };
        drop(height);
        let (img, lit) = img;

        let (w, h) = (img.w, img.h);
        let _te = super::Timer::start("tile encode");
        let tiles = encode_tiles(&img, s.quality);
        let lit_tiles = lit.map(|l| encode_tiles(&l, s.quality));
        {
            let mut p = progress.lock().unwrap();
            p.0 += 1;
            let done = p.0;
            (p.1)(done, s.zooms);
        }
        (zi, ppu, w, h, tiles, lit_tiles)
    };
    let levels: Vec<Level> =
        std::thread::scope(|sc| {
            let hs: Vec<_> = (0..s.zooms).map(|zi| sc.spawn(move || render_level(zi))).collect();
            hs.into_iter().map(|h| h.join().expect("level render panicked")).collect()
        });

    let mut meta = vec![
        ("name".to_string(), z.name.to_string()),
        // Which build produced this map. Lets a later version tell that a
        // container predates a rendering change without re-reading its tiles.
        ("generator".to_string(), env!("CARGO_PKG_VERSION").to_string()),
        // Symbols and lighting are drawn for north at world +X, which the
        // viewer puts up while the raster keeps +Z at its top. Maps without
        // this key have them drawn for +Z, and show sideways; see pyramid.rs.
        ("north".to_string(), "x".to_string()),
        ("format".to_string(), "webp".to_string()),
        ("tileSize".to_string(), TILE.to_string()),
        ("extent".to_string(), serde_json::to_string(&[bbox.0, bbox.1, bbox.2, bbox.3])?),
    ];
    match style {
        Style::Floor(i, name) => {
            meta.push(("floor".to_string(), (i + 1).to_string()));
            meta.push(("floor_name".to_string(), name.to_string()));
        }
        Style::Painted | Style::PaintedFloor => meta.push(("style".to_string(), "painted".to_string())),
        Style::Ink => {}
    }
    let day: Vec<_> = levels.iter().map(|l| (l.0, l.1, l.2, l.3, &l.4)).collect();
    let made = write_pyramid(out, &day, &meta, bbox)?;
    if levels.iter().all(|l| l.5.is_some()) && !levels.is_empty() {
        let lit: Vec<_> = levels.iter().map(|l| (l.0, l.1, l.2, l.3, l.5.as_ref().unwrap())).collect();
        let mut m = meta.clone();
        m.push(("light".to_string(), "lamp".to_string()));
        write_pyramid(&lamplight_path(out), &lit, &m, bbox)?;
    }
    Ok(made)
}

type Tiles = Vec<(usize, usize, Vec<u8>)>;
/// One rendered zoom level: index, ppu, size, its tiles, and its Lamplight
/// tiles where it has them.
type Level = (usize, f64, usize, usize, Tiles, Option<Tiles>);

/// Where the Lamplight version of a painted map lives: beside it.
pub fn lamplight_path(painted: &Path) -> std::path::PathBuf {
    let stem = painted.file_stem().unwrap_or_default().to_string_lossy();
    painted.with_file_name(format!("{stem}-lamplight.mbtiles"))
}

/// Cut a level's image into stored tiles.
fn encode_tiles(img: &ink::Rgb, quality: f32) -> Tiles {
    use rayon::prelude::*;
    let (w, h) = (img.w, img.h);
    let nx = (w + TILE - 1) / TILE;
    let ny = (h + TILE - 1) / TILE;
    (0..nx * ny).into_par_iter().map(|k| {
            super::throttle::gate();
            let (tx, ty) = (k / ny, k % ny);
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
            (tx, ty, enc.encode(quality).to_vec())
        }).collect()
}

/// Write rendered levels as a pyramid at `out`.
fn write_pyramid(out: &Path, levels: &[(usize, f64, usize, usize, &Tiles)],
                 meta: &[(String, String)], bbox: raster::Bbox) -> Result<(usize, u64)> {
    // Build into a temporary file and rename at the end. A container that
    // exists is then always a complete one, so an interrupted run leaves no
    // half-written zone that a later run would mistake for finished.
    let tmp = out.with_extension("mbtiles.part");
    std::fs::remove_file(&tmp).ok();
    let mut db = Connection::open(&tmp).with_context(|| format!("create {}", tmp.display()))?;
    db.execute_batch(SCHEMA)?;
    let mut zooms_meta = Vec::new();
    let (mut total_tiles, mut total_bytes) = (0usize, 0u64);
    let tx_db = db.transaction()?;
    {
        let mut stmt = tx_db.prepare("INSERT OR REPLACE INTO tiles VALUES (?,?,?,?)")?;
        for (zi, ppu, w, h, tiles) in levels {
            let (nx, ny) = ((w + TILE - 1) / TILE, (h + TILE - 1) / TILE);
            for (tx, ty, blob) in tiles.iter() {
                total_bytes += blob.len() as u64;
                total_tiles += 1;
                stmt.execute(rusqlite::params![*zi as i64, *tx as i64, *ty as i64, blob])?;
            }
            zooms_meta.push(serde_json::json!({
                "z": zi, "ppu": ppu, "px": [w, h], "tiles": [nx, ny],
                "extent": [bbox.0, bbox.1, bbox.2, bbox.3],
            }));
        }
    }
    tx_db.commit()?;
    let mut meta = meta.to_vec();
    meta.push(("zooms".to_string(), serde_json::to_string(&zooms_meta)?));
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
pub struct LoadedZone {
    /// Walkable triangles, clipped to `bbox`.
    pub tris: Vec<Tri>,
    pub sea: Option<f32>,
    pub props: BTreeMap<String, Vec<(f64, f64)>>,
    /// The play area to frame the map on; see [`super::bounds`].
    pub bbox: raster::Bbox,
    /// How the play area was found: "boundary", "walkable" or "all".
    pub bounds: &'static str,
    /// Every prop, unthinned: the painted style draws each palm.
    pub all_props: BTreeMap<String, Vec<(f64, f64)>>,
    /// The same, with each prop's height, for painting one floor.
    pub prop_points: BTreeMap<String, Vec<[f64; 3]>>,
    /// Per triangle, an index into `mat_table`: what the surface is made
    /// of. Empty unless materials were asked for (the painted style).
    pub mats: Vec<u16>,
    pub mat_table: Vec<super::materials::MatClass>,
    /// Its lamps, torches and glows; see [`super::light`].
    pub lights: Vec<super::light::Light>,
}

pub fn load_zone(env: &super::bundle::Env, shared: &super::bundle::Shared,
                 group: &super::zones::Group) -> LoadedZone {
    load_zone_with(env, shared, group, false)
}

/// As [`load_zone`]; `tagged` also records what each surface is made of.
pub fn load_zone_with(env: &super::bundle::Env, shared: &super::bundle::Shared,
                      group: &super::zones::Group, tagged: bool) -> LoadedZone {
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
    // What is a neighbour's scenery rather than this zone: worked out once,
    // for extraction and landmarks alike.
    let skip = super::extract::backdrop(env, &files);
    let floors = { let _t = super::Timer::start("load: extract");
        super::extract::extract_floors_with(env, shared, &files, tagged, &skip) };
    let _t = super::Timer::start("load: scene, bounds");
    let sea = { let _t = super::Timer::start("  sea level"); scene::find_sea_level(env, &files) };
    let prop_points = { let _t = super::Timer::start("  props"); scene::scene_props_3d(env, &files) };
    let all_props: BTreeMap<String, Vec<(f64, f64)>> = prop_points.iter()
        .map(|(k, v)| (k.clone(), v.iter().map(|p| (p[0], p[2])).collect())).collect();
    let props = all_props.iter()
        .map(|(k, v)| (k.clone(), scene::thin(v, 55.0)))
        .collect();
    if floors.tris.is_empty() {
        return LoadedZone { tris: floors.tris, sea, props, bbox: (0.0, 0.0, 0.0, 0.0), bounds: "all",
                            all_props, prop_points, mats: Vec::new(), mat_table: Vec::new(),
                            lights: Vec::new() };
    }
    let lm = { let _t = super::Timer::start("  landmarks"); super::extract::landmarks_with(env, &files, &skip) };
    // Every trimming step yields a keep-mask, applied to the triangles and
    // to their material tags alike so the two stay aligned.
    use super::bounds::apply;
    let (mut tris, mut mats) = (floors.tris, floors.mats);
    let keep = |tris: &mut Vec<Tri>, mats: &mut Vec<u16>, k: Vec<bool>| {
        if !mats.is_empty() { *mats = apply(mats, &k) }
        *tris = apply(tris, &k);
    };
    let k = { let _t = super::Timer::start("  strays"); super::bounds::strays_keep(&tris, &lm.objects) };
    keep(&mut tris, &mut mats, k);
    let k = { let _t = super::Timer::start("  sky"); super::bounds::sky_keep(&tris, raster::auto_bbox(&tris)) };
    keep(&mut tris, &mut mats, k);
    let full = raster::auto_bbox(&tris);
    let region = { let _t = super::Timer::start("  play region"); super::bounds::play_region(&tris, &floors.walls, &floors.blocks, &lm, sea, full) };
    let (bbox, bounds) = match region {
        Some(r) => {
            let b = r.bbox();
            let k = super::bounds::region_keep(&tris, &r);
            keep(&mut tris, &mut mats, k);
            // The frame is the region, but never wider than the geometry.
            ((b.0.max(full.0), b.1.min(full.1), b.2.max(full.2), b.3.min(full.3)), r.method)
        }
        None => (full, "all"),
    };
    let lights = if tagged { let _t = super::Timer::start("  lights"); super::light::scene_lights(env, &files) } else { Vec::new() };
    LoadedZone { tris, sea, props, bbox, bounds, all_props, prop_points, mats, mat_table: floors.mat_table,
                 lights }
}
