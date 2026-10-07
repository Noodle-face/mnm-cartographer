//! Place names: districts, buildings and landmarks lettered on the map.
//!
//! These are part of the map, not annotations. They ship inside the binary
//! from `places/<zone>.json` and are never written to the data directory, so
//! they cannot be moved, edited or deleted, and they update with the app.
//! They are not markers: no badge, no legend entry, no place in packs.

use serde::Deserialize;

mod data {
    include!(concat!(env!("OUT_DIR"), "/places.rs"));
}

#[derive(Clone, Deserialize)]
pub struct Place {
    pub name: String,
    pub x: f64,
    pub z: f64,
    /// As for markers: the floor it is lettered on, or None for the zone as
    /// a whole (the top-level map).
    #[serde(default)]
    pub floor: Option<u8>,
}

#[derive(Deserialize)]
struct File {
    places: Vec<Place>,
}

/// The place names for a zone, by its map slug (e.g. "nightharbor").
pub fn for_zone(slug: &str) -> Vec<Place> {
    let want = format!("{}.json", slug.to_lowercase());
    data::PLACES
        .iter()
        .find(|(name, _)| *name == want)
        .and_then(|(_, body)| serde_json::from_str::<File>(body).ok())
        .map(|f| f.places)
        .unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A malformed file would silently show no names, so every shipped file
    /// must parse and hold only named, finite positions.
    #[test]
    fn every_shipped_file_parses() {
        assert!(!data::PLACES.is_empty());
        for (name, body) in data::PLACES {
            let f: File = serde_json::from_str(body).unwrap_or_else(|e| panic!("{name}: {e}"));
            assert!(!f.places.is_empty(), "{name}: no places");
            for p in &f.places {
                assert!(!p.name.trim().is_empty(), "{name}: unnamed place");
                assert!(p.x.is_finite() && p.z.is_finite(), "{name}: {} has a bad position", p.name);
            }
        }
        assert!(!for_zone("NightHarbor").is_empty(), "lookup should ignore case");
    }
}
