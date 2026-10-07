//! Turning a scene's mesh colliders into world-space walkable triangles.
//!
//! Geometry comes from colliders rather than renderers because a collider is
//! what you can actually stand on: it skips foliage cards, decals and the
//! decorative shells that would otherwise bury the floor plan. Triangles are
//! transformed through the full Transform parent chain, then classified by
//! their normal -- up-facing is floor, and down-facing ceilings are discarded
//! or they occlude everything beneath them when seen from above.

use super::bundle::*;
use super::materials::MatClass;
use std::collections::{HashMap, HashSet};
use unity_rs_core::mesh::{read_mesh_with_collection, MeshReadLimits};
use unity_rs_core::type_tree::TypeValue;

pub type Tri = [[f32; 3]; 3];

/// Column-major 4x4 as [rotation*scale | translation].
#[derive(Clone, Copy)]
pub struct Mat4 {
    pub m: [[f64; 3]; 3],
    pub t: [f64; 3],
}

impl Mat4 {
    pub const IDENTITY: Mat4 = Mat4 {
        m: [[1.0, 0.0, 0.0], [0.0, 1.0, 0.0], [0.0, 0.0, 1.0]],
        t: [0.0, 0.0, 0.0],
    };

    fn from_trs(p: [f64; 3], r: [f64; 4], s: [f64; 3]) -> Mat4 {
        let [x, y, z, w] = r;
        let n = (x * x + y * y + z * z + w * w).sqrt();
        let n = if n == 0.0 { 1.0 } else { n };
        let (x, y, z, w) = (x / n, y / n, z / n, w / n);
        let rot = [
            [1.0 - 2.0 * (y * y + z * z), 2.0 * (x * y - z * w), 2.0 * (x * z + y * w)],
            [2.0 * (x * y + z * w), 1.0 - 2.0 * (x * x + z * z), 2.0 * (y * z - x * w)],
            [2.0 * (x * z - y * w), 2.0 * (y * z + x * w), 1.0 - 2.0 * (x * x + y * y)],
        ];
        // Scale columns, matching Unity's R * S composition.
        let mut m = [[0.0f64; 3]; 3];
        for i in 0..3 {
            for j in 0..3 {
                m[i][j] = rot[i][j] * s[j];
            }
        }
        Mat4 { m, t: p }
    }

    fn mul(&self, o: &Mat4) -> Mat4 {
        let mut m = [[0.0f64; 3]; 3];
        for i in 0..3 {
            for j in 0..3 {
                m[i][j] = (0..3).map(|k| self.m[i][k] * o.m[k][j]).sum();
            }
        }
        let t = [
            (0..3).map(|k| self.m[0][k] * o.t[k]).sum::<f64>() + self.t[0],
            (0..3).map(|k| self.m[1][k] * o.t[k]).sum::<f64>() + self.t[1],
            (0..3).map(|k| self.m[2][k] * o.t[k]).sum::<f64>() + self.t[2],
        ];
        Mat4 { m, t }
    }

    fn apply(&self, v: [f32; 3]) -> [f32; 3] {
        let v = [v[0] as f64, v[1] as f64, v[2] as f64];
        [
            (self.m[0][0] * v[0] + self.m[0][1] * v[1] + self.m[0][2] * v[2] + self.t[0]) as f32,
            (self.m[1][0] * v[0] + self.m[1][1] * v[1] + self.m[1][2] * v[2] + self.t[1]) as f32,
            (self.m[2][0] * v[0] + self.m[2][1] * v[1] + self.m[2][2] * v[2] + self.t[2]) as f32,
        ]
    }

    pub fn det(&self) -> f64 {
        let m = &self.m;
        m[0][0] * (m[1][1] * m[2][2] - m[1][2] * m[2][1])
            - m[0][1] * (m[1][0] * m[2][2] - m[1][2] * m[2][0])
            + m[0][2] * (m[1][0] * m[2][1] - m[1][1] * m[2][0])
    }
}

/// Cached per-transform local TRS and parent, so a deep hierarchy is walked
/// once per node rather than once per collider beneath it.
#[derive(Default)]
pub struct Hierarchy {
    local: HashMap<(usize, usize), (Mat4, Option<(usize, usize)>)>,
    world: HashMap<(usize, usize), Mat4>,
}

impl Hierarchy {
    fn local_of(&mut self, env: &Env, at: (usize, usize)) -> (Mat4, Option<(usize, usize)>) {
        if let Some(v) = self.local.get(&at) {
            return *v;
        }
        let v = match env.read(at.0, at.1) {
            Some(t) => {
                let p = vec3(field(&t, "m_LocalPosition"), 0.0, 0.0, 0.0);
                let r = quat(field(&t, "m_LocalRotation"));
                let s = vec3(field(&t, "m_LocalScale"), 1.0, 1.0, 1.0);
                let father = as_pptr(field(&t, "m_Father"))
                    .and_then(|p| env.resolve(at.0, p));
                (Mat4::from_trs(p, r, s), father)
            }
            None => (Mat4::IDENTITY, None),
        };
        self.local.insert(at, v);
        v
    }

    pub fn world(&mut self, env: &Env, at: (usize, usize)) -> Mat4 {
        if let Some(m) = self.world.get(&at) {
            return *m;
        }
        // Collect the chain first; recursing would blow the stack on deep
        // prefab nesting, and a cycle in m_Father would never return.
        let mut chain = Vec::new();
        let mut seen = HashSet::new();
        let mut cur = Some(at);
        while let Some(c) = cur {
            if !seen.insert(c) {
                break;
            }
            let (local, father) = self.local_of(env, c);
            chain.push((c, local));
            if let Some(m) = self.world.get(&c) {
                // Known ancestor: splice its world matrix in and stop.
                let mut acc = *m;
                chain.pop();
                for (node, local) in chain.iter().rev() {
                    acc = acc.mul(local);
                    self.world.insert(*node, acc);
                }
                return self.world[&at];
            }
            cur = father;
        }
        let mut acc = Mat4::IDENTITY;
        for (node, local) in chain.iter().rev() {
            acc = acc.mul(local);
            self.world.insert(*node, acc);
        }
        acc
    }
}

/// The Transform attached to a GameObject, if it has one.
pub fn transform_at(env: &Env, file: usize, go: &TypeValue) -> Option<(usize, usize)> {
    transform_of(env, file, go)
}

fn transform_of(env: &Env, file: usize, go: &TypeValue) -> Option<(usize, usize)> {
    let TypeValue::Array(comps) = field(go, "m_Component")? else { return None };
    for c in comps {
        let p = component_pptr(c)?;
        let at = env.resolve(file, p)?;
        match env.class_id(at.0, at.1) {
            Some(CLASS_TRANSFORM) | Some(CLASS_RECTTRANSFORM) => return Some(at),
            _ => {}
        }
    }
    None
}

pub struct Floors {
    pub tris: Vec<Tri>,
    /// Per triangle, an index into `mat_table`, when tagging was asked for.
    pub mats: Vec<u16>,
    pub mat_table: Vec<MatClass>,
    /// Near-vertical faces of the zone's boundary geometry: the invisible
    /// walls that stop a player. See [`is_boundary_name`].
    pub walls: Vec<Tri>,
    /// World boxes (min x, max x, min y, max y, min z, max z) of solid
    /// blocking volumes: invisible walls, navmesh obstacles.
    pub blocks: Vec<[f64; 6]>,
    /// Colliders whose mesh lives in a bundle we did not open.
    pub unresolved: usize,
    pub stats: Stats,
}

#[derive(Default, Debug)]
pub struct Stats {
    pub colliders: usize,
    pub disabled: usize,
    pub no_gameobject: usize,
    pub no_transform: usize,
    pub ptr_unresolved: usize,
    pub mesh_read_failed: usize,
    pub mesh_empty: usize,
    pub decoded: usize,
    pub raw_tris: usize,
    pub terrain_tris: usize,
    /// Meshes found in a shared asset bundle rather than the zone's own.
    pub shared: usize,
    /// Colliders skipped as another zone's scenery; see [`backdrop`].
    pub backdrop: usize,
    /// Colliders on objects switched off in the scene.
    pub inactive: usize,
    pub reasons: std::collections::BTreeMap<String, usize>,
}

/// Scenery borrowed from other zones, as (file, object) ids of the
/// components beneath it.
///
/// A zone's scene carries its neighbours' land as backdrop, grouped under
/// objects named for it: "Shallow Shoals Facade" in Vale of Zintar,
/// "Sungreet Strand Facade" in Glass Flats, "NH_Distant_Facade_Prefab" in
/// Shaded Dunes. In Vale that was 3,148 of 5,358 colliders, and it set the
/// map's frame. A group counts if its name says "distant", or says "facade"
/// and holds at least ten colliders -- a handful under "Palace_Facade" is a
/// building front, not backdrop.
pub fn backdrop(env: &Env, files: &[usize]) -> HashSet<(usize, usize)> {
    let _t = super::Timer::start("    backdrop");
    const MIN_FACADE: usize = 10;
    let mut root_of: HashMap<(usize, usize), Option<((usize, usize), bool)>> = HashMap::new();
    let mut members: HashMap<(usize, usize), Vec<(usize, usize)>> = HashMap::new();
    let mut distant: HashSet<(usize, usize)> = HashSet::new();
    let mut colliders: HashMap<(usize, usize), usize> = HashMap::new();
    let mut names = Names::default();
    for &fi in files {
        let count = env.col.serialized_files()[fi].file.objects.len();
        for oi in 0..count {
            let Some(cls) = env.class_id(fi, oi) else { continue };
            let collider = cls == CLASS_MESHCOLLIDER || cls == CLASS_TERRAINCOLLIDER;
            if !collider && !LANDMARK_CLASSES.contains(&cls) { continue }
            let Some(c) = env.read(fi, oi) else { continue };
            let Some(go_at) = as_pptr(field(&c, "m_GameObject")).and_then(|p| env.resolve(fi, p))
            else { continue };
            let Some(go) = env.read(go_at.0, go_at.1) else { continue };
            let Some(tr) = transform_of(env, go_at.0, &go) else { continue };
            if leftover(&names.up(env, tr)) {
                // A leftover group stands alone; key it by its own component.
                members.entry((fi, oi)).or_default().push((fi, oi));
                distant.insert((fi, oi));
                continue;
            }
            let Some((root, is_distant)) = backdrop_root(env, tr, &mut root_of) else { continue };
            members.entry(root).or_default().push((fi, oi));
            if is_distant { distant.insert(root); }
            if collider { *colliders.entry(root).or_default() += 1 }
        }
    }
    members.into_iter()
        .filter(|(root, _)| distant.contains(root)
            || colliders.get(root).copied().unwrap_or(0) >= MIN_FACADE)
        .flat_map(|(_, m)| m)
        .collect()
}

/// Whether the top-level group an object sits in is named as left over from
/// development: "SunkenCryptOLD" and "Tests" in Ain Golot, a copy of Night
/// Harbor's geometry in Shaded Dunes ("TEMP_NIGHTHARBORGEO"), Night
/// Harbor's own "(Depreciated)_..._Terrain". Matched on the exact markings
/// designers use, case and all -- "Gold", "Rothold" and "Temple" are not
/// leftovers -- and only on the top-level name: "ShadedDunesTest" inside
/// "Geo" is the zone itself.
fn leftover(path: &[String]) -> bool {
    let Some(root) = path.first() else { return false };
    let r = root.as_str();
    r.ends_with("OLD") || r.contains("OLD_") || r.starts_with("TEMP")
        || r == "Tests" || r == "Test" || r == "Test Area"
        || r.to_lowercase().contains("depreciated") || r.to_lowercase().contains("deprecated")
}

/// The nearest transform at or above `tr` whose object is named as
/// backdrop, and whether the name says "distant".
fn backdrop_root(env: &Env, tr: (usize, usize),
                 cache: &mut HashMap<(usize, usize), Option<((usize, usize), bool)>>)
                 -> Option<((usize, usize), bool)> {
    if let Some(r) = cache.get(&tr) { return *r }
    let Some(t) = env.read(tr.0, tr.1) else { return None };
    let name = as_pptr(field(&t, "m_GameObject")).and_then(|p| env.resolve(tr.0, p))
        .and_then(|g| env.read(g.0, g.1))
        .and_then(|g| as_str(field(&g, "m_Name")).map(|s| s.to_lowercase()))
        .unwrap_or_default();
    // Mark before recursing: a cycle in m_Father then ends here.
    cache.insert(tr, None);
    let r = if name.contains("distant") {
        Some((tr, true))
    } else if name.contains("facade") {
        Some((tr, false))
    } else {
        as_pptr(field(&t, "m_Father"))
            .and_then(|p| env.resolve(tr.0, p))
            .and_then(|f| backdrop_root(env, f, cache))
    };
    cache.insert(tr, r);
    r
}

/// Whether the object at transform `tr` is active in the scene: it and every
/// object above it switched on. A switched-off branch is not in the game --
/// a crypt's unused rooms, an old copy of a building -- however complete its
/// geometry.
fn active_in_hierarchy(env: &Env, tr: (usize, usize),
                       cache: &mut HashMap<(usize, usize), bool>) -> bool {
    if let Some(&a) = cache.get(&tr) { return a }
    cache.insert(tr, true); // a cycle in m_Father ends here
    let Some(t) = env.read(tr.0, tr.1) else { return true };
    let own = as_pptr(field(&t, "m_GameObject")).and_then(|p| env.resolve(tr.0, p))
        .and_then(|g| env.read(g.0, g.1))
        .map_or(true, |g| as_i64(field(&g, "m_IsActive")).unwrap_or(1) != 0);
    let a = own && as_pptr(field(&t, "m_Father"))
        .and_then(|p| env.resolve(tr.0, p))
        .map_or(true, |f| active_in_hierarchy(env, f, cache));
    cache.insert(tr, a);
    a
}

/// Whether an object's name, or an ancestor's, marks it as the zone's edge:
/// invisible walls and blocking volumes. Designers name these freely --
/// "BoundryGeo & Blockers", "HarrisonOriginalBoundaryGeo", "GeoBlockers",
/// "NavMeshCubeObstacle", "Invisible Walls / Collision" -- so this matches
/// the words they share. Rain blockers stop rain, not players.
pub fn is_boundary_name(path: &[String]) -> bool {
    path.iter().any(|n| {
        let n = n.to_lowercase();
        !n.contains("rain")
            && ["bound", "blocker", "invisible", "navmeshcube", "blockingvolume",
                "navblocking", "zone collider", "zoneline"]
                .iter().any(|k| n.contains(k))
    })
}

/// Names up transform chains, each transform read once. Walking a chain
/// re-read and re-parsed every ancestor for every object, and the same few
/// thousand parents were parsed over and over: in Night Harbor that was 10
/// seconds a pass, and two passes made it two-fifths of a whole build.
#[derive(Default)]
pub struct Names {
    at: HashMap<(usize, usize), (String, Option<(usize, usize)>)>,
}

impl Names {
    fn entry(&mut self, env: &Env, tr: (usize, usize)) -> (String, Option<(usize, usize)>) {
        if let Some(e) = self.at.get(&tr) { return e.clone() }
        let t = env.read(tr.0, tr.1);
        let name = t.as_ref().and_then(|t| as_pptr(field(t, "m_GameObject")))
            .and_then(|p| env.resolve(tr.0, p))
            .and_then(|g| env.read(g.0, g.1))
            .and_then(|g| as_str(field(&g, "m_Name")).map(String::from))
            .unwrap_or_default();
        let father = t.as_ref().and_then(|t| as_pptr(field(t, "m_Father")))
            .and_then(|p| env.resolve(tr.0, p));
        self.at.insert(tr, (name.clone(), father));
        (name, father)
    }

    /// The name of the object at `tr` and of every object above it, the
    /// topmost first -- as [`names_up`], without re-reading.
    pub fn up(&mut self, env: &Env, tr: (usize, usize)) -> Vec<String> {
        let mut v = Vec::new();
        let mut cur = Some(tr);
        // A cycle in m_Father, or absurd nesting, stops here.
        for _ in 0..65 {
            let Some(at) = cur else { break };
            let (name, father) = self.entry(env, at);
            v.push(name);
            cur = father;
        }
        v.reverse();
        v
    }
}


/// Solid, enabled box colliders on boundary-named objects, as world boxes.
fn boundary_boxes(env: &Env, files: &[usize], hier: &mut Hierarchy,
                  active: &mut HashMap<(usize, usize), bool>) -> Vec<[f64; 6]> {
    let mut names = Names::default();
    let mut out = Vec::new();
    for &fi in files {
        let count = env.col.serialized_files()[fi].file.objects.len();
        for oi in 0..count {
            if env.class_id(fi, oi) != Some(CLASS_BOXCOLLIDER) { continue }
            let Some(bc) = env.read(fi, oi) else { continue };
            if as_i64(field(&bc, "m_Enabled")).unwrap_or(1) == 0 { continue }
            // A trigger is a zone line or an event, not a wall.
            if as_i64(field(&bc, "m_IsTrigger")).unwrap_or(0) != 0 { continue }
            let Some(go_at) = as_pptr(field(&bc, "m_GameObject")).and_then(|p| env.resolve(fi, p))
            else { continue };
            let Some(go) = env.read(go_at.0, go_at.1) else { continue };
            let Some(tr) = transform_of(env, go_at.0, &go) else { continue };
            if !active_in_hierarchy(env, tr, active) { continue }
            if !is_boundary_name(&names.up(env, tr)) { continue }
            let m = hier.world(env, tr);
            let sz = vec3(field(&bc, "m_Size"), 1.0, 1.0, 1.0);
            let c = vec3(field(&bc, "m_Center"), 0.0, 0.0, 0.0);
            out.push(world_box(&m, c, sz));
        }
    }
    out
}

const CLASS_BOXCOLLIDER: i32 = 65;

/// Up-facing world-space triangles for every mesh collider in `files`.
pub fn extract_floors(env: &Env, shared: &Shared, files: &[usize]) -> Floors {
    extract_faces(env, shared, files, 0.5)
}


/// As [`extract_floors`], optionally tagging what each triangle is made of, given the zone's
/// [`backdrop`] -- worked out once and shared with [`landmarks_with`].
pub fn extract_floors_with(env: &Env, shared: &Shared, files: &[usize], tagged: bool,
                           skip: &HashSet<(usize, usize)>) -> Floors {
    extract_inner(env, shared, files, 0.5, tagged, Some(skip))
}

/// World-space triangles whose normal has an upward component of at least
/// `min_ny`. Below -1 keeps every face, walls and ceilings included.
pub fn extract_faces(env: &Env, shared: &Shared, files: &[usize], min_ny: f32) -> Floors {
    extract_inner(env, shared, files, min_ny, false, None)
}

/// Interns material classes into a small table; triangles carry an index.
#[derive(Default)]
struct Tags {
    on: bool,
    mats: Vec<u16>,
    table: Vec<MatClass>,
    index: HashMap<MatClass, u16>,
    cache: HashMap<(usize, usize, usize), MatClass>,
}

impl Tags {
    fn id(&mut self, c: MatClass) -> u16 {
        if let Some(&i) = self.index.get(&c) { return i }
        let i = self.table.len() as u16;
        self.table.push(c);
        self.index.insert(c, i);
        i
    }
}

fn extract_inner(env: &Env, shared: &Shared, files: &[usize], min_ny: f32, tagged: bool,
                 skip: Option<&HashSet<(usize, usize)>>) -> Floors {
    let mut tags = Tags { on: tagged, ..Default::default() };
    let mut out: Vec<Tri> = Vec::new();
    let mut walls: Vec<Tri> = Vec::new();
    let mut unresolved = 0usize;
    let mut st = Stats::default();
    let mut hier = Hierarchy::default();
    let own_skip;
    let skip = match skip {
        Some(s) => s,
        None => { own_skip = backdrop(env, files); &own_skip }
    };
    let mut active = HashMap::new();
    let mut names = Names::default();

    for &fi in files {
        let count = env.col.serialized_files()[fi].file.objects.len();
        for oi in 0..count {
            if env.class_id(fi, oi) != Some(CLASS_MESHCOLLIDER) {
                continue;
            }
            st.colliders += 1;
            if skip.contains(&(fi, oi)) {
                st.backdrop += 1;
                continue;
            }
            let Some(mc) = env.read(fi, oi) else { continue };
            // m_Enabled is absent on some versions; absent means enabled.
            if as_i64(field(&mc, "m_Enabled")).unwrap_or(1) == 0 {
                st.disabled += 1;
                continue;
            }
            let Some(go_at) = as_pptr(field(&mc, "m_GameObject")).and_then(|p| env.resolve(fi, p))
            else {
                st.no_gameobject += 1;
                continue;
            };
            let Some(go) = env.read(go_at.0, go_at.1) else { st.no_gameobject += 1; continue };
            let Some(tr) = transform_of(env, go_at.0, &go) else { st.no_transform += 1; continue };
            if !active_in_hierarchy(env, tr, &mut active) {
                st.inactive += 1;
                continue;
            }
            if let Ok(skip_root) = std::env::var("MNM_SKIP_ROOT") {
                if ancestry(env, tr).iter().any(|n| n.contains(skip_root.as_str())) { continue }
            }
            // Boundary geometry marks the zone's edge; it is not ground, and
            // a boundary "ceiling" drawn as floor would blank the map.
            let boundary = is_boundary_name(&names.up(env, tr));

            // The mesh is in this bundle, or in a shared one.
            let Some(mp) = as_pptr(field(&mc, "m_Mesh")) else {
                unresolved += 1;
                st.ptr_unresolved += 1;
                continue;
            };
            let found = match env.resolve(fi, mp) {
                Some((f, o)) => Some((env, f, o)),
                None => env.external_cab(fi, mp)
                    .and_then(|cab| shared.find(cab, mp.path_id))
                    .inspect(|_| st.shared += 1),
            };
            let Some((menv, mfile, mobj)) = found else {
                unresolved += 1;
                st.ptr_unresolved += 1;
                continue;
            };
            let sf = &menv.col.serialized_files()[mfile].file;
            let mesh = match read_mesh_with_collection(
                &menv.col, sf, mobj, MeshReadLimits::default())
            {
                Ok(m) => m,
                Err(e) => {
                    unresolved += 1;
                    st.mesh_read_failed += 1;
                    let msg = e.to_string();
                    // Collapse the variable tail so reasons group.
                    let key = msg.split(':').next().unwrap_or(&msg).trim().to_string();
                    *st.reasons.entry(key).or_default() += 1;
                    continue;
                }
            };
            if mesh.vertices.is_empty() {
                st.mesh_empty += 1;
                continue;
            }
            st.decoded += 1;
            st.raw_tris += mesh.sub_meshes.iter().map(|s| s.indices.len() / 3).sum::<usize>();

            let m = hier.world(env, tr);
            let flip = m.det() < 0.0;
            let world: Vec<[f32; 3]> = mesh.vertices.iter().map(|v| m.apply(*v)).collect();

            // What each submesh is made of: the renderer's material in the
            // same slot when it draws this very mesh, else its first.
            let sub_tags: Vec<u16> = if tags.on {
                let (slots, same) = super::materials::collider_classes(
                    env, shared, go_at.0, &go, mp, &mut tags.cache);
                (0..mesh.sub_meshes.len()).map(|k| {
                    let c = if same { slots.get(k).or(slots.first()) } else { slots.first() };
                    let c = c.copied().unwrap_or_default();
                    tags.id(c)
                }).collect()
            } else { Vec::new() };

            for (si, sub) in mesh.sub_meshes.iter().enumerate() {
                for t in sub.indices.chunks_exact(3) {
                    let (a, b, c) = (t[0] as usize, t[1] as usize, t[2] as usize);
                    if a >= world.len() || b >= world.len() || c >= world.len() {
                        continue;
                    }
                    let (p, q, r) = (world[a], world[b], world[c]);
                    let u = [q[0] - p[0], q[1] - p[1], q[2] - p[2]];
                    let v = [r[0] - p[0], r[1] - p[1], r[2] - p[2]];
                    let n = [
                        u[1] * v[2] - u[2] * v[1],
                        u[2] * v[0] - u[0] * v[2],
                        u[0] * v[1] - u[1] * v[0],
                    ];
                    let len = (n[0] * n[0] + n[1] * n[1] + n[2] * n[2]).sqrt();
                    if len <= 1e-12 {
                        continue;
                    }
                    let mut ny = n[1] / len;
                    if flip {
                        ny = -ny;
                    }
                    if boundary {
                        if ny.abs() < 0.7 { walls.push([p, q, r]) }
                    } else if ny >= min_ny {
                        out.push([p, q, r]);
                        if tags.on { tags.mats.push(sub_tags[si]) }
                    }
                }
            }
        }
    }
    st.terrain_tris = terrain_faces(env, shared, files, min_ny, skip, &mut hier, &mut out, &mut tags);
    let blocks = boundary_boxes(env, files, &mut hier, &mut active);
    Floors { tris: out, walls, blocks, unresolved, stats: st, mats: tags.mats, mat_table: tags.table }
}

const CLASS_TERRAINCOLLIDER: i32 = 154;

/// Unity terrains: a height grid rather than a mesh, and what most outdoor
/// zones stand on. Two triangles per grid cell, kept by the same facing rule
/// as mesh colliders. Returns how many were added.
///
/// A terrain ignores its transform's rotation and scale; only the position
/// places it. Heights are stored as 0..32766 for 0..1 of the terrain's height.
#[allow(clippy::too_many_arguments)]
fn terrain_faces(env: &Env, shared: &Shared, files: &[usize], min_ny: f32, skip: &HashSet<(usize, usize)>,
                 hier: &mut Hierarchy, out: &mut Vec<Tri>, tags: &mut Tags) -> usize {
    let before = out.len();
    for &fi in files {
        let count = env.col.serialized_files()[fi].file.objects.len();
        for oi in 0..count {
            if env.class_id(fi, oi) != Some(CLASS_TERRAINCOLLIDER) { continue }
            if skip.contains(&(fi, oi)) { continue }
            let Some(tc) = env.read(fi, oi) else { continue };
            if as_i64(field(&tc, "m_Enabled")).unwrap_or(1) == 0 { continue }
            let Some(go_at) = as_pptr(field(&tc, "m_GameObject")).and_then(|p| env.resolve(fi, p))
            else { continue };
            let Some(go) = env.read(go_at.0, go_at.1) else { continue };
            let Some(tr) = transform_of(env, go_at.0, &go) else { continue };
            // A switched-off terrain is a leftover (Tel Ekir keeps one named
            // "Terrain (Depreciated)"), not ground anyone stands on.
            if !active_in_hierarchy(env, tr, &mut HashMap::new()) { continue }
            let origin = hier.world(env, tr).t;
            let Some(td_at) = as_pptr(field(&tc, "m_TerrainData")).and_then(|p| env.resolve(fi, p))
            else { continue };
            let Some(td) = env.read(td_at.0, td_at.1) else { continue };
            let tag = if tags.on {
                let c = super::materials::terrain_class(env, shared, td_at.0, &td);
                tags.id(c)
            } else { 0 };
            let Some(hm) = field(&td, "m_Heightmap") else { continue };
            let res = as_i64(field(hm, "m_Resolution")).unwrap_or(0) as usize;
            let scale = vec3(field(hm, "m_Scale"), 1.0, 1.0, 1.0);
            let Some(TypeValue::Array(hs)) = field(hm, "m_Heights") else { continue };
            if res < 2 || hs.len() < res * res { continue }
            let h: Vec<f64> = hs.iter()
                .map(|v| as_f64(Some(v)).unwrap_or(0.0) / 32766.0 * scale[1])
                .collect();
            // Rows run along z, columns along x.
            let at = |x: usize, z: usize| -> [f32; 3] {
                [(origin[0] + x as f64 * scale[0]) as f32,
                 (origin[1] + h[z * res + x]) as f32,
                 (origin[2] + z as f64 * scale[2]) as f32]
            };
            for z in 0..res - 1 {
                for x in 0..res - 1 {
                    let (a, b, c, d) = (at(x, z), at(x + 1, z), at(x, z + 1), at(x + 1, z + 1));
                    for t in [[a, c, b], [b, c, d]] {
                        if up_component(&t) >= min_ny {
                            out.push(t);
                            if tags.on { tags.mats.push(tag) }
                        }
                    }
                }
            }
        }
    }
    out.len() - before
}

/// The y component of a triangle's unit normal, for a triangle wound the way
/// Unity winds an up-facing one.
fn up_component(t: &Tri) -> f32 {
    let u = [t[1][0] - t[0][0], t[1][1] - t[0][1], t[1][2] - t[0][2]];
    let v = [t[2][0] - t[0][0], t[2][1] - t[0][1], t[2][2] - t[0][2]];
    let n = [u[1] * v[2] - u[2] * v[1], u[2] * v[0] - u[0] * v[2], u[0] * v[1] - u[1] * v[0]];
    let len = (n[0] * n[0] + n[1] * n[1] + n[2] * n[2]).sqrt();
    if len <= 1e-12 { -2.0 } else { n[1] / len }
}

/// One scene object, for inspecting what a zone holds besides its geometry.
#[derive(serde::Serialize)]
pub struct ObjInfo {
    pub name: String,
    pub pos: [f64; 3],
    /// Unity class ids of its components.
    pub classes: Vec<i32>,
    /// Class names of its scripts (MonoBehaviours).
    pub scripts: Vec<String>,
    /// World-space axis-aligned box of a BoxCollider, if it has one.
    pub boxw: Option<[f64; 6]>,
    pub trigger: bool,
    /// World-space box of an OcclusionArea, if it has one.
    pub occl: Option<[f64; 6]>,
    /// Names of its ancestors, root first.
    pub path: Vec<String>,
}

fn world_box(m: &Mat4, c: [f64; 3], s: [f64; 3]) -> [f64; 6] {
    let (mut lo, mut hi) = ([f64::MAX; 3], [f64::MIN; 3]);
    for k in 0..8 {
        let corner = [
            c[0] + s[0] * if k & 1 == 0 { -0.5 } else { 0.5 },
            c[1] + s[1] * if k & 2 == 0 { -0.5 } else { 0.5 },
            c[2] + s[2] * if k & 4 == 0 { -0.5 } else { 0.5 },
        ];
        let w = m.apply([corner[0] as f32, corner[1] as f32, corner[2] as f32]);
        for d in 0..3 {
            lo[d] = lo[d].min(w[d] as f64);
            hi[d] = hi[d].max(w[d] as f64);
        }
    }
    [lo[0], hi[0], lo[1], hi[1], lo[2], hi[2]]
}

/// Names of the GameObjects above the transform at `tr`, root first.
fn ancestry(env: &Env, tr: (usize, usize)) -> Vec<String> {
    let mut out = Vec::new();
    let mut cur = env.read(tr.0, tr.1)
        .and_then(|t| as_pptr(field(&t, "m_Father")))
        .and_then(|p| env.resolve(tr.0, p));
    let mut guard = 0;
    while let Some(at) = cur {
        guard += 1;
        if guard > 64 { break }
        let Some(t) = env.read(at.0, at.1) else { break };
        let name = as_pptr(field(&t, "m_GameObject")).and_then(|p| env.resolve(at.0, p))
            .and_then(|g| env.read(g.0, g.1))
            .and_then(|g| as_str(field(&g, "m_Name")).map(String::from))
            .unwrap_or_default();
        out.push(name);
        cur = as_pptr(field(&t, "m_Father")).and_then(|p| env.resolve(at.0, p));
    }
    out.reverse();
    out
}

/// Every GameObject in `files` with its world position, components and
/// scripts. A developer aid; generation does not use it.
pub fn objects(env: &Env, files: &[usize]) -> Vec<ObjInfo> {
    let mut hier = Hierarchy::default();
    let mut out = Vec::new();
    let mut script_names: HashMap<(usize, usize), String> = HashMap::new();
    for &fi in files {
        let n = env.col.serialized_files()[fi].file.objects.len();
        for oi in 0..n {
            if env.class_id(fi, oi) != Some(CLASS_GAMEOBJECT) { continue }
            let Some(go) = env.read(fi, oi) else { continue };
            let name = as_str(field(&go, "m_Name")).unwrap_or("").to_string();
            let Some(tr) = transform_of(env, fi, &go) else { continue };
            let m = hier.world(env, tr);
            let mut info = ObjInfo { name, pos: m.t, classes: vec![], scripts: vec![],
                                     boxw: None, trigger: false, occl: None,
                                     path: ancestry(env, tr) };
            if let Some(TypeValue::Array(comps)) = field(&go, "m_Component") {
                for c in comps {
                    let Some(at) = component_pptr(c).and_then(|p| env.resolve(fi, p)) else { continue };
                    let Some(cls) = env.class_id(at.0, at.1) else { continue };
                    info.classes.push(cls);
                    if cls == 114 {
                        let Some(mb) = env.read(at.0, at.1) else { continue };
                        let Some(sp) = as_pptr(field(&mb, "m_Script")).and_then(|p| env.resolve(at.0, p)) else { continue };
                        let nm = script_names.entry(sp).or_insert_with(|| {
                            env.read(sp.0, sp.1)
                                .and_then(|s| as_str(field(&s, "m_ClassName")).map(String::from))
                                .unwrap_or_default()
                        });
                        info.scripts.push(nm.clone());
                    }
                    if cls == 65 {
                        let Some(bc) = env.read(at.0, at.1) else { continue };
                        info.trigger |= as_i64(field(&bc, "m_IsTrigger")).unwrap_or(0) != 0;
                        let s = vec3(field(&bc, "m_Size"), 1.0, 1.0, 1.0);
                        let c = vec3(field(&bc, "m_Center"), 0.0, 0.0, 0.0);
                        info.boxw = Some(world_box(&m, c, s));
                    }
                    if cls == 192 {
                        let Some(oa) = env.read(at.0, at.1) else { continue };
                        let s = vec3(field(&oa, "m_Size"), 1.0, 1.0, 1.0);
                        let c = vec3(field(&oa, "m_Center"), 0.0, 0.0, 0.0);
                        info.occl = Some(world_box(&m, c, s));
                    }
                }
            }
            out.push(info);
        }
    }
    out
}

/// Describe the TerrainData behind each terrain collider: a developer aid.
pub fn terrain_probe(env: &Env, files: &[usize]) -> Vec<String> {
    let mut out = Vec::new();
    for &fi in files {
        let n = env.col.serialized_files()[fi].file.objects.len();
        for oi in 0..n {
            if env.class_id(fi, oi) != Some(154) { continue }
            let Some(tc) = env.read(fi, oi) else { out.push("collider unreadable".into()); continue };
            let td = as_pptr(field(&tc, "m_TerrainData"));
            let at = td.and_then(|p| env.resolve(fi, p));
            match at.and_then(|a| env.read(a.0, a.1).map(|v| (a, v))) {
                None => out.push(format!("collider {oi}: TerrainData {td:?} unresolved")),
                Some((a, v)) => {
                    let mut s = format!("collider {oi}: TerrainData class {:?}\n", env.class_id(a.0, a.1));
                    describe(&v, 0, &mut s);
                    out.push(s);
                }
            }
        }
    }
    out
}

fn describe(v: &TypeValue, depth: usize, s: &mut String) {
    if depth > 3 { return }
    match v {
        TypeValue::Object(fields) => {
            for f in fields.iter() {
                let (k, val) = (&f.name, &f.value);
                let kind = match val {
                    TypeValue::Array(a) => format!("array[{}]", a.len()),
                    TypeValue::Object(_) => "object".into(),
                    other => format!("{:.60}", format!("{other:?}")),
                };
                s.push_str(&format!("{}{k}: {kind}\n", "  ".repeat(depth + 1)));
                if matches!(val, TypeValue::Object(_)) { describe(val, depth + 1, s) }
            }
        }
        _ => {}
    }
}

/// Where a zone's content is, as opposed to its scenery.
#[derive(Default)]
pub struct Landmarks {
    /// World XZ of every visible or solid object: renderers, colliders,
    /// lights, particle systems.
    pub objects: Vec<[f64; 2]>,
    /// World XZ boxes (min_x, max_x, min_z, max_z) of OcclusionAreas, which
    /// level designers draw around where the camera can go.
    pub occlusion: Vec<[f64; 4]>,
}

/// Components whose presence marks a spot as somewhere the zone has content.
const LANDMARK_CLASSES: [i32; 7] = [
    23,  // MeshRenderer
    64,  // MeshCollider
    65,  // BoxCollider
    108, // Light
    135, // SphereCollider
    136, // CapsuleCollider
    198, // ParticleSystem
];
const CLASS_OCCLUSIONAREA: i32 = 192;


/// Where the zone's content is, leaving out the [`backdrop`] objects in `skip`.
pub fn landmarks_with(env: &Env, files: &[usize], skip: &HashSet<(usize, usize)>) -> Landmarks {
    let mut hier = Hierarchy::default();
    let mut lm = Landmarks::default();
    // A neighbour's scenery carries its props too; they mark where the
    // neighbour is, not this zone.
    for &fi in files {
        let count = env.col.serialized_files()[fi].file.objects.len();
        for oi in 0..count {
            let Some(cls) = env.class_id(fi, oi) else { continue };
            let occl = cls == CLASS_OCCLUSIONAREA;
            if !occl && !LANDMARK_CLASSES.contains(&cls) { continue }
            if skip.contains(&(fi, oi)) { continue }
            let Some(c) = env.read(fi, oi) else { continue };
            let Some(go_at) = as_pptr(field(&c, "m_GameObject")).and_then(|p| env.resolve(fi, p))
            else { continue };
            let Some(go) = env.read(go_at.0, go_at.1) else { continue };
            let Some(tr) = transform_of(env, go_at.0, &go) else { continue };
            let m = hier.world(env, tr);
            if occl {
                let s = vec3(field(&c, "m_Size"), 1.0, 1.0, 1.0);
                let ctr = vec3(field(&c, "m_Center"), 0.0, 0.0, 0.0);
                let b = world_box(&m, ctr, s);
                lm.occlusion.push([b[0], b[1], b[4], b[5]]);
            } else if m.t[0] != 0.0 || m.t[2] != 0.0 {
                // Managers and empty roots sit at the origin; they say nothing
                // about where the zone is.
                lm.objects.push([m.t[0], m.t[2]]);
            }
        }
    }
    lm
}
