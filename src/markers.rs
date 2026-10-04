//! Per-zone annotations, stored in WORLD coordinates as plain JSON.
//!
//! World coordinates (not pixels) so a marker is independent of zoom level and
//! of any future re-render at a different resolution. Plain JSON so a group can
//! diff and share them.
//!
//! On identity: eight marker kinds are visible at once, and eight categorical
//! colours cannot all be told apart -- the best assignment scores a worst-pair
//! OKLab dE of 13.7 against a floor of 15 for normal vision, and 3.9 under
//! simulated deuteranopia. So SHAPE is the primary identity channel and colour
//! is secondary, with a label always available. Several of these hues also sit
//! under 3:1 contrast on the cream map, which is why every marker carries a
//! dark ink ring.

use anyhow::Result;
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

#[derive(Copy, Clone, PartialEq, Eq, Debug)]
pub enum Shape {
    Circle,
    Star,
    Diamond,
    Square,
    Hexagon,
    Pentagon,
    Triangle,
    Cross,
}

pub struct KindInfo {
    pub key: &'static str,
    /// Capitalised name for the UI; `key` stays lowercase in the JSON.
    pub label: &'static str,
    pub color: egui::Color32,
    pub shape: Shape,
    pub about: &'static str,
}

pub const KINDS: &[KindInfo] = &[
    KindInfo { key: "camp", label: "Camp",     color: egui::Color32::from_rgb(0x2a, 0x78, 0xd6), shape: Shape::Circle,   about: "A pull spot or sit-and-fight camp" },
    KindInfo { key: "named", label: "Named spawn",    color: egui::Color32::from_rgb(0xeb, 0x68, 0x34), shape: Shape::Star,     about: "Named or rare spawn" },
    KindInfo { key: "harvest", label: "Harvest",  color: egui::Color32::from_rgb(0x00, 0x83, 0x00), shape: Shape::Diamond,  about: "Resource node: ore, herb, wood" },
    KindInfo { key: "merchant", label: "Merchant", color: egui::Color32::from_rgb(0x4a, 0x3a, 0xa7), shape: Shape::Square,   about: "Vendor, banker or trainer" },
    KindInfo { key: "exit", label: "Exit",     color: egui::Color32::from_rgb(0x1b, 0xaf, 0x7a), shape: Shape::Hexagon,  about: "Zone connection, stairs or portal" },
    KindInfo { key: "quest", label: "Quest",    color: egui::Color32::from_rgb(0xed, 0xa1, 0x00), shape: Shape::Pentagon, about: "Quest giver or turn-in" },
    KindInfo { key: "danger", label: "Danger",   color: egui::Color32::from_rgb(0xa0, 0x1b, 0x22), shape: Shape::Triangle, about: "Avoid: KOS mob, roamer, drop or trap" },
    KindInfo { key: "note", label: "Note",     color: egui::Color32::from_rgb(0xd4, 0x55, 0x9a), shape: Shape::Cross,    about: "Anything else worth remembering" },
];

pub fn kind(key: &str) -> &'static KindInfo {
    KINDS.iter().find(|k| k.key == key).unwrap_or(&KINDS[7])
}

#[derive(Clone, Serialize, Deserialize)]
pub struct Marker {
    pub id: String,
    pub x: f64,
    pub z: f64,
    #[serde(default)]
    pub label: String,
    #[serde(default = "default_kind")]
    pub kind: String,
    #[serde(default)]
    pub note: String,
    /// Another marker this one is paired with: a bare id for the same zone, or
    /// "<zone-slug>:<id>" to cross zones. Used for teleporter pairs, where the
    /// game ships no destination data of its own.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub link: String,
    /// Who can actually use this. Kept as fields rather than prose in the note
    /// so a marker can say "Cleric, level 20, Ashira" in a form that is
    /// readable at a glance and could later be filtered on.
    #[serde(default, skip_serializing_if = "Reqs::is_empty")]
    pub reqs: Reqs,
    #[serde(default)]
    pub added: String,
}

#[derive(Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct Reqs {
    /// Classes that can take or use it, free text: "Cleric" or "Cleric, Druid".
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub class: String,
    /// Faction or standing needed.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub faction: String,
    /// Minimum level.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub level: Option<u32>,
    /// Anything else gating it: a prerequisite quest, an item, a key.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub other: String,
}

impl Reqs {
    pub fn is_empty(&self) -> bool {
        self.class.is_empty()
            && self.faction.is_empty()
            && self.level.is_none()
            && self.other.is_empty()
    }

    /// One line, for a tooltip or the marker list.
    pub fn summary(&self) -> String {
        let mut parts: Vec<String> = Vec::new();
        if let Some(l) = self.level {
            parts.push(format!("level {l}"));
        }
        if !self.class.is_empty() {
            parts.push(self.class.clone());
        }
        if !self.faction.is_empty() {
            parts.push(self.faction.clone());
        }
        if !self.other.is_empty() {
            parts.push(self.other.clone());
        }
        parts.join(" \u{00b7} ")
    }
}
fn default_kind() -> String { "note".into() }

#[derive(Default, Serialize, Deserialize)]
struct File {
    #[serde(default)]
    markers: Vec<Marker>,
}

pub struct MarkerSet {
    pub path: PathBuf,
    pub items: Vec<Marker>,
}

impl MarkerSet {
    pub fn load(path: &Path) -> Self {
        let items = std::fs::read_to_string(path)
            .ok()
            .and_then(|s| serde_json::from_str::<File>(&s).ok())
            .map(|f| f.markers)
            .unwrap_or_default();
        Self { path: path.to_path_buf(), items }
    }

    pub fn add(&mut self, x: f64, z: f64, label: &str, kind: &str, note: &str) -> String {
        let id = format!("{:08x}", fastrand_id());
        self.items.push(Marker {
            id: id.clone(),
            x: (x * 100.0).round() / 100.0,
            z: (z * 100.0).round() / 100.0,
            label: label.into(),
            kind: kind.into(),
            note: note.into(),
            link: String::new(),
            reqs: Reqs::default(),
            added: now_stamp(),
        });
        let _ = self.save();
        id
    }

    pub fn remove(&mut self, id: &str) {
        self.items.retain(|m| m.id != id);
        let _ = self.save();
    }

    pub fn get_mut(&mut self, id: &str) -> Option<&mut Marker> {
        self.items.iter_mut().find(|m| m.id == id)
    }

    pub fn save(&self) -> Result<()> {
        if let Some(d) = self.path.parent() {
            std::fs::create_dir_all(d)?;
        }
        let f = File { markers: self.items.clone() };
        std::fs::write(&self.path, serde_json::to_string_pretty(&f)?)?;
        Ok(())
    }
}

fn now_stamp() -> String {
    use std::time::{SystemTime, UNIX_EPOCH};
    let secs = SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0);
    // minimal civil-date conversion; avoids pulling in a date crate for one string
    let days = secs / 86_400;
    let (h, mi) = ((secs % 86_400) / 3600, (secs % 3600) / 60);
    let (mut y, mut d) = (1970i64, days as i64);
    loop {
        let leap = (y % 4 == 0 && y % 100 != 0) || y % 400 == 0;
        let len = if leap { 366 } else { 365 };
        if d < len { break }
        d -= len;
        y += 1;
    }
    let leap = (y % 4 == 0 && y % 100 != 0) || y % 400 == 0;
    let ml = [31, if leap {29} else {28}, 31, 30, 31, 30, 31, 31, 30, 31, 30, 31];
    let mut m = 0;
    while d >= ml[m] { d -= ml[m]; m += 1; }
    format!("{y:04}-{:02}-{:02} {h:02}:{mi:02}", m + 1, d + 1)
}

fn fastrand_id() -> u32 {
    use std::time::{SystemTime, UNIX_EPOCH};
    let n = SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.subsec_nanos()).unwrap_or(1);
    n.wrapping_mul(2654435761).wrapping_add(std::process::id())
}
