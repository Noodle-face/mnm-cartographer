//! Follow the game's zone changes by tailing Player.log.
//!
//! The only gameplay line the client writes to disk:
//!     [Client][ZONE] Start zoning process to underdocks
//! Nothing is read from the game but this file. The client is not modified,
//! its memory is not read, and its network traffic is not touched.

use std::path::{Path, PathBuf};

const MARK: &str = "[Client][ZONE] Start zoning process to ";

pub struct ZoneWatcher {
    pub path: Option<PathBuf>,
    pos: u64,
    pub zone: Option<String>,
}

fn candidates() -> Vec<PathBuf> {
    let mut v = Vec::new();
    let rel = "AppData/LocalLow/Niche Worlds Cult/Monsters and Memories/Player.log";
    if let Some(home) = dirs::home_dir() {
        // Proton / Wine prefixes, whichever user directory the prefix uses
        for pfx in [
            home.join("Games/umu/mnm/drive_c/users"),
            home.join(".local/share/mnm/mnm/pfx/drive_c/users"),
        ] {
            if let Ok(rd) = std::fs::read_dir(&pfx) {
                for e in rd.flatten() {
                    v.push(e.path().join(rel));
                }
            }
        }
    }
    if let Some(d) = dirs::data_local_dir() {
        // Windows: %USERPROFILE%\AppData\LocalLow\...
        if let Some(p) = d.parent() {
            v.push(p.join("LocalLow/Niche Worlds Cult/Monsters and Memories/Player.log"));
        }
    }
    v
}

fn scan(path: &Path) -> Option<String> {
    let text = std::fs::read_to_string(path).ok()?;
    text.lines()
        .filter_map(|l| l.find(MARK).map(|i| l[i + MARK.len()..].trim().to_string()))
        .last()
}

impl ZoneWatcher {
    pub fn new(explicit: Option<PathBuf>) -> Self {
        let path = explicit
            .filter(|p| p.exists())
            .or_else(|| candidates().into_iter().find(|p| p.exists()));
        let mut zone = None;
        let mut pos = 0;
        if let Some(p) = &path {
            zone = scan(p);
            // A fresh launch rolls the old log to Player-prev.log, so the live
            // file often has no zone line yet. Seed from the previous session.
            if zone.is_none() {
                let prev = p.with_file_name("Player-prev.log");
                if prev.exists() {
                    zone = scan(&prev);
                }
            }
            pos = std::fs::metadata(p).map(|m| m.len()).unwrap_or(0);
        }
        Self { path, pos, zone }
    }

    /// Returns a zone id if it changed since the last call.
    pub fn poll(&mut self) -> Option<String> {
        let path = self.path.clone()?;
        let len = std::fs::metadata(&path).ok()?.len();
        if len < self.pos {
            self.pos = 0; // log rotated on relaunch
        }
        if len == self.pos {
            return None;
        }
        let text = std::fs::read_to_string(&path).ok()?;
        let tail = text.get(self.pos as usize..).unwrap_or(&text).to_string();
        self.pos = len;
        let found = tail
            .lines()
            .filter_map(|l| l.find(MARK).map(|i| l[i + MARK.len()..].trim().to_string()))
            .last()?;
        if Some(&found) != self.zone.as_ref() {
            self.zone = Some(found.clone());
            return Some(found);
        }
        None
    }
}

fn norm(s: &str) -> String {
    s.chars().filter(|c| c.is_ascii_alphanumeric()).collect::<String>().to_lowercase()
}

/// Map a log zone id ("underdocks", "nightharbore") to a pyramid name.
/// Ids carry sub-zone suffixes, so fall back to the longest prefix match.
pub fn match_zone(zone_id: &str, names: &[String]) -> Option<usize> {
    let z = norm(zone_id);
    if z.is_empty() {
        return None;
    }
    let mut best: Option<(usize, usize)> = None;
    for (i, n) in names.iter().enumerate() {
        let k = norm(n);
        if k.is_empty() {
            continue;
        }
        if z == k {
            return Some(i);
        }
        if z.starts_with(&k) || k.starts_with(&z) {
            if best.map_or(true, |(len, _)| k.len() > len) {
                best = Some((k.len(), i));
            }
        }
    }
    best.map(|(_, i)| i)
}
