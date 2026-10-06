//! What a zone's walkable surfaces are made of: an inventory, for working
//! out how to colour maps by material. A developer aid; generation does not
//! use it yet.
//!
//! For each walkable mesh collider, the materials of the renderer on the same
//! object (colliders without one are counted as such), weighted by the
//! up-facing area of the collider. For each terrain, its ground layers,
//! weighted by how much of the terrain each layer covers.

use super::bundle::*;
use std::collections::{BTreeMap, HashMap};
use unity_rs_core::material::{read_material, MaterialReadLimits};
use unity_rs_core::mesh::{read_mesh_with_collection, MeshReadLimits};
use unity_rs_core::texture::{read_texture2d, TextureReadLimits};
use unity_rs_core::type_tree::TypeValue;

const CLASS_MESHCOLLIDER: i32 = 64;
const CLASS_MESHRENDERER: i32 = 23;
const CLASS_TERRAINCOLLIDER: i32 = 154;

/// An object anywhere in the zone's bundle or the shared ones.
#[derive(Clone, Copy)]
struct At<'a> { env: &'a Env, file: usize, obj: usize }

fn resolve<'a>(env: &'a Env, shared: &'a Shared, file: usize, p: PPtr) -> Option<At<'a>> {
    if let Some((f, o)) = env.resolve(file, p) { return Some(At { env, file: f, obj: o }) }
    let cab = env.external_cab(file, p)?;
    let (e, f, o) = shared.find(cab, p.path_id)?;
    Some(At { env: e, file: f, obj: o })
}

#[derive(Default, serde::Serialize)]
pub struct MaterialUse {
    pub name: String,
    pub shader: String,
    /// Texture names bound to the material, by property (_MainTex, ...).
    pub textures: BTreeMap<String, String>,
    /// _Color / _BaseColor, if set.
    pub color: Option<[f32; 4]>,
    /// Up-facing (walkable) area of the colliders it is on, square units.
    pub area: f64,
    pub colliders: usize,
    /// Average colour of the main texture's top mip, and its spread.
    pub tex_rgb: Option<[f32; 3]>,
    pub tex_sd: Option<f32>,
    pub tex_format: Option<String>,
    pub tex_size: Option<[u32; 2]>,
}

#[derive(Default, serde::Serialize)]
pub struct Inventory {
    /// Keyed by "cab:path_id" so the same material shared by zones matches.
    pub materials: BTreeMap<String, MaterialUse>,
    /// Walkable area on colliders with no renderer on the same object.
    pub no_renderer_area: f64,
    pub total_area: f64,
    /// Terrain layer name -> (diffuse texture, covered area).
    pub terrain_layers: BTreeMap<String, (String, f64)>,
    pub terrain_area: f64,
    pub errors: BTreeMap<String, usize>,
}

fn up_area(env: &Env, shared: &Shared, mesh_at: At, m: &[[f64; 3]; 3], flip: bool) -> Option<f64> {
    let _ = env;
    let sf = &mesh_at.env.col.serialized_files()[mesh_at.file].file;
    let mesh = read_mesh_with_collection(&mesh_at.env.col, sf, mesh_at.obj, MeshReadLimits::default()).ok()?;
    let _ = shared;
    let app = |v: [f32; 3]| -> [f64; 3] {
        let v = [v[0] as f64, v[1] as f64, v[2] as f64];
        [m[0][0] * v[0] + m[0][1] * v[1] + m[0][2] * v[2],
         m[1][0] * v[0] + m[1][1] * v[1] + m[1][2] * v[2],
         m[2][0] * v[0] + m[2][1] * v[1] + m[2][2] * v[2]]
    };
    let w: Vec<[f64; 3]> = mesh.vertices.iter().map(|v| app(*v)).collect();
    let mut area = 0.0;
    for sub in &mesh.sub_meshes {
        for t in sub.indices.chunks_exact(3) {
            let (a, b, c) = (t[0] as usize, t[1] as usize, t[2] as usize);
            if a >= w.len() || b >= w.len() || c >= w.len() { continue }
            let u = [w[b][0] - w[a][0], w[b][1] - w[a][1], w[b][2] - w[a][2]];
            let v = [w[c][0] - w[a][0], w[c][1] - w[a][1], w[c][2] - w[a][2]];
            let n = [u[1] * v[2] - u[2] * v[1], u[2] * v[0] - u[0] * v[2], u[0] * v[1] - u[1] * v[0]];
            let len = (n[0] * n[0] + n[1] * n[1] + n[2] * n[2]).sqrt();
            if len < 1e-12 { continue }
            let ny = if flip { -n[1] } else { n[1] } / len;
            if ny >= 0.5 { area += 0.5 * n[1].abs() }
        }
    }
    Some(area)
}

fn name_of(at: At) -> String {
    at.env.read(at.file, at.obj)
        .and_then(|v| as_str(field(&v, "m_Name")).map(String::from))
        .unwrap_or_default()
}

/// Average colour (0..1) and luminance spread of a texture's top mip.
fn tex_stats(at: At) -> Result<([f32; 3], f32, String, [u32; 2]), String> {
    let sf = &at.env.col.serialized_files()[at.file].file;
    let mut lim = TextureReadLimits::default();
    lim.maximum_output_bytes = 512 * 1024 * 1024;
    let t = read_texture2d(&at.env.col, sf, at.obj, lim).map_err(|e| e.to_string())?;
    let img = t.decode_mip_rgba8(0, lim).map_err(|e| e.to_string())?;
    let px = img.pixels.chunks_exact(4);
    let n = (img.pixels.len() / 4).max(1) as f64;
    // Every 7th pixel is plenty for a mean.
    let (mut s, mut l, mut l2, mut k) = ([0f64; 3], 0f64, 0f64, 0f64);
    for p in px.step_by(7) {
        for c in 0..3 { s[c] += p[c] as f64 }
        let lum = 0.299 * p[0] as f64 + 0.587 * p[1] as f64 + 0.114 * p[2] as f64;
        l += lum; l2 += lum * lum; k += 1.0;
    }
    let _ = n;
    let k = k.max(1.0);
    let mean = [(s[0] / k / 255.0) as f32, (s[1] / k / 255.0) as f32, (s[2] / k / 255.0) as f32];
    let sd = (((l2 / k) - (l / k).powi(2)).max(0.0).sqrt() / 255.0) as f32;
    Ok((mean, sd, format!("{:?}", t.format), [t.width, t.height]))
}

pub fn inventory(env: &Env, shared: &Shared, files: &[usize]) -> Inventory {
    let mut inv = Inventory::default();
    let mut hier = super::extract::Hierarchy::default();
    let mut tex_cache: HashMap<(usize, usize, usize), Result<([f32; 3], f32, String, [u32; 2]), String>> = HashMap::new();
    let err = |inv: &mut Inventory, k: &str| *inv.errors.entry(k.to_string()).or_default() += 1;

    for &fi in files {
        let count = env.col.serialized_files()[fi].file.objects.len();
        for oi in 0..count {
            let cls = env.class_id(fi, oi);
            if cls == Some(CLASS_TERRAINCOLLIDER) {
                terrain(env, shared, fi, oi, &mut inv);
                continue;
            }
            if cls != Some(CLASS_MESHCOLLIDER) { continue }
            let Some(mc) = env.read(fi, oi) else { continue };
            let Some(go_at) = as_pptr(field(&mc, "m_GameObject")).and_then(|p| env.resolve(fi, p)) else { continue };
            let Some(go) = env.read(go_at.0, go_at.1) else { continue };
            let Some(tr) = super::extract::transform_at(env, go_at.0, &go) else { continue };
            let Some(mesh_at) = as_pptr(field(&mc, "m_Mesh")).and_then(|p| resolve(env, shared, fi, p)) else {
                err(&mut inv, "collider mesh unresolved"); continue
            };
            let m = hier.world(env, tr);
            let Some(area) = up_area(env, shared, mesh_at, &m.m, m.det() < 0.0) else {
                err(&mut inv, "mesh read failed"); continue
            };
            if area <= 0.0 { continue }
            inv.total_area += area;

            // The renderer on the same object, if any.
            let mut mats: Vec<PPtr> = Vec::new();
            if let Some(TypeValue::Array(comps)) = field(&go, "m_Component") {
                for c in comps {
                    let Some(at) = component_pptr(c).and_then(|p| env.resolve(go_at.0, p)) else { continue };
                    if env.class_id(at.0, at.1) != Some(CLASS_MESHRENDERER) { continue }
                    let Some(r) = env.read(at.0, at.1) else { continue };
                    if let Some(TypeValue::Array(ms)) = field(&r, "m_Materials") {
                        mats = ms.iter().filter_map(|m| as_pptr(Some(m))).filter(|p| !p.is_null()).collect();
                    }
                    // Pointers are relative to the renderer's own file.
                    if at.0 != go_at.0 { err(&mut inv, "renderer in another file") }
                }
            }
            if mats.is_empty() { inv.no_renderer_area += area; continue }
            let share = area / mats.len() as f64;
            for p in mats {
                let Some(ma) = resolve(env, shared, go_at.0, p) else { err(&mut inv, "material unresolved"); continue };
                let key = format!("{}:{}", ma.env.col.serialized_files()[ma.file].path.rsplit('/').next().unwrap_or(""),
                                  ma.env.col.serialized_files()[ma.file].file.objects[ma.obj].path_id);
                if !inv.materials.contains_key(&key) {
                    let sf = &ma.env.col.serialized_files()[ma.file].file;
                    let mut mu = MaterialUse::default();
                    match read_material(sf, ma.obj, MaterialReadLimits::default()) {
                        Ok(mat) => {
                            mu.name = mat.name.clone();
                            let shp = PPtr { file: mat.shader.file_id as i64, path_id: mat.shader.path_id };
                            mu.shader = resolve(ma.env, shared, ma.file, shp).map(name_of).unwrap_or_default();
                            for c in &mat.saved_properties.colors {
                                if c.name == "_Color" || c.name == "_BaseColor" { mu.color = Some(c.value) }
                            }
                            let mut main: Option<At> = None;
                            for t in &mat.saved_properties.texture_environments {
                                let tp = PPtr { file: t.value.texture.file_id as i64, path_id: t.value.texture.path_id };
                                if tp.is_null() { continue }
                                let Some(ta) = resolve(ma.env, shared, ma.file, tp) else { continue };
                                mu.textures.insert(t.name.clone(), name_of(ta));
                                let is_main = ["_BaseLayerDiffuse", "_MainTex", "_BaseMap", "_BaseColorMap", "_Albedo", "_Diffuse"]
                                    .contains(&t.name.as_str());
                                if is_main || main.is_none() && t.name.to_lowercase().contains("albedo") { main = Some(ta) }
                            }
                            if let Some(ta) = main {
                                let k = (ta.env as *const Env as usize, ta.file, ta.obj);
                                let r = tex_cache.entry(k).or_insert_with(|| tex_stats(ta)).clone();
                                match r {
                                    Ok((rgb, sd, fmt, sz)) => {
                                        mu.tex_rgb = Some(rgb); mu.tex_sd = Some(sd);
                                        mu.tex_format = Some(fmt); mu.tex_size = Some(sz);
                                    }
                                    Err(e) => err(&mut inv, &format!("texture decode: {}", e.split(':').next().unwrap_or(&e))),
                                }
                            }
                        }
                        Err(e) => { err(&mut inv, &format!("material read: {}", e.to_string().split(':').next().unwrap_or(""))) }
                    }
                    inv.materials.insert(key.clone(), mu);
                }
                let mu = inv.materials.get_mut(&key).unwrap();
                mu.area += share;
                mu.colliders += 1;
            }
        }
    }
    inv
}

/// A terrain's ground layers, weighted by their share of its alpha maps.
fn terrain(env: &Env, shared: &Shared, fi: usize, oi: usize, inv: &mut Inventory) {
    let Some(tc) = env.read(fi, oi) else { return };
    let Some(td_at) = as_pptr(field(&tc, "m_TerrainData")).and_then(|p| resolve(env, shared, fi, p)) else { return };
    let Some(td) = td_at.env.read(td_at.file, td_at.obj) else { return };
    let hm = field(&td, "m_Heightmap");
    let scale = vec3(hm.and_then(|h| field(h, "m_Scale")), 1.0, 1.0, 1.0);
    let res = as_i64(hm.and_then(|h| field(h, "m_Resolution"))).unwrap_or(1) as f64;
    let area = (res - 1.0).max(0.0).powi(2) * scale[0] * scale[2];
    inv.terrain_area += area;
    let Some(sd) = field(&td, "m_SplatDatabase") else { return };
    let mut names: Vec<(String, String)> = Vec::new();
    if let Some(TypeValue::Array(ls)) = field(sd, "m_TerrainLayers") {
        for l in ls {
            let Some(la) = as_pptr(Some(l)).and_then(|p| resolve(td_at.env, shared, td_at.file, p)) else {
                names.push(("?".into(), String::new())); continue
            };
            let lv = la.env.read(la.file, la.obj);
            let lname = lv.as_ref().and_then(|v| as_str(field(v, "m_Name")).map(String::from)).unwrap_or_default();
            let tex = lv.as_ref().and_then(|v| as_pptr(field(v, "m_DiffuseTexture")))
                .and_then(|p| resolve(la.env, shared, la.file, p)).map(name_of).unwrap_or_default();
            names.push((lname, tex));
        }
    }
    // Share of each layer: without decoding the alpha maps, split evenly; the
    // alpha maps hold the real split, worth reading if terrains matter.
    let n = names.len().max(1) as f64;
    for (lname, tex) in names {
        let e = inv.terrain_layers.entry(lname).or_insert((tex, 0.0));
        e.1 += area / n;
    }
}

// ------------------------------------------------------- classification
//
// The painted style draws each kind of ground in its own colours. The game's
// own colours are no use for that: its main shader blends up to three
// greyish textures and tints them at render time, so a texture's average is
// grey for grass and black for water. Names, though, are reliable -- an
// inventory of every zone sorted 99.7% of walkable area by keyword, the rest
// being two placeholder materials -- so surfaces are sorted by name into a
// handful of classes, and the painter supplies the look.

pub const UNKNOWN: u8 = 0;
pub const SAND: u8 = 1;
pub const ROCK: u8 = 2;
pub const GRASS: u8 = 3;
pub const DIRT: u8 = 4;
pub const MUD: u8 = 5;
pub const SNOW: u8 = 6;
pub const LAVA: u8 = 7;
pub const WATER: u8 = 8;
pub const WOOD: u8 = 9;
pub const STONE: u8 = 10; // built: floors, tiles, walls
pub const METAL: u8 = 11;
pub const CLOTH: u8 = 12;

/// A surface's base class and the classes of up to two blend layers mixed
/// into it (UNKNOWN where there is none).
#[derive(Clone, Copy, PartialEq, Eq, Hash, Default, Debug, serde::Serialize)]
pub struct MatClass {
    pub base: u8,
    pub blend: [u8; 2],
}

/// Split "FantasyDesertSand_01" into lowercase words: fantasy desert sand 01.
fn words(s: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut cur = String::new();
    let mut prev_lower = false;
    for ch in s.chars() {
        if !ch.is_alphanumeric() {
            if !cur.is_empty() { out.push(std::mem::take(&mut cur)) }
            prev_lower = false;
            continue;
        }
        if ch.is_uppercase() && prev_lower && !cur.is_empty() { out.push(std::mem::take(&mut cur)) }
        prev_lower = ch.is_lowercase();
        cur.extend(ch.to_lowercase());
    }
    if !cur.is_empty() { out.push(cur) }
    out
}

/// The class a name says, or UNKNOWN.
pub fn classify(name: &str) -> u8 {
    let w = words(name);
    let joined = w.join(" ");
    let has = |keys: &[&str]| keys.iter().any(|k| joined.contains(k));
    let word = |keys: &[&str]| w.iter().any(|x| keys.contains(&x.as_str()));
    // Placeholder and debug materials say nothing about the ground.
    if has(&["probuilder", "gridbox", "flatred", "matteblack", "default"]) { return UNKNOWN }
    // A cliff texture is reused freely -- Underdocks' generic cliff is
    // "Ice_Cliff" -- so "cliff" outranks whatever else the name says.
    if has(&["cliff"]) { return ROCK }
    if has(&["water", "ocean", "lake", "river", "pond"]) || word(&["sea"]) { return WATER }
    if has(&["lava", "magma", "volcan"]) { return LAVA }
    if has(&["snow", "frost", "glacier"]) || word(&["ice", "icy"]) { return SNOW }
    if has(&["sand", "dune", "beach"]) { return SAND }
    if has(&["grass", "meadow", "moss", "lawn", "turf", "leaf", "leaves", "foliage", "bush",
             "tree", "pine", "branch", "fern"]) { return GRASS }
    if has(&["mud", "swamp", "bog", "marsh"]) { return MUD }
    if has(&["dirt", "soil", "path", "road", "ground", "earth", "scree", "gravel", "salt", "loam"]) {
        return DIRT
    }
    let built = ["brick", "tile", "floor", "cobble", "paving", "flagstone", "ruin", "castle",
                 "temple", "pillar", "column", "stair", "marble", "masonry", "crypt", "tomb",
                 "concrete", "citadel"];
    // A cliff named "Zonewall" is rock; a "StoneWall" is built.
    if has(&["rock", "cliff", "boulder", "mountain", "cave", "crag", "layered"]) { return ROCK }
    if has(&built) || (has(&["stone"]) && has(&["wall", "floor", "tile", "brick"])) || word(&["wall", "walls", "keep", "fort"]) {
        return STONE
    }
    if has(&["stone"]) { return ROCK }
    if has(&["wood", "plank", "timber", "dock", "pier", "bridge", "crate", "barrel", "ship",
             "boat", "table", "tent"]) || word(&["log", "logs"]) { return WOOD }
    if has(&["metal", "iron", "steel", "bronze", "gold"]) { return METAL }
    if has(&["cloth", "fabric", "carpet", "banner"]) || word(&["rug"]) { return CLOTH }
    UNKNOWN
}

/// Classes of one material: from its layer textures when it uses the
/// game's layered blend shader, else from its name and main texture.
fn class_of_material(env: &Env, shared: &Shared, at: At) -> MatClass {
    let sf = &at.env.col.serialized_files()[at.file].file;
    let Ok(mat) = read_material(sf, at.obj, MaterialReadLimits::default()) else {
        return MatClass { base: classify(&name_of(at)), blend: [UNKNOWN; 2] };
    };
    let _ = env;
    let tex = |prop: &str| -> Option<String> {
        let t = mat.saved_properties.texture_environments.iter().find(|t| t.name == prop)?;
        let p = PPtr { file: t.value.texture.file_id as i64, path_id: t.value.texture.path_id };
        if p.is_null() { return None }
        resolve(at.env, shared, at.file, p).map(name_of)
    };
    let by = |prop: &str| tex(prop).map_or(UNKNOWN, |n| classify(&n));
    let own = classify(&mat.name);
    let base = match by("_BaseLayerDiffuse") {
        UNKNOWN => match own {
            UNKNOWN => ["_MainTex", "_BaseMap", "_BaseColorMap"].iter()
                .map(|p| by(p)).find(|c| *c != UNKNOWN).unwrap_or(UNKNOWN),
            c => c,
        },
        c => c,
    };
    let b1 = by("_BlendLayer1Diffuse");
    let b2 = by("_BlendLayer2Diffuse");
    // A blend layer of the same class as the base adds nothing.
    let clean = |c: u8| if c == base { UNKNOWN } else { c };
    MatClass { base, blend: [clean(b1), clean(b2)] }
}

/// For a mesh collider's GameObject: the class of each of its renderer's
/// material slots, and whether the renderer draws the very mesh the collider
/// uses -- in which case submesh i of the collider is drawn with slot i.
pub fn collider_classes(env: &Env, shared: &Shared, go_file: usize, go: &TypeValue,
                        collider_mesh: PPtr,
                        cache: &mut HashMap<(usize, usize, usize), MatClass>) -> (Vec<MatClass>, bool) {
    const CLASS_MESHFILTER: i32 = 33;
    let mut slots = Vec::new();
    let mut same_mesh = false;
    let Some(TypeValue::Array(comps)) = field(go, "m_Component") else { return (slots, false) };
    for c in comps {
        let Some(at) = component_pptr(c).and_then(|p| env.resolve(go_file, p)) else { continue };
        match env.class_id(at.0, at.1) {
            Some(CLASS_MESHRENDERER) => {
                let Some(r) = env.read(at.0, at.1) else { continue };
                if let Some(TypeValue::Array(ms)) = field(&r, "m_Materials") {
                    for m in ms {
                        let cls = as_pptr(Some(m)).filter(|p| !p.is_null())
                            .and_then(|p| resolve(env, shared, at.0, p))
                            .map(|ma| *cache.entry((ma.env as *const Env as usize, ma.file, ma.obj))
                                .or_insert_with(|| class_of_material(env, shared, ma)))
                            .unwrap_or_default();
                        slots.push(cls);
                    }
                }
            }
            Some(CLASS_MESHFILTER) => {
                if let Some(f) = env.read(at.0, at.1) {
                    same_mesh = as_pptr(field(&f, "m_Mesh")) == Some(collider_mesh);
                }
            }
            _ => {}
        }
    }
    (slots, same_mesh)
}

/// A terrain's classes, from its ground layers: the first is the base.
pub fn terrain_class(env: &Env, shared: &Shared, file: usize, td: &TypeValue) -> MatClass {
    let mut cls: Vec<u8> = Vec::new();
    if let Some(TypeValue::Array(ls)) = field(td, "m_SplatDatabase").and_then(|s| field(s, "m_TerrainLayers")) {
        for l in ls {
            let Some(la) = as_pptr(Some(l)).and_then(|p| resolve(env, shared, file, p)) else { continue };
            let lv = la.env.read(la.file, la.obj);
            let mut c = lv.as_ref().and_then(|v| as_pptr(field(v, "m_DiffuseTexture")))
                .and_then(|p| resolve(la.env, shared, la.file, p))
                .map_or(UNKNOWN, |t| classify(&name_of(t)));
            if c == UNKNOWN { c = classify(&name_of(la)) }
            if c != UNKNOWN && !cls.contains(&c) { cls.push(c) }
        }
    }
    MatClass {
        base: cls.first().copied().unwrap_or(UNKNOWN),
        blend: [cls.get(1).copied().unwrap_or(UNKNOWN), cls.get(2).copied().unwrap_or(UNKNOWN)],
    }
}
