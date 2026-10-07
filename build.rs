//! Embed the shipped annotation files into the binary.
//!
//! The viewer must work as a single executable placed anywhere: without
//! connections.json it loses official zone names and the wiki exit graph, and
//! without the seed markers a new user starts with nothing. Both are small and
//! are written into the data directory on first run if absent.
//!
//! Place names are embedded too, but are read straight from the binary and
//! never written out: they are part of the map, not the user's to edit.
use std::io::Write;

fn main() {
    println!("cargo:rerun-if-changed=connections.json");
    println!("cargo:rerun-if-changed=markers");
    println!("cargo:rerun-if-changed=places");
    let dir = std::path::PathBuf::from(std::env::var("OUT_DIR").unwrap());
    embed(&dir.join("seed.rs"), "SEED_MARKERS", "markers");
    embed(&dir.join("places.rs"), "PLACES", "places");
}

/// Write `pub static <name>: &[(file name, contents)]` for every .json in `src`.
fn embed(out: &std::path::Path, name: &str, src: &str) {

    let mut f = std::fs::File::create(out).unwrap();
    writeln!(f, "pub static {name}: &[(&str, &str)] = &[").unwrap();
    if let Ok(rd) = std::fs::read_dir(src) {
        let mut names: Vec<_> = rd
            .flatten()
            .map(|e| e.path())
            .filter(|p| p.extension().and_then(|s| s.to_str()) == Some("json"))
            .collect();
        names.sort();
        for p in names {
            let name = p.file_name().unwrap().to_string_lossy().to_string();
            writeln!(
                f,
                "    ({:?}, include_str!({:?})),",
                name,
                std::fs::canonicalize(&p).unwrap()
            )
            .unwrap();
        }
    }
    writeln!(f, "];").unwrap();
}
