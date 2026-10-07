//! Generating zone maps from a local game install.
//!
//! The viewer ships no map data. This module reads the game's own Addressables
//! bundles and renders the tile pyramids the viewer displays, so a user builds
//! their maps from the copy of the game they already own.

pub mod bundle;
pub mod grid;
pub mod job;
pub mod analyze;
pub mod ink;
pub mod raster;
pub mod scene;
pub mod tiles;
pub mod zones;
pub mod extract;
pub mod floors;
pub mod bounds;
pub mod paint;
pub mod materials;
pub mod throttle;
pub mod light;

/// Scenes that carry no walkable geometry, or carry geometry that lies about
/// the zone's size. Audio subscenes are empty; distant/facade/backdrop scenery
/// is drawn to be seen from far away and inflated Night Harbor's extent 10.5x,
/// demoting its finest zoom from 4 px/unit to 1.
pub fn is_geometry_scene(path: &str) -> bool {
    let p = path.to_lowercase();
    !["_audio", "_sound", "audio_subscene", "subscene_sound",
      "distant", "facade", "backdrop", "background", "skybox"]
        .iter()
        .any(|k| p.contains(k))
}

/// Stage timing, on when MNM_PROFILE is set. Generation is a long opaque
/// operation; without per-stage numbers any optimisation is guesswork.
pub struct Timer(std::time::Instant, &'static str);

pub fn profiling() -> bool {
    static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ON.get_or_init(|| std::env::var("MNM_PROFILE").is_ok())
}

impl Timer {
    pub fn start(what: &'static str) -> Option<Timer> {
        profiling().then(|| Timer(std::time::Instant::now(), what))
    }
}

impl Drop for Timer {
    fn drop(&mut self) {
        eprintln!("      {:<22} {:7.2}s", self.1, self.0.elapsed().as_secs_f32());
    }
}
