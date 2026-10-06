//! Running a full generation in the background, with progress a UI can show.

use super::{tiles, zones};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{channel, Receiver, TryRecvError};
use std::sync::Arc;
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
        Self::start_with(install, out_dir, only, force, false)
    }

    /// As [`Job::start`]; `floors_only` rebuilds just the chosen floors of
    /// each zone and leaves its top-level map as it is.
    pub fn start_with(
        install: PathBuf,
        out_dir: PathBuf,
        only: Option<std::collections::HashSet<String>>,
        force: bool,
        floors_only: bool,
    ) -> Job {
        let floors_off = Settings::load().floors_off;
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
            // Skip zones already built before anything opens their bundle --
            // a resumed run should not load a 2 GB file to find nothing to do.
            let todo: Vec<_> = located.into_iter()
                .filter(|l| !floors_only || !super::floors::plan(&l.zone).is_empty())
                .filter(|l| force || !out_dir.join(format!("{}.mbtiles", zones::slug(&l.zone))).exists())
                .collect();
            let completed = std::sync::atomic::AtomicUsize::new(total - todo.len());
            let results: Vec<(usize, u64, Option<String>)> = run_zones(
                todo, zone_concurrency(), &install, &out_dir, &c,
                |env, shared, l| {
                    let seen = completed.load(Ordering::Relaxed);
                    let _ = tx.send(Stage::Zone {
                        done: seen, total, name: l.zone.clone(), level: 0, levels: 4,
                    });
                    let out = out_dir.join(format!("{}.mbtiles", zones::slug(&l.zone)));
                    let tiles::LoadedZone { tris, sea, props, bbox, .. } =
                        tiles::load_zone(env, shared, &l.group);
                    if tris.is_empty() {
                        completed.fetch_add(1, Ordering::Relaxed);
                        return (0, 0, Some(format!("{}: no walkable geometry", l.zone)));
                    }
                    let name = l.zone.clone();
                    let txl = tx.clone();
                    let tset = tiles::Settings {
                        skip_floors: floors_off.get(&l.zone).cloned().unwrap_or_default(),
                        floors_only,
                        ..tiles::Settings::default()
                    };
                    let res = tiles::build_zone(
                        &out,
                        &tiles::ZoneInput {
                            name: &name, tris: &tris, sea, props: &props, bbox: Some(bbox), tags: None,
                        },
                        &tset,
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
                },
                |l, msg| {
                    completed.fetch_add(1, Ordering::Relaxed);
                    (0, 0, Some(format!("{}: {msg}", l.zone)))
                },
            );
            if c.load(Ordering::Relaxed) { return }
            let (mut tiles_n, mut bytes_n) = (0usize, 0u64);
            let mut failed: Vec<String> = Vec::new();
            for (n, b, err) in results {
                tiles_n += n;
                bytes_n += b;
                if let Some(e) = err { failed.push(e) }
            }
            let done = completed.load(Ordering::Relaxed);
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
/// Bounded by memory, not cores: a zone in flight renders all its zoom
/// levels -- and its floors -- side by side, and the largest peak around
/// 6 GB that way. The work inside each zone already spreads over every core,
/// so more lanes than memory allows would only trade speed for swapping.
pub fn zone_concurrency() -> usize {
    const PER_LANE_GB: f64 = 6.0;
    let cores = std::thread::available_parallelism().map_or(4, |n| n.get());
    let by_cores = (cores / 2).clamp(1, 6);
    match available_memory_gb() {
        Some(gb) => ((gb / PER_LANE_GB) as usize).clamp(1, by_cores),
        // Unknown: assume a modest machine.
        None => by_cores.min(2),
    }
}

/// Memory free for use, in GB, if the platform says.
fn available_memory_gb() -> Option<f64> {
    #[cfg(target_os = "linux")]
    {
        let t = std::fs::read_to_string("/proc/meminfo").ok()?;
        let kb: f64 = t.lines().find(|l| l.starts_with("MemAvailable:"))?
            .split_whitespace().nth(1)?.parse().ok()?;
        Some(kb / 1024.0 / 1024.0)
    }
    #[cfg(windows)]
    {
        #[repr(C)]
        struct MemoryStatusEx {
            length: u32, memory_load: u32, total_phys: u64, avail_phys: u64,
            total_page_file: u64, avail_page_file: u64, total_virtual: u64,
            avail_virtual: u64, avail_extended_virtual: u64,
        }
        #[link(name = "kernel32")]
        extern "system" { fn GlobalMemoryStatusEx(buf: *mut MemoryStatusEx) -> i32; }
        let mut m: MemoryStatusEx = unsafe { std::mem::zeroed() };
        m.length = std::mem::size_of::<MemoryStatusEx>() as u32;
        if unsafe { GlobalMemoryStatusEx(&mut m) } == 0 { return None }
        Some(m.avail_phys as f64 / 1024.0 / 1024.0 / 1024.0)
    }
    #[cfg(not(any(target_os = "linux", windows)))]
    { None }
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
    /// The zone that was open last, so a restart resumes where you were
    /// rather than at whatever sorts first.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub last_zone: String,
    /// Window size in logical points.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub window: Option<(f32, f32)>,
    /// Map rotation per zone, in degrees clockwise. A zone left north-up has
    /// no entry.
    #[serde(default, skip_serializing_if = "std::collections::BTreeMap::is_empty")]
    pub rotations: std::collections::BTreeMap<String, f32>,
    /// Per zone, the floors (numbered from 1, lowest first) NOT to build.
    /// Floors take as long as their zone each, so skipping unwanted ones
    /// is most of what a multi-storey zone costs.
    #[serde(default, skip_serializing_if = "std::collections::BTreeMap::is_empty")]
    pub floors_off: std::collections::BTreeMap<String, Vec<usize>>,
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

/// Paint one zone into the frame of the map it already has, on a worker
/// thread. The result is the path written, or what went wrong. A debug aid:
/// the painted style is an experiment, built on demand for the zone open.
pub struct PaintJob {
    pub zone: String,
    pub started: Instant,
    rx: Receiver<Result<PathBuf, String>>,
    progress: Arc<std::sync::Mutex<String>>,
}

pub struct PaintFrame {
    pub extent: [f64; 4],
    pub base_ppu: f64,
    pub zooms: usize,
}

impl PaintJob {
    pub fn start(install: PathBuf, zone: String, out: PathBuf, frame: PaintFrame) -> PaintJob {
        let (tx, rx) = channel();
        let progress = Arc::new(std::sync::Mutex::new("finding the zone".to_string()));
        let prog = progress.clone();
        let z = zone.clone();
        std::thread::spawn(move || {
            let say = |s: &str| { if let Ok(mut p) = prog.lock() { *p = s.to_string() } };
            let res = (|| -> Result<PathBuf, String> {
                let located = zones::survey_cached(&install, |_, _, _| {});
                let l = located.into_iter().find(|l| l.zone == z)
                    .ok_or_else(|| format!("{z} is not in this install"))?;
                say("reading the game files");
                let env = super::bundle::Env::open(&l.bundle).map_err(|e| e.to_string())?;
                let shared = super::bundle::Shared::open(&install);
                let lz = tiles::load_zone_with(&env, &shared, &l.group, true);
                if lz.tris.is_empty() { return Err("no walkable geometry".into()) }
                let [ax, bx, az, bz] = frame.extent;
                let mut level = |done: usize, total: usize| say(&format!("painting level {done}/{total}"));
                say(&format!("painting level 0/{}", frame.zooms));
                tiles::build_painted(&out,
                    &tiles::ZoneInput { name: &z, tris: &lz.tris, sea: lz.sea,
                                        props: &lz.all_props, bbox: None,
                                        tags: Some((&lz.mats, &lz.mat_table)) },
                    ((ax, bx, az, bz), frame.base_ppu), frame.zooms, &mut level)
                    .map_err(|e| e.to_string())?;
                Ok(out)
            })();
            let _ = tx.send(res);
        });
        PaintJob { zone, started: Instant::now(), rx, progress }
    }

    /// The outcome once finished; None while still running.
    pub fn poll(&self) -> Option<Result<PathBuf, String>> {
        match self.rx.try_recv() {
            Ok(r) => Some(r),
            Err(std::sync::mpsc::TryRecvError::Empty) => None,
            Err(_) => Some(Err("the painting thread stopped".into())),
        }
    }

    pub fn progress(&self) -> String {
        self.progress.lock().map(|p| p.clone()).unwrap_or_default()
    }
}

/// Run `work` on every zone in `zones`, `lanes` at a time, from one queue.
///
/// Two things this does that a per-bundle loop did not:
///
/// - **No bundle barrier.** Zones of the next bundle start as soon as a lane
///   frees, instead of waiting for the slowest zone of the current one; a
///   bundle is opened when its first zone is reached and dropped once its
///   last finishes, so only about two are ever open.
/// - **All cores.** Lanes are plain threads; the parallel work inside each
///   zone runs on the global thread pool. Running the zones themselves on a
///   pool of `lanes` threads confined that inner work to those threads too,
///   which kept a full build at 4 of 16 cores busy.
///
/// Within a bundle the costliest zones go first, so the run does not end on
/// one long zone with every other lane idle. Cost is guessed from the size
/// of the map a previous build left, when there is one.
pub fn run_zones<R: Send>(
    zones_in: Vec<zones::Located>,
    lanes: usize,
    install: &Path,
    out_dir: &Path,
    cancel: &AtomicBool,
    work: impl Fn(&super::bundle::Env, &super::bundle::Shared, &zones::Located) -> R + Sync,
    on_open_failed: impl Fn(&zones::Located, &str) -> R + Sync,
) -> Vec<R> {
    use std::collections::HashMap;
    use std::sync::Mutex;
    if zones_in.is_empty() { return Vec::new() }
    let cost = |l: &zones::Located| -> u64 {
        std::fs::metadata(out_dir.join(format!("{}.mbtiles", zones::slug(&l.zone))))
            .map(|m| m.len()).unwrap_or(0)
    };
    let mut order: Vec<(PathBuf, u64, zones::Located)> =
        zones_in.into_iter().map(|l| (l.bundle.clone(), cost(&l), l)).collect();
    order.sort_by(|a, b| a.0.cmp(&b.0).then(b.1.cmp(&a.1)));
    let mut left: HashMap<PathBuf, usize> = HashMap::new();
    for (b, _, _) in &order { *left.entry(b.clone()).or_default() += 1 }

    struct Slot { env: Option<Arc<Result<super::bundle::Env, String>>>, left: usize }
    let slots: Mutex<HashMap<PathBuf, Arc<Mutex<Slot>>>> = Mutex::new(
        left.into_iter().map(|(b, n)| (b, Arc::new(Mutex::new(Slot { env: None, left: n })))).collect());
    let shared = super::bundle::Shared::open(install);
    let queue: Mutex<std::collections::VecDeque<(usize, zones::Located)>> =
        Mutex::new(order.into_iter().map(|(_, _, l)| l).enumerate().collect());
    let results: Mutex<Vec<(usize, R)>> = Mutex::new(Vec::new());

    std::thread::scope(|s| {
        for _ in 0..lanes.max(1) {
            s.spawn(|| loop {
                if cancel.load(Ordering::Relaxed) { return }
                let Some((i, l)) = queue.lock().unwrap().pop_front() else { return };
                let slot = slots.lock().unwrap()[&l.bundle].clone();
                // Open the bundle once; a lane arriving meanwhile waits here.
                let env = {
                    let mut sl = slot.lock().unwrap();
                    sl.env.get_or_insert_with(|| Arc::new(
                        super::bundle::Env::open(&l.bundle).map_err(|e| e.to_string()))).clone()
                };
                let r = match env.as_ref() {
                    Ok(e) => work(e, &shared, &l),
                    Err(msg) => on_open_failed(&l, msg),
                };
                drop(env);
                {
                    let mut sl = slot.lock().unwrap();
                    sl.left -= 1;
                    if sl.left == 0 { sl.env = None } // last zone of this bundle
                }
                results.lock().unwrap().push((i, r));
            });
        }
    });
    let mut r = results.into_inner().unwrap();
    r.sort_by_key(|(i, _)| *i);
    r.into_iter().map(|(_, r)| r).collect()
}
