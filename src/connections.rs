//! Wiki zone-connection graph.
//!
//! Topology only. The community wiki map says which zones touch; it does not
//! say where the zone line sits inside a zone, so this is a checklist of exits
//! to look for, not a source of marker positions.

use serde::Deserialize;
use std::collections::BTreeMap;
use std::path::Path;

#[derive(Default, Deserialize)]
struct Raw {
    #[serde(default)]
    connections: Vec<(String, String)>,
    #[serde(default)]
    boat_routes: Vec<(String, String)>,
    #[serde(default)]
    portals: BTreeMap<String, Vec<String>>,
    #[serde(default)]
    no_known_connections: Vec<String>,
    #[serde(default)]
    display_names: BTreeMap<String, String>,
}

#[derive(Default)]
pub struct Graph {
    pub adj: BTreeMap<String, Vec<String>>,
    pub boats: BTreeMap<String, Vec<String>>,
    pub portals: BTreeMap<String, Vec<String>>,
    pub isolated: Vec<String>,
    pub names: BTreeMap<String, String>,
}

/// "ShadedDunes" -> "Shaded Dunes". Used for every zone the wiki does not name
/// explicitly, so the UI never shows a raw scene identifier.
pub fn split_camel(s: &str) -> String {
    let mut out = String::new();
    let cs: Vec<char> = s.chars().collect();
    for (i, c) in cs.iter().enumerate() {
        if i > 0 && c.is_uppercase() {
            let prev = cs[i - 1];
            let next_lower = cs.get(i + 1).map_or(false, |n| n.is_lowercase());
            if prev.is_lowercase() || prev.is_ascii_digit() || (prev.is_uppercase() && next_lower) {
                out.push(' ');
            }
        }
        out.push(*c);
    }
    out
}

impl Graph {
    pub fn load(base: &Path) -> Self {
        let mut p = base.join("connections.json");
        if !p.exists() {
            if let Some(up) = base.parent() {
                p = up.join("connections.json");
            }
        }
        let Ok(text) = std::fs::read_to_string(&p) else { return Self::default() };
        let Ok(raw) = serde_json::from_str::<Raw>(&text) else { return Self::default() };
        let mut g = Self {
            portals: raw.portals,
            isolated: raw.no_known_connections,
            names: raw.display_names,
            ..Default::default()
        };
        for (a, b) in raw.connections {
            g.adj.entry(a.clone()).or_default().push(b.clone());
            g.adj.entry(b).or_default().push(a);
        }
        for (a, b) in raw.boat_routes {
            g.boats.entry(a.clone()).or_default().push(b.clone());
            g.boats.entry(b).or_default().push(a);
        }
        for v in g.adj.values_mut() { v.sort(); v.dedup(); }
        for v in g.boats.values_mut() { v.sort(); v.dedup(); }
        g
    }

    /// Display name for a zone key, falling back to a CamelCase split.
    pub fn pretty(&self, key: &str) -> String {
        self.names.get(key).cloned().unwrap_or_else(|| split_camel(key))
    }
}
