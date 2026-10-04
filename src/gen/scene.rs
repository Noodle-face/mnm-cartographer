//! Scene facts that are not geometry: where the sea surface sits, and where
//! the dressing props stand.

use super::bundle::*;
use std::collections::HashSet;

/// Zones place Crest water objects at the ocean surface, so the sea level can
/// be read off their transforms instead of guessed from the height histogram.
fn is_sea_object(name: &str) -> bool {
    let n = name.trim().to_lowercase();
    if n == "waves" || n == "ocean" {
        return true;
    }
    // crest<digits>_ocean / crest<digits>_shoreline
    let Some(rest) = n.strip_prefix("crest") else { return false };
    let rest = rest.trim_start_matches(|c: char| c.is_ascii_digit());
    rest == "_ocean" || rest == "_shoreline"
}

/// Sum a transform chain's local positions. The Python did the same: a plain
/// sum rather than a full matrix, which is exact for the unrotated, unscaled
/// parents these objects actually have.
fn chain_sum(env: &Env, start: (usize, usize)) -> [f64; 3] {
    let mut acc = [0.0; 3];
    let mut seen = HashSet::new();
    let mut cur = Some(start);
    while let Some(at) = cur {
        if !seen.insert(at) {
            break;
        }
        let Some(t) = env.read(at.0, at.1) else { break };
        let p = vec3(field(&t, "m_LocalPosition"), 0.0, 0.0, 0.0);
        for k in 0..3 {
            acc[k] += p[k];
        }
        cur = as_pptr(field(&t, "m_Father")).and_then(|p| env.resolve(at.0, p));
    }
    acc
}

fn transform_of(env: &Env, file: usize, go: &unity_rs_core::type_tree::TypeValue) -> Option<(usize, usize)> {
    use unity_rs_core::type_tree::TypeValue;
    let TypeValue::Array(comps) = field(go, "m_Component")? else { return None };
    for c in comps {
        let at = env.resolve(file, component_pptr(c)?)?;
        match env.class_id(at.0, at.1) {
            Some(CLASS_TRANSFORM) | Some(CLASS_RECTTRANSFORM) => return Some(at),
            _ => {}
        }
    }
    None
}

/// World Y of the ocean surface, or None for a zone with no sea.
pub fn find_sea_level(env: &Env, files: &[usize]) -> Option<f32> {
    let mut ys: Vec<f64> = Vec::new();
    for &fi in files {
        let n = env.col.serialized_files()[fi].file.objects.len();
        for oi in 0..n {
            if env.class_id(fi, oi) != Some(CLASS_GAMEOBJECT) {
                continue;
            }
            let Some(go) = env.read(fi, oi) else { continue };
            let Some(name) = as_str(field(&go, "m_Name")) else { continue };
            if !is_sea_object(name) {
                continue;
            }
            let Some(tr) = transform_of(env, fi, &go) else { continue };
            ys.push(chain_sum(env, tr)[1]);
        }
    }
    if ys.is_empty() {
        return None;
    }
    ys.sort_by(|a, b| a.partial_cmp(b).unwrap());
    Some(ys[ys.len() / 2] as f32)
}

// ------------------------------------------------------------------- props

/// Zones are dressed with real objects -- 570 palms, 46 tents and 41 campfires
/// in Shaded Dunes -- and their names say what they are. Drawing them as map
/// symbols is the difference between a terrain plot and a map.
fn prop_kind(name: &str) -> Option<&'static str> {
    // Strip a trailing " (12)" duplicate suffix.
    let n = name.trim();
    let n = match n.rfind(" (") {
        Some(i) if n.ends_with(')') && n[i + 2..n.len() - 1].chars().all(|c| c.is_ascii_digit()) => {
            &n[..i]
        }
        _ => n,
    };
    let n = n.trim().to_lowercase();
    if n.starts_with("palmtree") || n.starts_with("yuccabush") {
        Some("tree")
    } else if n.starts_with("barrelcactus") {
        Some("scrub")
    } else if n == "ashiratent" {
        Some("tent")
    } else if n == "campfire" {
        Some("fire")
    } else {
        None
    }
}

/// World XZ of props worth drawing, by kind.
pub fn scene_props(env: &Env, files: &[usize]) -> std::collections::BTreeMap<String, Vec<(f64, f64)>> {
    let mut out: std::collections::BTreeMap<String, Vec<(f64, f64)>> = Default::default();
    for &fi in files {
        let n = env.col.serialized_files()[fi].file.objects.len();
        for oi in 0..n {
            if env.class_id(fi, oi) != Some(CLASS_GAMEOBJECT) {
                continue;
            }
            let Some(go) = env.read(fi, oi) else { continue };
            let Some(name) = as_str(field(&go, "m_Name")) else { continue };
            let Some(kind) = prop_kind(name) else { continue };
            let Some(tr) = transform_of(env, fi, &go) else { continue };
            let p = chain_sum(env, tr);
            out.entry(kind.to_string()).or_default().push((p[0], p[2]));
        }
    }
    out
}

/// Keep one point per `spacing` world units: 570 palm symbols is a forest,
/// 60 well-spaced ones is a map.
pub fn thin(points: &[(f64, f64)], spacing: f64) -> Vec<(f64, f64)> {
    let mut sorted = points.to_vec();
    sorted.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
    let mut seen = HashSet::new();
    let mut keep = Vec::new();
    for (x, z) in sorted {
        let cell = ((x / spacing).round() as i64, (z / spacing).round() as i64);
        if seen.insert(cell) {
            keep.push((x, z));
        }
    }
    keep
}
