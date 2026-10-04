//! Opening a Monsters & Memories Addressables bundle.
//!
//! Three things make these awkward, all of them learned the hard way:
//!
//! 1. Every bundle carries a custom 8-byte `\0MnM\1` header before the standard
//!    `UnityFS` magic. Standard tools reject them until those bytes are gone;
//!    underneath it is an ordinary archive. We skip the header with a subregion
//!    rather than copying -- these files run to 2 GB.
//! 2. The scene -> file mapping lives in the AssetBundle object's
//!    `m_SceneHashes`, which is a *map*, not an array.
//! 3. Path IDs are per-serialized-file. A bundle holds a dozen or more files
//!    and the same id means different objects in each, so every pointer has to
//!    be resolved through the owning file's external table. This is not an edge
//!    case: in a typical zone only ~2% of mesh pointers are same-file.

use anyhow::{anyhow, Context, Result};
use std::collections::HashMap;
use std::io::Read;
use std::path::Path;
use unity_rs_core::loader::{AssetCollection, AssetLoadLimits};
use unity_rs_core::source::Region;
use unity_rs_core::type_tree::TypeValue;

pub const MAGIC: &[u8; 8] = b"\x00MnM\x01\x00\x00\x00";

pub const CLASS_GAMEOBJECT: i32 = 1;
pub const CLASS_TRANSFORM: i32 = 4;
pub const CLASS_MESHCOLLIDER: i32 = 64;
pub const CLASS_ASSETBUNDLE: i32 = 142;
pub const CLASS_RECTTRANSFORM: i32 = 224;

/// A pointer as Unity serializes it: which file, and which object in it.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub struct PPtr {
    pub file: i64,
    pub path_id: i64,
}

impl PPtr {
    pub fn is_null(&self) -> bool {
        self.path_id == 0
    }
}

pub struct Env {
    pub col: AssetCollection,
    /// Per serialized file: path_id -> index into that file's object table.
    index: Vec<HashMap<i64, usize>>,
    /// CAB name (the part after `::`) -> serialized file index.
    by_cab: HashMap<String, usize>,
    /// Per file: resolved external table, as indices into `col`.
    externals: Vec<Vec<Option<usize>>>,
}

/// `archive:/CAB-aaa/CAB-bbb` and `<bundle>::CAB-bbb` both key on the tail.
fn cab_of(path: &str) -> &str {
    let after = path.rsplit("::").next().unwrap_or(path);
    after.rsplit('/').next().unwrap_or(after)
}

impl Env {
    pub fn open(path: &Path) -> Result<Self> {
        let len = std::fs::metadata(path)
            .with_context(|| format!("stat {}", path.display()))?
            .len();
        let mut head = [0u8; 8];
        let magic = std::fs::File::open(path)
            .and_then(|mut f| f.read_exact(&mut head))
            .is_ok()
            && &head == MAGIC;

        let region = Region::from_file(path).map_err(|e| anyhow!("{e}"))?;
        let region = if magic {
            region.subregion(8, len - 8).map_err(|e| anyhow!("{e}"))?
        } else {
            region
        };
        // The crate's default 4 GiB traversal cap is a safety limit for
        // untrusted input. These bundles are the user's own game files and the
        // largest expands to ~4.5 GiB, so the cap has to come up or that zone
        // group -- Underdocks among them -- silently fails to load.
        let mut limits = AssetLoadLimits::default();
        limits.maximum_expanded_bytes = 32 * 1024 * 1024 * 1024;
        limits.maximum_single_entry_bytes = 4 * 1024 * 1024 * 1024;
        let col = AssetCollection::load_with_limits(
            path.to_string_lossy().to_string(), region, limits)
            .map_err(|e| anyhow!("load {}: {e}", path.display()))?;

        let files = col.serialized_files();
        let mut index = Vec::with_capacity(files.len());
        let mut by_cab = HashMap::new();
        for (fi, sf) in files.iter().enumerate() {
            let mut m = HashMap::with_capacity(sf.file.objects.len());
            for (oi, o) in sf.file.objects.iter().enumerate() {
                m.insert(o.path_id, oi);
            }
            index.push(m);
            by_cab.insert(cab_of(&sf.path).to_string(), fi);
        }
        let externals = files
            .iter()
            .map(|sf| {
                sf.file
                    .externals
                    .iter()
                    .map(|e| by_cab.get(cab_of(&e.path)).copied())
                    .collect()
            })
            .collect();

        Ok(Self { col, index, by_cab, externals })
    }

    pub fn file_count(&self) -> usize {
        self.col.serialized_files().len()
    }

    pub fn file_index(&self, cab: &str) -> Option<usize> {
        self.by_cab.get(cab_of(cab)).copied()
    }

    /// Resolve a pointer read from `from_file` into (file index, object index).
    pub fn resolve(&self, from_file: usize, p: PPtr) -> Option<(usize, usize)> {
        if p.is_null() {
            return None;
        }
        let target = if p.file == 0 {
            from_file
        } else {
            (*self.externals.get(from_file)?.get(p.file as usize - 1)?)?
        };
        let oi = *self.index.get(target)?.get(&p.path_id)?;
        Some((target, oi))
    }

    pub fn class_id(&self, file: usize, obj: usize) -> Option<i32> {
        Some(self.col.serialized_files()[file].file.objects.get(obj)?.class_id)
    }

    pub fn read(&self, file: usize, obj: usize) -> Option<TypeValue> {
        self.col.serialized_files()[file]
            .file
            .read_type_tree_value(obj)
            .ok()
    }

    /// `scene path -> CAB`, straight from the bundle manifest.
    pub fn scene_index(&self) -> HashMap<String, String> {
        let mut out = HashMap::new();
        for sf in self.col.serialized_files() {
            for (i, o) in sf.file.objects.iter().enumerate() {
                if o.class_id != CLASS_ASSETBUNDLE {
                    continue;
                }
                let Ok(v) = sf.file.read_type_tree_value(i) else { continue };
                // m_SceneHashes is serialized as a map of scene path -> hash.
                if let Some(TypeValue::Map(entries)) = field(&v, "m_SceneHashes") {
                    for e in entries {
                        if let (TypeValue::String(k), TypeValue::String(hash)) = (&e.key, &e.value) {
                            out.insert(k.clone(), hash.clone());
                        }
                    }
                }
            }
        }
        out
    }
}

// ----------------------------------------------------------- TypeValue access

pub fn field<'a>(v: &'a TypeValue, name: &str) -> Option<&'a TypeValue> {
    match v {
        TypeValue::Object(fs) => fs.iter().find(|f| f.name == name).map(|f| &f.value),
        _ => None,
    }
}

pub fn as_f64(v: Option<&TypeValue>) -> Option<f64> {
    match v? {
        TypeValue::Float32(f) => Some(*f as f64),
        TypeValue::Float(f) => Some(*f),
        TypeValue::Signed(i) => Some(*i as f64),
        TypeValue::Unsigned(u) => Some(*u as f64),
        _ => None,
    }
}

pub fn as_i64(v: Option<&TypeValue>) -> Option<i64> {
    match v? {
        TypeValue::Signed(i) => Some(*i),
        TypeValue::Unsigned(u) => Some(*u as i64),
        _ => None,
    }
}

pub fn as_str(v: Option<&TypeValue>) -> Option<&str> {
    match v? {
        TypeValue::String(s) => Some(s.as_str()),
        _ => None,
    }
}

/// Unity writes pointers as `{m_FileID, m_PathID}`.
pub fn as_pptr(v: Option<&TypeValue>) -> Option<PPtr> {
    let v = v?;
    Some(PPtr {
        file: as_i64(field(v, "m_FileID")).unwrap_or(0),
        path_id: as_i64(field(v, "m_PathID")).unwrap_or(0),
    })
}

/// `m_Component` entries are either a bare pointer or `{component: pointer}`
/// depending on the Unity version that built the scene.
pub fn component_pptr(entry: &TypeValue) -> Option<PPtr> {
    if let Some(inner) = field(entry, "component") {
        return as_pptr(Some(inner));
    }
    as_pptr(Some(entry))
}

pub fn vec3(v: Option<&TypeValue>, dx: f64, dy: f64, dz: f64) -> [f64; 3] {
    let Some(v) = v else { return [dx, dy, dz] };
    [
        as_f64(field(v, "x")).unwrap_or(dx),
        as_f64(field(v, "y")).unwrap_or(dy),
        as_f64(field(v, "z")).unwrap_or(dz),
    ]
}

pub fn quat(v: Option<&TypeValue>) -> [f64; 4] {
    let Some(v) = v else { return [0.0, 0.0, 0.0, 1.0] };
    [
        as_f64(field(v, "x")).unwrap_or(0.0),
        as_f64(field(v, "y")).unwrap_or(0.0),
        as_f64(field(v, "z")).unwrap_or(0.0),
        as_f64(field(v, "w")).unwrap_or(1.0),
    ]
}

// Zone generation wants to run several zones from one bundle at once, which
// requires sharing the open collection across threads. Assert it here so the
// requirement fails at compile time rather than deep inside the job.
fn _assert_env_shareable() {
    fn require<T: Send + Sync>() {}
    require::<Env>();
}
