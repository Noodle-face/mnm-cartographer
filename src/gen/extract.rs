//! Turning a scene's mesh colliders into world-space walkable triangles.
//!
//! Geometry comes from colliders rather than renderers because a collider is
//! what you can actually stand on: it skips foliage cards, decals and the
//! decorative shells that would otherwise bury the floor plan. Triangles are
//! transformed through the full Transform parent chain, then classified by
//! their normal -- up-facing is floor, and down-facing ceilings are discarded
//! or they occlude everything beneath them when seen from above.

use super::bundle::*;
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

    fn det(&self) -> f64 {
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
    pub reasons: std::collections::BTreeMap<String, usize>,
}

/// Up-facing world-space triangles for every mesh collider in `files`.
pub fn extract_floors(env: &Env, files: &[usize]) -> Floors {
    let mut out: Vec<Tri> = Vec::new();
    let mut unresolved = 0usize;
    let mut st = Stats::default();
    let mut hier = Hierarchy::default();

    for &fi in files {
        let count = env.col.serialized_files()[fi].file.objects.len();
        for oi in 0..count {
            if env.class_id(fi, oi) != Some(CLASS_MESHCOLLIDER) {
                continue;
            }
            st.colliders += 1;
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

            let Some(mesh_at) = as_pptr(field(&mc, "m_Mesh")).and_then(|p| env.resolve(fi, p))
            else {
                unresolved += 1;
                st.ptr_unresolved += 1;
                continue;
            };
            let sf = &env.col.serialized_files()[mesh_at.0].file;
            let mesh = match read_mesh_with_collection(
                &env.col, sf, mesh_at.1, MeshReadLimits::default())
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

            for sub in &mesh.sub_meshes {
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
                    if ny >= 0.5 {
                        out.push([p, q, r]);
                    }
                }
            }
        }
    }
    Floors { tris: out, unresolved, stats: st }
}
