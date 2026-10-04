//! Tile pyramids: one SQLite container per zone.
//!
//! Scene coordinates are WORLD units with Z negated, so +Z (north) is up on
//! screen. Keeping the scene in world units -- not in the pixels of some zoom
//! level -- is what lets markers stay put when the level under them changes.

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

        Ok(Self { path: path.to_path_buf(), name, tile, extent, levels, conn })
    }

    pub fn scene_rect(&self) -> egui::Rect {
        let [ax, bx, az, bz] = self.extent;
        egui::Rect::from_min_max(
            egui::pos2(ax as f32, -bz as f32),
            egui::pos2(bx as f32, -az as f32),
        )
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
    pub fn markers_path(&self, base: &Path) -> PathBuf {
        let slug = self.path.file_stem().unwrap_or_default().to_string_lossy().into_owned();
        base.join("markers").join(format!("{slug}.json"))
    }
}

pub fn world_to_scene(wx: f64, wz: f64) -> egui::Pos2 {
    egui::pos2(wx as f32, -wz as f32)
}
pub fn scene_to_world(p: egui::Pos2) -> (f64, f64) {
    (p.x as f64, -p.y as f64)
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
