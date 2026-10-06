//! Floors: separate maps of the storeys of a zone built on top of itself.
//!
//! A zone with rooms stacked over rooms renders as a tangle from above, since
//! a top-down view can only show one surface per spot. Such a zone also gets
//! one map per floor, each a horizontal slab of its walkable geometry, beside
//! the ordinary top-level map.
//!
//! Floors are cut at fixed heights per zone. The cuts were chosen where the
//! zone has the least walkable area -- between floors rather than through
//! them -- and checked by eye. A stair crossing a cut shows half on each floor.

use super::extract::Tri;
use std::path::{Path, PathBuf};

pub struct Floor {
    /// World height (y) of this floor's ceiling cut; the floor below's ends
    /// here. The top floor has no ceiling.
    pub top: f32,
    pub name: &'static str,
}

/// The floors of `zone`, bottom first; empty for a zone with one level.
///
/// Chosen from a survey of every zone: the cut heights where the least
/// walkable area lies, kept only where each floor then rendered as a clean
/// plan of its own. Outdoor zones are left out even where hills give them
/// "levels" -- splitting a hillside by height cuts the terrain into strips
/// rather than separating anything a player would call a floor.
pub fn plan(zone: &str) -> &'static [Floor] {
    const TOP: f32 = f32::INFINITY;
    macro_rules! two {
        ($cut:expr) => {
            &[Floor { top: $cut, name: "Lower level" }, Floor { top: TOP, name: "Upper level" }]
        };
    }
    match zone {
        "BlindMidden" => &[
            Floor { top: 53.0, name: "Caverns & lake hall" },
            Floor { top: 93.0, name: "Lower complex" },
            Floor { top: 105.0, name: "Upper complex" },
            Floor { top: TOP, name: "Upper story" },
        ],
        "KingPyrotrsFortress" => &[
            Floor { top: 198.0, name: "Lower level" },
            Floor { top: 240.0, name: "Middle level" },
            Floor { top: TOP, name: "Upper level" },
        ],
        "AilVorith" => two!(42.0),
        "AncientCrypt" => two!(5.0),
        "Broodwood" => two!(192.0),
        "FallenCrypt" => two!(19.0),
        "GrimtideSanctum" => two!(-142.0),
        "MiniCrypt" => two!(5.0),
        "SandyCrypt" => two!(-2.0),
        "TelEkir" => two!(27.0),
        "TelEkirB" => two!(27.0),
        "WyrmsbaneTomb" => two!(-17.0),
        _ => &[],
    }
}

/// Each floor's triangles, by the height of the triangle's centre.
pub fn split<'a>(tris: &[Tri], floors: &'a [Floor]) -> Vec<(&'a Floor, Vec<Tri>)> {
    let mut out: Vec<(&Floor, Vec<Tri>)> = floors.iter().map(|f| (f, Vec::new())).collect();
    for t in tris {
        let y = (t[0][1] + t[1][1] + t[2][1]) / 3.0;
        if let Some(i) = floors.iter().position(|f| y < f.top) {
            out[i].1.push(*t);
        }
    }
    out
}

/// Where the floors of the map at `map` live: `floors/<map stem>/` beside it.
/// A subdirectory, so map discovery -- which reads one directory flat -- never
/// mistakes a floor for a zone of its own.
pub fn dir_for(map: &Path) -> PathBuf {
    let stem = map.file_stem().unwrap_or_default();
    map.parent().unwrap_or(Path::new(".")).join("floors").join(stem)
}

pub fn file_name(index: usize) -> String {
    format!("floor{}.mbtiles", index + 1)
}
