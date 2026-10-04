//! Finding zones: which bundle holds them, and which scenes make them up.

use super::bundle::Env;
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

/// Where the game keeps its Addressables, with the usual install locations
/// tried in turn. `MNM_BUNDLES` overrides everything.
pub fn bundles_dir() -> Option<PathBuf> {
    if let Ok(p) = std::env::var("MNM_BUNDLES") {
        let p = PathBuf::from(p);
        if p.is_dir() {
            return Some(p);
        }
    }
    let rel = "mnm_Data/StreamingAssets/aa/StandaloneWindows64";
    let mut roots: Vec<PathBuf> = Vec::new();
    if let Some(home) = dirs::home_dir() {
        roots.push(home.join("Games/mnm"));
        roots.push(home.join(".local/share/mnm/mnm"));
        roots.push(home.join("Games/umu/mnm/drive_c/Program Files/Monsters and Memories"));
        // Steam and the standalone launcher, Windows and Proton alike.
        for base in [
            home.join(".steam/steam/steamapps/common"),
            home.join(".local/share/Steam/steamapps/common"),
        ] {
            roots.push(base.join("Monsters and Memories"));
        }
    }
    for drive in ["C:/Program Files", "C:/Program Files (x86)"] {
        roots.push(PathBuf::from(drive).join("Monsters and Memories"));
    }
    roots.into_iter().map(|r| r.join(rel)).find(|p| p.is_dir())
}

pub fn bundle_files(dir: &Path) -> Vec<PathBuf> {
    let Ok(rd) = std::fs::read_dir(dir) else { return Vec::new() };
    let mut v: Vec<PathBuf> = rd
        .flatten()
        .map(|e| e.path())
        .filter(|p| {
            let n = p.file_name().and_then(|s| s.to_str()).unwrap_or("");
            n.contains("_scenes_") && n.ends_with(".bundle")
        })
        .collect();
    v.sort();
    v
}

/// Scenes that are not zones at all.
fn skip_scene(path: &str) -> bool {
    let p = path.to_lowercase();
    ["biomejam", "template_testing", "cszone", "pathfindingtest", "loginbackground"]
        .iter()
        .any(|k| p.contains(k))
}

/// `Assets/Scenes/Maps/<Zone>.unity` or `.../Maps/<Zone>/Sub.unity`.
fn zone_of(path: &str) -> Option<String> {
    for marker in ["/Maps/", "/Terrain/"] {
        if let Some(i) = path.find(marker) {
            let rest = &path[i + marker.len()..];
            let end = rest.find('/').unwrap_or_else(|| rest.len());
            let name = &rest[..end];
            let name = name.strip_suffix(".unity").unwrap_or(name);
            if !name.is_empty() {
                return Some(name.to_string());
            }
        }
    }
    None
}

pub fn slug(name: &str) -> String {
    name.chars().filter(|c| c.is_ascii_alphanumeric()).collect::<String>().to_lowercase()
}

#[derive(Default, Clone)]
pub struct Group {
    pub parent: Option<String>,
    /// Scene paths carrying geometry.
    pub subs: Vec<String>,
}

impl Group {
    pub fn scenes(&self) -> Vec<&str> {
        let mut v: Vec<&str> = Vec::new();
        if let Some(p) = &self.parent {
            v.push(p);
        }
        v.extend(self.subs.iter().map(|s| s.as_str()));
        v
    }
}

pub fn zone_groups(env: &Env) -> BTreeMap<String, Group> {
    let mut out: BTreeMap<String, Group> = BTreeMap::new();
    for path in env.scene_index().keys() {
        if skip_scene(path) {
            continue;
        }
        let Some(zone) = zone_of(path) else { continue };
        let g = out.entry(zone.clone()).or_default();
        let leaf = format!("/maps/{}.unity", zone.to_lowercase());
        if path.to_lowercase().ends_with(&leaf) {
            g.parent = Some(path.clone());
        } else if super::is_geometry_scene(path) {
            g.subs.push(path.clone());
        }
    }
    out.retain(|_, g| g.parent.is_some() || !g.subs.is_empty());
    out
}

pub struct Located {
    pub zone: String,
    pub bundle: PathBuf,
    pub group: Group,
}

/// Every zone the install ships, with the bundle that best holds it.
///
/// A zone can appear in more than one bundle -- some have subscenes in the
/// default group and their parent scene in a zone bundle -- so prefer the one
/// that actually has the parent scene, then the one with the most subscenes.
/// Otherwise the partial copy wins and renders a fragment of the zone.
pub fn survey(dir: &Path, mut progress: impl FnMut(usize, usize, &str)) -> Vec<Located> {
    let files = bundle_files(dir);
    let total = files.len();
    let mut best: BTreeMap<String, (u8, usize, PathBuf, Group)> = BTreeMap::new();
    for (i, f) in files.iter().enumerate() {
        progress(i, total, f.file_name().and_then(|s| s.to_str()).unwrap_or(""));
        let Ok(env) = Env::open(f) else { continue };
        for (zone, g) in zone_groups(&env) {
            let score = (g.parent.is_some() as u8, g.subs.len());
            let better = match best.get(&zone) {
                None => true,
                Some((p, n, _, _)) => (score.0, score.1) > (*p, *n),
            };
            if better {
                best.insert(zone, (score.0, score.1, f.clone(), g));
            }
        }
    }
    progress(total, total, "");
    best.into_iter()
        .map(|(zone, (_, _, bundle, group))| Located { zone, bundle, group })
        .collect()
}

// --------------------------------------------------------------- survey cache

/// The survey opens every bundle -- around 20 seconds, and the bundles only
/// change when the game updates. Cache the result against each file's size and
/// modification time, which is now a large share of a short run.
#[derive(serde::Serialize, serde::Deserialize)]
struct CachedSurvey {
    stamps: Vec<(String, u64, u64)>,
    zones: Vec<CachedZone>,
}

#[derive(serde::Serialize, serde::Deserialize)]
struct CachedZone {
    zone: String,
    bundle: String,
    parent: Option<String>,
    subs: Vec<String>,
}

fn stamps(dir: &Path) -> Vec<(String, u64, u64)> {
    bundle_files(dir)
        .iter()
        .filter_map(|p| {
            let m = std::fs::metadata(p).ok()?;
            let t = m
                .modified()
                .ok()
                .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
                .map_or(0, |d| d.as_secs());
            Some((p.to_string_lossy().to_string(), m.len(), t))
        })
        .collect()
}

fn cache_path() -> PathBuf {
    dirs::data_dir()
        .unwrap_or_else(|| PathBuf::from("."))
        .join("mnm-cartographer")
        .join("zone-cache.json")
}

/// `survey`, but reusing the previous result when no bundle has changed.
pub fn survey_cached(dir: &Path, progress: impl FnMut(usize, usize, &str)) -> Vec<Located> {
    let now = stamps(dir);
    if let Ok(text) = std::fs::read_to_string(cache_path()) {
        if let Ok(c) = serde_json::from_str::<CachedSurvey>(&text) {
            if c.stamps == now && !c.zones.is_empty() {
                return c
                    .zones
                    .into_iter()
                    .map(|z| Located {
                        zone: z.zone,
                        bundle: PathBuf::from(z.bundle),
                        group: Group { parent: z.parent, subs: z.subs },
                    })
                    .collect();
            }
        }
    }
    let found = survey(dir, progress);
    let c = CachedSurvey {
        stamps: now,
        zones: found
            .iter()
            .map(|l| CachedZone {
                zone: l.zone.clone(),
                bundle: l.bundle.to_string_lossy().to_string(),
                parent: l.group.parent.clone(),
                subs: l.group.subs.clone(),
            })
            .collect(),
    };
    if let Ok(t) = serde_json::to_string(&c) {
        let p = cache_path();
        if let Some(d) = p.parent() {
            std::fs::create_dir_all(d).ok();
        }
        std::fs::write(p, t).ok();
    }
    found
}
