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
    /// One zone's map is finished and can be opened, while the rest build.
    Built(String),
    Finished { zones: usize, tiles: usize, bytes: u64, failed: Vec<String> },
    Failed(String),
}

pub struct Job {
    rx: Receiver<Stage>,
    cancel: Arc<AtomicBool>,
    pub started: Instant,
    pub last: Option<Stage>,
    pub done: bool,
    /// Zones finished since the app last took them, to show at once rather
    /// than after the whole run.
    pub built: Vec<String>,
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
        Self::start_inner(install, out_dir, only, force, false, None)
    }

    /// Build whatever is missing, starting with `first` -- the zone the player
    /// is standing in -- on its own, so a first run has something to show in
    /// about a minute instead of after every zone.
    pub fn start_missing_first(install: PathBuf, out_dir: PathBuf, first: Option<String>) -> Job {
        Self::start_inner(install, out_dir, None, false, false, first)
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
        Self::start_inner(install, out_dir, only, force, floors_only, None)
    }

    fn start_inner(
        install: PathBuf,
        out_dir: PathBuf,
        only: Option<std::collections::HashSet<String>>,
        force: bool,
        floors_only: bool,
        first: Option<String>,
    ) -> Job {
        let settings = Settings::load();
        super::throttle::set_limit(settings.build_cpu());
        let floors_off = settings.floors_off;
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
            let work = |env: &super::bundle::Env, shared: &super::bundle::Shared, l: &zones::Located| {
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
                            lights: None,
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
                        Ok((n, b)) => {
                            let _ = tx.send(Stage::Built(l.zone.clone()));
                            (n, b, None)
                        }
                        Err(e) => (0, 0, Some(format!("{}: {e}", l.zone))),
                    }
            };
            let open_failed = |l: &zones::Located, msg: &str| {
                completed.fetch_add(1, Ordering::Relaxed);
                (0, 0, Some(format!("{}: {msg}", l.zone)))
            };
            // The priority zone alone first, with every core to itself; then
            // the rest as usual.
            let want = first.as_deref().map(zones::slug);
            let (head, rest): (Vec<_>, Vec<_>) = todo.into_iter()
                .partition(|l| want.as_ref().is_some_and(|w| zones::slug(&l.zone) == *w));
            let mut results: Vec<(usize, u64, Option<String>)> =
                run_zones(head, 1, &install, &out_dir, &c, &work, &open_failed);
            results.extend(run_zones(
                rest, zone_concurrency(), &install, &out_dir, &c, &work, &open_failed));
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
            built: Vec::new(),
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
                Ok(Stage::Built(name)) => {
                    self.built.push(name);
                    changed = true;
                }
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

/// What the first zone of a run costs in memory: every shared asset bundle,
/// which every zone needs, plus its own scene bundle, decoded. Measured: a
/// zone with a 100 KB map peaked at 11.6 GB, Underdocks at 17.3 GB.
const FIRST_LANE_GB: f64 = 12.0;
/// What each further zone in flight adds: its rasters, and its bundle when
/// that is not one already open. Six at once peaked at 31 GB.
const EXTRA_LANE_GB: f64 = 4.0;
/// Do not start another zone with less than this free, unless nothing else
/// is building -- a run always makes progress, one zone at a time if it must.
const MIN_FREE_GB: f64 = 4.0;

/// How many zones to build at once.
///
/// Bounded by memory, not cores: the work inside each zone already spreads
/// over every core, so more lanes than memory allows would only trade speed
/// for swapping. `run_zones` also checks free memory before each zone starts,
/// since the game, a browser or anything else may be using more by then.
pub fn zone_concurrency() -> usize {
    let cores = std::thread::available_parallelism().map_or(4, |n| n.get());
    let by_cores = (cores / 2).clamp(1, 6);
    let lanes = match available_memory_gb() {
        Some(gb) => (1 + ((gb - FIRST_LANE_GB).max(0.0) / EXTRA_LANE_GB) as usize).clamp(1, by_cores),
        // Unknown: assume a modest machine.
        None => by_cores.min(2),
    };
    super::throttle::scale_lanes(lanes)
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
    /// no entry. Up to 0.0.6 these were measured from +Z, not from north; see
    /// `north_is_x`.
    #[serde(default, skip_serializing_if = "std::collections::BTreeMap::is_empty")]
    pub rotations: std::collections::BTreeMap<String, f32>,
    /// Per zone, the floors (numbered from 1, lowest first) NOT to build.
    /// Floors take as long as their zone each, so skipping unwanted ones
    /// is most of what a multi-storey zone costs.
    #[serde(default, skip_serializing_if = "std::collections::BTreeMap::is_empty")]
    pub floors_off: std::collections::BTreeMap<String, Vec<usize>>,
    /// Do not ask GitHub for a newer release on launch. Stored as "off" so a
    /// fresh settings file checks.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub updates_off: bool,
    /// A release the user chose to skip; it is not offered again.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub skip_update: String,
    /// Marker packs followed by URL; see subscribe.rs.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub subscriptions: Vec<crate::subscribe::Subscription>,
    /// Overlay opacity while using the map and while playing, and the key
    /// that swaps the two; unset means the defaults in overlay.rs.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub overlay_opacity: Option<f32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub overlay_passive: Option<f32>,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub overlay_hotkey: String,
    /// Share of the CPU map building may use, in percent; see throttle.rs.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub build_cpu: Option<u8>,
    /// Show painted maps by the zone's own lights rather than in daylight;
    /// see gen::light.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub lamplight: bool,
    /// `rotations` are measured from the corrected north (world +X); older
    /// ones were cleared on upgrade.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub north_is_x: bool,
}

impl Settings {
    /// The CPU share for building, defaulting when never chosen.
    pub fn build_cpu(&self) -> u8 {
        self.build_cpu.unwrap_or(super::throttle::DEFAULT)
    }

    pub fn load() -> Self {
        let mut s: Settings = std::fs::read_to_string(settings_path())
            .ok()
            .and_then(|t| serde_json::from_str(&t).ok())
            .unwrap_or_default();
        // Rotations saved before north was corrected were turns away from +Z.
        // Most are nudges rather than choices, and kept they would leave a
        // zone west-up for good; every map starts north-up again instead.
        if !s.north_is_x {
            s.north_is_x = true;
            if !s.rotations.is_empty() {
                s.rotations.clear();
                s.save();
            }
        }
        s
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
    /// The floor being painted (index into `floors::plan`), if not the zone.
    pub floor: Option<usize>,
    pub started: Instant,
    rx: Receiver<Result<PathBuf, String>>,
    progress: Arc<std::sync::Mutex<String>>,
}

pub struct PaintFrame {
    pub extent: [f64; 4],
    pub base_ppu: f64,
    pub zooms: usize,
    /// Paint one floor of a zone built on top of itself -- an index into
    /// `floors::plan`, 0 = lowest -- rather than the zone from above.
    pub floor: Option<usize>,
}

impl PaintJob {
    pub fn start(install: PathBuf, zone: String, out: PathBuf, frame: PaintFrame) -> PaintJob {
        let floor_i = frame.floor;
        super::throttle::set_limit(Settings::load().build_cpu());
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
                // The zone's bundle and the shared ones, opened side by side.
                let (env, shared) = std::thread::scope(|s| {
                    let sh = s.spawn(|| super::bundle::Shared::open(&install));
                    let env = super::bundle::Env::open(&l.bundle).map_err(|e| e.to_string());
                    (env, sh.join().expect("opening shared bundles panicked"))
                });
                let env = env?;
                let mut lz = tiles::load_zone_with(&env, &shared, &l.group, true);
                if let Some(i) = frame.floor {
                    // The floor's slab, cut where its own map is cut.
                    let plan = super::floors::plan(&z);
                    let f = plan.get(i).ok_or_else(|| format!("{z} has no floor {}", i + 1))?;
                    let lo = if i == 0 { f32::NEG_INFINITY } else { plan[i - 1].top };
                    let on = |y: f32| y >= lo && y < f.top;
                    let keep: Vec<bool> = lz.tris.iter()
                        .map(|t| on((t[0][1] + t[1][1] + t[2][1]) / 3.0)).collect();
                    lz.tris = super::bounds::apply(&lz.tris, &keep);
                    lz.mats = super::bounds::apply(&lz.mats, &keep);
                    lz.all_props = lz.prop_points.iter()
                        .map(|(k, v)| (k.clone(), v.iter().filter(|p| on(p[1] as f32)).map(|p| (p[0], p[2])).collect()))
                        .collect();
                    // A floor's own lamps: those hanging within its slab.
                    lz.lights.retain(|l| on(l.p[1] as f32));
                }
                if lz.tris.is_empty() { return Err("no walkable geometry".into()) }
                let [ax, bx, az, bz] = frame.extent;
                let mut level = |done: usize, total: usize| say(&format!("painting level {done}/{total}"));
                say(&format!("painting level 0/{}", frame.zooms));
                tiles::build_painted(&out,
                    &tiles::ZoneInput { name: &z, tris: &lz.tris, sea: lz.sea,
                                        props: &lz.all_props, bbox: None,
                                        tags: Some((&lz.mats, &lz.mat_table)),
                                        lights: Some(&lz.lights) },
                    ((ax, bx, az, bz), frame.base_ppu), frame.zooms, frame.floor.is_some(), &mut level)
                    .map_err(|e| e.to_string())?;
                Ok(out)
            })();
            let _ = tx.send(res);
        });
        PaintJob { zone, floor: floor_i, started: Instant::now(), rx, progress }
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
    // The shared bundles open in the background while the first zones open
    // their own; a zone that needs them before they are ready waits.
    let shared = std::sync::OnceLock::new();
    let queue: Mutex<std::collections::VecDeque<(usize, zones::Located)>> =
        Mutex::new(order.into_iter().map(|(_, _, l)| l).enumerate().collect());
    let results: Mutex<Vec<(usize, R)>> = Mutex::new(Vec::new());
    // Zones building now, and when the last one started: a zone's memory
    // climbs for a few seconds after it starts, so a free-memory reading
    // taken sooner than that over-promises.
    let active = std::sync::atomic::AtomicUsize::new(0);
    let last_start: Mutex<Option<Instant>> = Mutex::new(None);

    std::thread::scope(|s| {
        s.spawn(|| { shared.get_or_init(|| super::bundle::Shared::open(install)); });
        for _ in 0..lanes.max(1) {
            s.spawn(|| loop {
                if cancel.load(Ordering::Relaxed) { return }
                let next = {
                    // One lane decides at a time, so two cannot both read the
                    // same free memory and both start.
                    let mut ls = last_start.lock().unwrap();
                    // With room to spare, start at once. Only when memory runs
                    // short are starts spaced out, so a reading is not taken
                    // before the last zone's memory has climbed: spaced always,
                    // 44 zones at five seconds apart held a full build to over
                    // three and a half minutes whatever else got faster.
                    let free = available_memory_gb();
                    let recent = ls.is_some_and(|t| t.elapsed().as_secs() < 5);
                    let can = active.load(Ordering::Relaxed) == 0
                        || free.map_or(!recent, |gb| gb >= FIRST_LANE_GB
                            || (!recent && gb >= MIN_FREE_GB));
                    if !can {
                        None
                    } else {
                        let got = queue.lock().unwrap().pop_front();
                        if got.is_some() {
                            active.fetch_add(1, Ordering::Relaxed);
                            *ls = Some(Instant::now());
                        }
                        Some(got)
                    }
                };
                let (i, l) = match next {
                    Some(Some(x)) => x,
                    Some(None) => return,
                    None => { std::thread::sleep(std::time::Duration::from_millis(500)); continue }
                };
                let slot = slots.lock().unwrap()[&l.bundle].clone();
                // Open the bundle once; a lane arriving meanwhile waits here.
                let env = {
                    let mut sl = slot.lock().unwrap();
                    sl.env.get_or_insert_with(|| Arc::new(
                        super::bundle::Env::open(&l.bundle).map_err(|e| e.to_string()))).clone()
                };
                let r = match env.as_ref() {
                    Ok(e) => work(e, shared.get_or_init(|| super::bundle::Shared::open(install)), &l),
                    Err(msg) => on_open_failed(&l, msg),
                };
                drop(env);
                {
                    let mut sl = slot.lock().unwrap();
                    sl.left -= 1;
                    if sl.left == 0 { sl.env = None } // last zone of this bundle
                }
                active.fetch_sub(1, Ordering::Relaxed);
                results.lock().unwrap().push((i, r));
            });
        }
    });
    let mut r = results.into_inner().unwrap();
    r.sort_by_key(|(i, _)| *i);
    r.into_iter().map(|(_, r)| r).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Needs the game installed, and builds a real zone, so it is not part of
    /// the normal run: `cargo test -- --ignored first_zone_builds_first`.
    #[test]
    #[ignore]
    fn first_zone_builds_first() {
        let install = Settings::load().resolve().expect("game install not found");
        let out = std::env::temp_dir().join(format!("mnm-first-{}", std::process::id()));
        std::fs::create_dir_all(&out).unwrap();
        // A small zone from the middle of the list, so it is not first by accident.
        let mut job = Job::start_missing_first(install, out.clone(), Some("MiniCrypt".into()));
        let first = loop {
            job.poll();
            if let Some(b) = job.built.first() { break b.clone() }
            assert!(!job.done, "finished without building anything: {:?}", job.last);
            std::thread::sleep(std::time::Duration::from_millis(200));
        };
        job.cancel();
        while !job.done { job.poll(); std::thread::sleep(std::time::Duration::from_millis(200)) }
        std::fs::remove_dir_all(&out).ok();
        assert_eq!(zones::slug(&first), zones::slug("MiniCrypt"));
    }
}
