//! Sharing markers: a one-line code to paste in chat, and packs of many.
//!
//! Two formats, because they answer different questions.
//!
//! A **share code** is one marker on one line, readable enough that someone can
//! see where it points before pasting it:
//!
//! ```text
//! mnm1|underdocks|camp|-2500.4|2000.1|Griffon camp|pull from the north
//! ```
//!
//! A **pack** is a JSON file of many markers across zones, for publishing a set
//! someone else can import. It is deliberately the same shape as the files the
//! viewer already writes, so a pack can be inspected and hand-edited.

use crate::markers::{kind, Marker, Reqs};
use anyhow::{anyhow, bail, Result};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::path::Path;

pub const CODE_PREFIX: &str = "mnm1";
const SEP: char = '|';

/// Escape the field separator and the escape character itself.
fn esc(s: &str) -> String {
    s.replace('\\', "\\\\").replace('|', "\\|").replace(['\n', '\r'], " ")
}

fn split_escaped(s: &str) -> Vec<String> {
    let mut out = vec![String::new()];
    let mut esc = false;
    for c in s.chars() {
        let last = out.last_mut().unwrap();
        if esc {
            last.push(c);
            esc = false;
        } else if c == '\\' {
            esc = true;
        } else if c == SEP {
            out.push(String::new());
        } else {
            last.push(c);
        }
    }
    out
}

/// One marker as a line someone can paste into chat.
pub fn encode(zone_slug: &str, m: &Marker) -> String {
    let mut s = format!(
        "{CODE_PREFIX}{SEP}{}{SEP}{}{SEP}{:.1}{SEP}{:.1}{SEP}{}",
        esc(zone_slug),
        esc(&m.kind),
        m.x,
        m.z,
        esc(&m.label)
    );
    // Trailing fields are optional; omit them when there is nothing to say.
    let note = esc(&m.note);
    let reqs = if m.reqs.is_empty() { String::new() } else { esc(&m.reqs.summary()) };
    if !note.is_empty() || !reqs.is_empty() {
        s.push(SEP);
        s.push_str(&note);
    }
    if !reqs.is_empty() {
        s.push(SEP);
        s.push_str(&reqs);
    }
    s
}

pub struct Decoded {
    pub zone_slug: String,
    pub marker: Marker,
}

/// Parse a share code. Tolerant of surrounding chat punctuation and whitespace,
/// because these arrive pasted out of a chat window.
pub fn decode(text: &str) -> Result<Decoded> {
    let t = text.trim().trim_matches(|c| "<>[]()\"'`".contains(c)).trim();
    let start = t
        .find(CODE_PREFIX)
        .ok_or_else(|| anyhow!("no {CODE_PREFIX} code found in that text"))?;
    let t = &t[start..];
    let f = split_escaped(t);
    if f.len() < 6 {
        bail!("that code is incomplete ({} fields, need at least 6)", f.len());
    }
    let zone_slug = f[1].trim().to_lowercase();
    if zone_slug.is_empty() {
        bail!("that code names no zone");
    }
    let k = f[2].trim().to_lowercase();
    // An unknown kind is not fatal: fall back rather than reject a marker from
    // a newer version that added one.
    let kind_key = if kind(&k).key == "note" && k != "note" { "note" } else { kind(&k).key };
    let x: f64 = f[3].trim().parse().map_err(|_| anyhow!("bad X coordinate {:?}", f[3]))?;
    let z: f64 = f[4].trim().parse().map_err(|_| anyhow!("bad Z coordinate {:?}", f[4]))?;
    let label = f[5].trim().to_string();
    let note = f.get(6).map(|s| s.trim().to_string()).unwrap_or_default();
    // Requirements arrive as the rendered summary; keep it rather than lose it.
    let other = f.get(7).map(|s| s.trim().to_string()).unwrap_or_default();
    Ok(Decoded {
        zone_slug,
        marker: Marker {
            id: String::new(), // assigned on insert
            x,
            z,
            label,
            kind: kind_key.to_string(),
            note,
            link: String::new(),
            reqs: Reqs { other, ..Default::default() },
            src: String::new(),
            added: String::new(),
        },
    })
}

// ------------------------------------------------------------------- packs

#[derive(Serialize, Deserialize)]
pub struct Pack {
    /// Format version, so a future change can be detected rather than guessed.
    pub format: u32,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub name: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub author: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub note: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub created: String,
    /// zone slug -> markers
    pub zones: BTreeMap<String, Vec<Marker>>,
}

impl Pack {
    pub fn count(&self) -> usize {
        self.zones.values().map(|v| v.len()).sum()
    }

    pub fn write(&self, path: &Path) -> Result<()> {
        std::fs::write(path, serde_json::to_string_pretty(self)?)?;
        Ok(())
    }

    pub fn read(path: &Path) -> Result<Pack> {
        Self::parse(&std::fs::read_to_string(path)?)
    }

    pub fn parse(text: &str) -> Result<Pack> {
        let p: Pack = serde_json::from_str(text)?;
        if p.format != 1 {
            bail!("pack format {} is newer than this version understands", p.format);
        }
        Ok(p)
    }
}

/// How close two markers must be, in world units, to count as the same thing.
///
/// Two people marking the same camp will not agree to the decimetre, and
/// importing a community pack should not litter the map with near-duplicates.
const SAME_PLACE: f64 = 12.0;

pub struct MergeReport {
    pub added: usize,
    pub duplicates: usize,
}

/// Add `incoming` to `existing`, skipping ones already there.
///
/// A marker counts as already present when it is within `SAME_PLACE` units and
/// carries the same kind. Labels are not compared: the same camp described two
/// ways is still one camp.
pub fn merge_from(
    existing: &mut Vec<Marker>,
    incoming: &[Marker],
    stamp: &str,
    source: &str,
) -> MergeReport {
    let mut r = MergeReport { added: 0, duplicates: 0 };
    for m in incoming {
        let dup = existing.iter().any(|e| {
            e.kind == m.kind
                && (e.x - m.x).hypot(e.z - m.z) <= SAME_PLACE
        });
        if dup {
            r.duplicates += 1;
            continue;
        }
        let mut n = m.clone();
        if n.id.is_empty() || existing.iter().any(|e| e.id == n.id) {
            n.id = crate::markers::new_id();
        }
        if n.added.is_empty() {
            n.added = stamp.to_string();
        }
        if !source.is_empty() && n.src.is_empty() {
            n.src = source.to_string();
        }
        // A link points at an id in the source set and will not resolve here.
        n.link.clear();
        existing.push(n);
        r.added += 1;
    }
    r
}
