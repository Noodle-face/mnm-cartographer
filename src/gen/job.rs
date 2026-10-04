//! Running a full generation in the background, with progress a UI can show.

use super::{tiles, zones};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{channel, Receiver, TryRecvError};
use std::sync::Arc;
use rayon::prelude::*;
use std::time::Instant;

#[derive(Clone, Debug)]
pub enum Stage {
    /// Reading each bundle to find out which zones it holds.
    Survey { done: usize, total: usize, what: String },
    /// Building one zone's pyramid.
    Zone { done: usize, total: usize, name: String, level: usize, levels: usize },
    Finished { zones: usize, tiles: usize, bytes: u64, failed: Vec<String> },
    Failed(String),
}

pub struct Job {
    rx: Receiver<Stage>,
    cancel: Arc<AtomicBool>,
    pub started: Instant,
    pub last: Option<Stage>,
    pub done: bool,
    /// Seconds per completed zone, for the estimate.
    zone_times: Vec<f32>,
    zone_started: Option<Instant>,
    last_zone: usize,
}

/// One zone as the maps panel lists it.
pub struct ZoneEntry {
    pub name: String,
    pub built: bool,
}

/// Every zone the install ships, and whether it has been built already.
/// Uses the cached survey, so this is instant once the bundles have been seen.
pub fn list_zones(install: &Path, out_dir: &Path) -> Vec<ZoneEntry> {
    zones::survey_cached(install, |_, _, _| {})
        .into_iter()
        .map(|l| {
            let built = out_dir
                .join(format!("{}.mbtiles", zones::slug(&l.zone)))
                .exists();
            ZoneEntry { name: l.zone, built }
        })
        .collect()
}

impl Job {
    /// Generate zones into `out_dir`.
    ///
    /// `only` limits the run to named zones; `force` rebuilds ones that already
    /// exist. With neither, it builds whatever is missing.
    pub fn start(
        install: PathBuf,
        out_dir: PathBuf,
        only: Option<std::collections::HashSet<String>>,
        force: bool,
    ) -> Job {
        let (tx, rx) = channel();
        let cancel = Arc::new(AtomicBool::new(false));
        let c = cancel.clone();
        std::thread::spawn(move || {
            let send = |s: Stage| tx.send(s).is_ok();
            let located: Vec<_> = {
                let tx2 = tx.clone();
                zones::survey_cached(&install, |done, total, what| {
                    let _ = tx2.send(Stage::Survey { done, total, what: what.to_string() });
                })
            }
            .into_iter()
            .filter(|l| only.as_ref().map_or(true, |s| s.contains(&l.zone)))
            .collect();
            if located.is_empty() {
                send(Stage::Failed(format!(
                    "No zone bundles found in {}", install.display())));
                return;
            }
            let total = located.len();
            let (mut tiles_n, mut bytes_n) = (0usize, 0u64);
            let mut failed: Vec<String> = Vec::new();
            // Group by bundle so each 2 GB file is opened once, not per zone.
            let mut by_bundle: std::collections::BTreeMap<PathBuf, Vec<zones::Located>> =
                Default::default();
            for l in located {
                by_bundle.entry(l.bundle.clone()).or_default().push(l);
            }
            let pool = rayon::ThreadPoolBuilder::new()
                .num_threads(zone_concurrency())
                .build();
            let Ok(pool) = pool else {
                send(Stage::Failed("could not start worker threads".into()));
                return;
            };
            let mut done = 0usize;
            for (bundle, group) in by_bundle {
                if c.load(Ordering::Relaxed) {
                    return;
                }
                // Skip zones that are already built before paying to open the
                // bundle -- a resumed run should not reload a 2 GB file to find
                // there is nothing left to do in it.
                let total_in_group = group.len();
                let group: Vec<_> = group
                    .into_iter()
                    .filter(|l| {
                        force
                            || !out_dir
                                .join(format!("{}.mbtiles", zones::slug(&l.zone)))
                                .exists()
                    })
                    .collect();
                done += total_in_group - group.len();
                if group.is_empty() {
                    continue;
                }
                let env = match super::bundle::Env::open(&bundle) {
                    Ok(e) => e,
                    Err(e) => {
                        for l in &group {
                            failed.push(format!("{}: {e}", l.zone));
                        }
                        done += group.len();
                        continue;
                    }
                };
                let base_done = done;
                let completed = std::sync::atomic::AtomicUsize::new(base_done);
                let results: Vec<(usize, u64, Option<String>)> = pool.install(|| {
                    group
                        .par_iter()
                        .map(|l| {
                            if c.load(Ordering::Relaxed) {
                                return (0, 0, None);
                            }
                            let seen = completed.load(Ordering::Relaxed);
                            let _ = tx.send(Stage::Zone {
                                done: seen, total, name: l.zone.clone(), level: 0, levels: 4,
                            });
                            let out = out_dir.join(format!("{}.mbtiles", zones::slug(&l.zone)));
                            let (tris, sea, props) = tiles::load_zone(&env, &l.group);
                            if tris.is_empty() {
                                completed.fetch_add(1, Ordering::Relaxed);
                                return (0, 0, Some(format!("{}: no walkable geometry", l.zone)));
                            }
                            let name = l.zone.clone();
                            let txl = tx.clone();
                            let res = tiles::build(
                                &out,
                                &tiles::ZoneInput {
                                    name: &name, tris: &tris, sea, props: &props,
                                },
                                &tiles::Settings::default(),
                                |level, levels| {
                                    let seen = completed.load(Ordering::Relaxed);
                                    let _ = txl.send(Stage::Zone {
                                        done: seen, total, name: name.clone(), level, levels,
                                    });
                                },
                            );
                            completed.fetch_add(1, Ordering::Relaxed);
                            match res {
                                Ok((n, b)) => (n, b, None),
                                Err(e) => (0, 0, Some(format!("{}: {e}", l.zone))),
                            }
                        })
                        .collect()
                });
                for (n, b, err) in results {
                    tiles_n += n;
                    bytes_n += b;
                    if let Some(e) = err {
                        failed.push(e);
                    }
                }
                done = completed.load(Ordering::Relaxed);
            }
            send(Stage::Finished { zones: done, tiles: tiles_n, bytes: bytes_n, failed });
        });
        Job {
            rx,
            cancel,
            started: Instant::now(),
            last: None,
            done: false,
            zone_times: Vec::new(),
            zone_started: None,
            last_zone: usize::MAX,
        }
    }

    pub fn cancel(&self) {
        self.cancel.store(true, Ordering::Relaxed);
    }

    /// Drain pending updates. Returns true if anything changed.
    pub fn poll(&mut self) -> bool {
        let mut changed = false;
        loop {
            match self.rx.try_recv() {
                Ok(s) => {
                    if let Stage::Zone { done, .. } = &s {
                        // Time each zone so the estimate is based on measured
                        // work rather than a guess.
                        if *done != self.last_zone {
                            if let Some(t) = self.zone_started.take() {
                                self.zone_times.push(t.elapsed().as_secs_f32());
                            }
                            self.zone_started = Some(Instant::now());
                            self.last_zone = *done;
                        }
                    }
                    if matches!(s, Stage::Finished { .. } | Stage::Failed(_)) {
                        self.done = true;
                    }
                    self.last = Some(s);
                    changed = true;
                }
                Err(TryRecvError::Empty) => break,
                Err(TryRecvError::Disconnected) => {
                    if !self.done {
                        self.done = true;
                        changed = true;
                    }
                    break;
                }
            }
        }
        changed
    }

    /// Overall completion in 0..1, and a human estimate of time remaining.
    pub fn progress(&self) -> (f32, Option<f32>) {
        match &self.last {
            Some(Stage::Survey { done, total, .. }) => {
                // The survey is a real part of the wait: it reads every bundle.
                let f = if *total == 0 { 0.0 } else { *done as f32 / *total as f32 };
                (0.08 * f, None)
            }
            Some(Stage::Zone { done, total, level, levels, .. }) => {
                let per = 1.0 / *total.max(&1) as f32;
                let within = if *levels == 0 { 0.0 } else { *level as f32 / *levels as f32 };
                let f = 0.08 + 0.92 * (*done as f32 * per + within * per);
                let eta = if self.zone_times.is_empty() {
                    None
                } else {
                    let mean: f32 =
                        self.zone_times.iter().sum::<f32>() / self.zone_times.len() as f32;
                    let left = total.saturating_sub(*done) as f32 - within;
                    Some((mean * left).max(0.0))
                };
                (f.min(1.0), eta)
            }
            Some(Stage::Finished { .. }) => (1.0, None),
            _ => (0.0, None),
        }
    }
}

/// How many zones to build at once.
///
/// Each zone in flight holds its rasters, masks and the finished sheet -- about
/// a gigabyte at the finest level for a large zone -- so this is bounded by
/// memory rather than cores. The remaining threads are not idle: the elevation
/// bands inside a zone run in parallel too.
pub fn zone_concurrency() -> usize {
    let cores = std::thread::available_parallelism().map_or(4, |n| n.get());
    (cores / 2).clamp(2, 6)
}

/// Where generated maps live. Chosen so the viewer's normal search finds them.
pub fn maps_dir() -> PathBuf {
    dirs::data_dir()
        .unwrap_or_else(|| PathBuf::from("."))
        .join("mnm-cartographer")
        .join("maps")
}

// ------------------------------------------------------------- settings

fn settings_path() -> PathBuf {
    dirs::data_dir()
        .unwrap_or_else(|| PathBuf::from("."))
        .join("mnm-cartographer")
        .join("settings.json")
}

#[derive(Default, serde::Serialize, serde::Deserialize)]
pub struct Settings {
    /// Explicit game install, when auto-detection did not find it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub install_dir: Option<PathBuf>,
}

impl Settings {
    pub fn load() -> Self {
        std::fs::read_to_string(settings_path())
            .ok()
            .and_then(|t| serde_json::from_str(&t).ok())
            .unwrap_or_default()
    }
    pub fn save(&self) {
        let p = settings_path();
        if let Some(d) = p.parent() {
            std::fs::create_dir_all(d).ok();
        }
        if let Ok(t) = serde_json::to_string_pretty(self) {
            std::fs::write(p, t).ok();
        }
    }
    /// The bundles directory to generate from: the saved choice if it still
    /// looks right, otherwise whatever auto-detection turns up.
    pub fn resolve(&self) -> Option<PathBuf> {
        if let Some(d) = &self.install_dir {
            if let Some(b) = resolve_install(d) {
                return Some(b);
            }
        }
        zones::bundles_dir()
    }
}

/// Accept either the bundles directory itself or the install root above it,
/// since a user picking a folder will reasonably choose either.
pub fn resolve_install(chosen: &Path) -> Option<PathBuf> {
    let rel = Path::new("mnm_Data/StreamingAssets/aa/StandaloneWindows64");
    for cand in [chosen.to_path_buf(), chosen.join(rel)] {
        if !zones::bundle_files(&cand).is_empty() {
            return Some(cand);
        }
    }
    None
}
