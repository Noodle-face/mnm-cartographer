//! Tile pyramids: one SQLite container per zone.
//!
//! Scene coordinates are WORLD units turned so north is up on screen. Keeping
//! the scene in world units -- not in the pixels of some zoom level -- is what
//! lets markers stay put when the level under them changes.
//!
//! North is world +X, and east -Z. The game draws no compass, so north is the
//! community's: the wiki's world map of Calafrey and Szurr is drawn north-up,
//! and every zone's backdrop scenery of its neighbours lies in the direction
//! that map gives them only under this orientation (median error 12 degrees
//! over 22 neighbour pairs; +Z north, assumed before, was off by 90). Maps are
//! still rendered with +Z at the top of the raster; the viewer turns them.

use anyhow::{Context, Result};
use rusqlite::Connection;
use serde::Deserialize;
use std::path::{Path, PathBuf};

#[derive(Debug, Clone, Deserialize)]
pub struct Level {
    pub z: i64,
    pub ppu: f64,
    pub px: [i64; 2],
    pub tiles: [i64; 2],
}

pub struct Pyramid {
    pub path: PathBuf,
    pub name: String,
    pub tile: i64,
    /// (min_x, max_x, min_z, max_z) in world units, shared by every level.
    pub extent: [f64; 4],
    pub levels: Vec<Level>,
    /// For one floor of a multi-storey zone: its number (1 = lowest) and name.
    pub floor: Option<(usize, String)>,
    /// Symbols and lighting were drawn for the corrected north. Maps built
    /// before 0.0.7 show palms and tents on their sides until rebuilt.
    pub upright: bool,
    conn: Connection,
}

impl Pyramid {
    pub fn open(path: &Path) -> Result<Self> {
        let conn = Connection::open_with_flags(
            path,
            rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY | rusqlite::OpenFlags::SQLITE_OPEN_URI,
        )
        .with_context(|| format!("opening {}", path.display()))?;

        let meta = |k: &str| -> Result<String> {
            conn.query_row("SELECT value FROM metadata WHERE name=?1", [k], |r| r.get(0))
                .with_context(|| format!("metadata key {k}"))
        };

        let name = meta("name").unwrap_or_else(|_| {
            path.file_stem().unwrap_or_default().to_string_lossy().into_owned()
        });
        let tile: i64 = meta("tileSize")
            .ok()
            .and_then(|s| s.trim_matches('"').parse().ok())
            .unwrap_or(256);
        let mut levels: Vec<Level> = serde_json::from_str(&meta("zooms")?)?;
        levels.sort_by_key(|l| l.z);
        let extent: [f64; 4] = serde_json::from_str(&meta("extent")?)?;

        let floor = meta("floor").ok().and_then(|n| n.parse().ok())
            .map(|n| (n, meta("floor_name").unwrap_or_default()));

        let upright = meta("north").is_ok_and(|n| n == "x");

        Ok(Self { path: path.to_path_buf(), name, tile, extent, levels, floor, upright, conn })
    }

    pub fn scene_rect(&self) -> egui::Rect {
        let [ax, bx, az, bz] = self.extent;
        egui::Rect::from_two_pos(world_to_scene(ax, az), world_to_scene(bx, bz))
    }

    /// Finest level whose native resolution still exceeds what is on screen.
    pub fn level_for(&self, px_per_world: f64) -> &Level {
        let mut best = &self.levels[0];
        for l in &self.levels {
            if l.ppu <= px_per_world * 1.35 {
                best = l;
            }
        }
        best
    }

    /// World-space rect covered by one tile. Row 0 is NORTH.
    pub fn tile_world_rect(&self, l: &Level, tx: i64, ty: i64) -> (f64, f64, f64, f64) {
        let [ax, bx, az, bz] = self.extent;
        let upx = (bx - ax) / l.px[0] as f64;
        let upz = (bz - az) / l.px[1] as f64;
        let t = self.tile as f64;
        (ax + tx as f64 * t * upx, bz - ty as f64 * t * upz, t * upx, t * upz)
    }

    /// Tiles intersecting a world-space window.
    pub fn tiles_in(&self, l: &Level, wx0: f64, wx1: f64, wz0: f64, wz1: f64) -> Vec<(i64, i64)> {
        let [ax, bx, az, bz] = self.extent;
        let upx = (bx - ax) / l.px[0] as f64;
        let upz = (bz - az) / l.px[1] as f64;
        let t = self.tile as f64;
        let tx0 = ((wx0 - ax) / (t * upx)).floor() as i64;
        let tx1 = ((wx1 - ax) / (t * upx)).floor() as i64;
        let ty0 = ((bz - wz1) / (t * upz)).floor() as i64;
        let ty1 = ((bz - wz0) / (t * upz)).floor() as i64;
        let mut out = Vec::new();
        for tx in tx0.max(0)..=tx1.min(l.tiles[0] - 1) {
            for ty in ty0.max(0)..=ty1.min(l.tiles[1] - 1) {
                out.push((tx, ty));
            }
        }
        out
    }

    pub fn tile_bytes(&self, z: i64, tx: i64, ty: i64) -> Option<Vec<u8>> {
        self.conn
            .query_row(
                "SELECT tile_data FROM tiles WHERE zoom_level=?1 AND tile_column=?2 AND tile_row=?3",
                [z, tx, ty],
                |r| r.get::<_, Vec<u8>>(0),
            )
            .ok()
    }

    /// Annotations live in markers/<slug>.json beside the map directory:
    /// tiles are generated and disposable, annotations are authored.
    /// The zone's file-name key, as markers and place names are stored.
    pub fn slug(&self) -> String {
        self.path.file_stem().unwrap_or_default().to_string_lossy().into_owned()
    }

    pub fn markers_path(&self, base: &Path) -> PathBuf {
        base.join("markers").join(format!("{}.json", self.slug()))
    }
}

/// Screen right is east (-Z); screen down is south (-X).
pub fn world_to_scene(wx: f64, wz: f64) -> egui::Pos2 {
    egui::pos2(-wz as f32, -wx as f32)
}
pub fn scene_to_world(p: egui::Pos2) -> (f64, f64) {
    (-p.y as f64, -p.x as f64)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn north_is_up_and_east_is_right() {
        let o = world_to_scene(0.0, 0.0);
        // +X is north: up the screen, so smaller y.
        assert!(world_to_scene(100.0, 0.0).y < o.y);
        // -Z is east: right on the screen.
        assert!(world_to_scene(0.0, -100.0).x > o.x);
        let (x, z) = scene_to_world(world_to_scene(123.5, -45.25));
        assert!((x - 123.5).abs() < 1e-3 && (z + 45.25).abs() < 1e-3);
    }
}

/// The floor maps of the zone map at `map`, lowest first. Empty for a zone
/// with one level, or one generated before floors existed.
pub fn floors_of(map: &Path) -> Vec<Pyramid> {
    let Ok(rd) = std::fs::read_dir(crate::gen::floors::dir_for(map)) else { return Vec::new() };
    let mut out: Vec<Pyramid> = rd.filter_map(|e| e.ok())
        .map(|e| e.path())
        .filter(|p| p.extension().and_then(|s| s.to_str()) == Some("mbtiles"))
        .filter_map(|p| Pyramid::open(&p).ok())
        .filter(|p| p.floor.is_some())
        .collect();
    out.sort_by_key(|p| p.floor.as_ref().map(|f| f.0));
    out
}

/// Where the painted version of the map at `map` lives, beside its floors.
pub fn painted_path(map: &Path) -> PathBuf {
    let stem = map.file_stem().unwrap_or_default();
    map.parent().unwrap_or(Path::new(".")).join("painted").join(stem).with_extension("mbtiles")
}

/// The painted version of the map at `map`, if one has been made.
pub fn painted_of(map: &Path) -> Option<Pyramid> {
    let p = painted_path(map);
    p.is_file().then(|| Pyramid::open(&p).ok()).flatten()
}

/// Its Lamplight version -- coloured by the zone's own lights -- if one has
/// been made; see gen::light.
pub fn lamplight_of(map: &Path) -> Option<Pyramid> {
    let p = crate::gen::tiles::lamplight_path(&painted_path(map));
    p.is_file().then(|| Pyramid::open(&p).ok()).flatten()
}

/// Every pyramid under `base`: a flat directory of .mbtiles, or a maps/ subdir.
pub fn discover(base: &Path) -> Vec<Pyramid> {
    let mut out = Vec::new();
    for dir in [base.to_path_buf(), base.join("maps")] {
        let Ok(rd) = std::fs::read_dir(&dir) else { continue };
        let mut entries: Vec<_> = rd.filter_map(|e| e.ok()).map(|e| e.path()).collect();
        entries.sort();
        for p in entries {
            if p.extension().and_then(|s| s.to_str()) == Some("mbtiles") {
                if let Ok(py) = Pyramid::open(&p) {
                    if !out.iter().any(|o: &Pyramid| o.name == py.name) {
                        out.push(py);
                    }
                }
            }
        }
        if !out.is_empty() {
            break;
        }
    }
    out.sort_by(|a, b| a.name.to_lowercase().cmp(&b.name.to_lowercase()));
    out
}
