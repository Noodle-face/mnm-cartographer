#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

//! M&M Cartographer -- zone maps for Monsters & Memories.

mod connections;
mod gen;

/// Single source of truth: the version in Cargo.toml. Nothing else declares it,
/// so the title bar, --version and the stamp written into generated maps can
/// never disagree.
pub const VERSION: &str = env!("CARGO_PKG_VERSION");

/// Annotation files compiled into the binary, so a lone executable is complete.
mod seed {
    include!(concat!(env!("OUT_DIR"), "/seed.rs"));
    pub static CONNECTIONS: &str = include_str!("../connections.json");
}

/// Write the built-in connections graph and seed markers into `dir` if they are
/// not already present. Never overwrites: a user's own edits win.
fn seed_assets(dir: &std::path::Path) {
    let cj = dir.join("connections.json");
    if !cj.exists() {
        std::fs::create_dir_all(dir).ok();
        std::fs::write(&cj, seed::CONNECTIONS).ok();
    }
    let md = dir.join("markers");
    for (name, body) in seed::SEED_MARKERS {
        let p = md.join(name);
        if !p.exists() {
            std::fs::create_dir_all(&md).ok();
            std::fs::write(&p, body).ok();
        }
    }
}
mod paper;
mod markers;
mod pyramid;
mod watch;

use markers::{kind, MarkerSet, Shape, KINDS};
use pyramid::{discover, scene_to_world, world_to_scene, Pyramid};
use std::collections::HashMap;
use std::path::{Path, PathBuf};

const PAPER: egui::Color32 = egui::Color32::from_rgb(0xef, 0xe6, 0xcf);
const INK: egui::Color32 = egui::Color32::from_rgb(0x1d, 0x1a, 0x16);

fn app_dir() -> PathBuf {
    std::env::current_exe()
        .ok()
        .and_then(|p| p.parent().map(Path::to_path_buf))
        .unwrap_or_else(|| PathBuf::from("."))
}

/// Everywhere maps may live, best first.
///
/// The release layout puts maps beside the executable. In a dev checkout the
/// binary sits in target/release/ while the maps are at the project root, so
/// the working directory is searched too -- otherwise running from the project
/// root finds nothing, which is a confusing first experience.
fn data_dirs() -> Vec<PathBuf> {
    let mut v = vec![app_dir().join("maps"), app_dir()];
    if let Ok(cwd) = std::env::current_dir() {
        v.push(cwd.join("maps"));
        v.push(cwd);
    }
    if let Some(d) = dirs::data_dir() {
        let app = d.join("mnm-cartographer");
        v.push(app.join("maps")); // where generation writes
        v.push(app);
    }
    // Run from the folder it lives in -- the usual case for an unzipped build --
    // and the exe dir and cwd are the same place. The duplicates interleave
    // rather than sitting adjacent, so dedup() would not catch them, and
    // listing each path twice makes the --check output read like a bug report.
    let mut seen = std::collections::HashSet::new();
    v.retain(|p| seen.insert(p.clone()));
    v
}

fn find_base() -> Option<PathBuf> {
    for d in data_dirs() {
        if discover(&d).is_empty() {
            continue;
        }
        // Prefer the directory that also holds markers/ and connections.json,
        // so annotations and the wiki graph are found alongside the tiles.
        if d.join("markers").is_dir() || d.join("connections.json").is_file() {
            return Some(d);
        }
        if let Some(up) = d.parent() {
            if up.join("markers").is_dir() || up.join("connections.json").is_file() {
                return Some(up.to_path_buf());
            }
        }
        return Some(d);
    }
    None
}

/// Every map found anywhere, not just under one root. Generated maps land in
/// the data directory while a release keeps its markers beside the binary, so
/// tying the two together loses one or the other.
fn discover_all() -> Vec<Pyramid> {
    let mut out: Vec<Pyramid> = Vec::new();
    for d in data_dirs() {
        for p in discover(&d) {
            if !out.iter().any(|o| o.name == p.name) {
                out.push(p);
            }
        }
    }
    out.sort_by(|a, b| a.name.cmp(&b.name));
    out
}

/// Where markers and connections.json live: the first searched directory that
/// actually has them, else the data directory, which is always writable.
fn asset_base() -> PathBuf {
    for d in data_dirs() {
        if d.join("markers").is_dir() || d.join("connections.json").is_file() {
            return d;
        }
    }
    dirs::data_dir()
        .map(|d| d.join("mnm-cartographer"))
        .unwrap_or_else(app_dir)
}

struct Editing {
    id: String,
    label: String,
    kind: usize,
    note: String,
    link: String,
    reqs: markers::Reqs,
    /// Level is edited as text so the field can be left blank.
    req_level: String,
    creating: bool,
}

struct App {
    base: PathBuf,
    maps: Vec<Pyramid>,
    cur: usize,
    mset: MarkerSet,
    graph: connections::Graph,
    watcher: watch::ZoneWatcher,
    follow: bool,
    show_legend: bool,
    /// world units -> screen px
    scale: f32,
    /// screen position of world origin
    offset: egui::Vec2,
    textures: HashMap<(i64, i64, i64), egui::TextureHandle>,
    fitted: bool,
    editing: Option<Editing>,
    status: String,
    last_poll: f64,
    paper: Option<egui::TextureHandle>,
    /// Marker being dragged to a new position, if any.
    dragging: Option<String>,
    /// Map generation: settings, the running job, and the last result.
    settings: gen::job::Settings,
    gen_job: Option<gen::job::Job>,
    gen_note: String,
    /// Maps panel: open state, the zone list, and what is ticked.
    maps_open: bool,
    zone_list: Vec<gen::job::ZoneEntry>,
    zone_picked: std::collections::HashSet<String>,
    zone_filter: String,
    cursor_world: Option<(f64, f64)>,
    title_shown: String,
    confirm_clear: bool,
}

impl App {
    fn new(base: PathBuf, log: Option<PathBuf>) -> Self {
        // Maps and annotations are found independently: generated maps live in
        // the data directory while markers and connections.json sit beside the
        // application. Tying them to one root loses whichever is elsewhere.
        let maps = discover_all();
        let assets = if base.join("markers").is_dir() || base.join("connections.json").is_file() {
            base.clone()
        } else {
            asset_base()
        };
        let base = assets;
        let graph = connections::Graph::load(&base);
        let watcher = watch::ZoneWatcher::new(log);
        let names: Vec<String> = maps.iter().map(|m| m.name.clone()).collect();
        let cur = watcher
            .zone
            .as_deref()
            .and_then(|z| watch::match_zone(z, &names))
            .unwrap_or(0);
        let settings = gen::job::Settings::load();
        let mset = maps
            .get(cur)
            .map(|p| MarkerSet::load(&p.markers_path(&base)))
            .unwrap_or_else(|| MarkerSet { path: base.join("markers/none.json"), items: vec![] });
        Self {
            base, maps, cur, mset, graph, watcher,
            follow: true, show_legend: true,
            scale: 1.0, offset: egui::Vec2::ZERO,
            textures: HashMap::new(), fitted: false,
            editing: None, status: String::new(), last_poll: 0.0,
            paper: None,
            dragging: None,
            cursor_world: None,
            title_shown: String::new(),
            confirm_clear: false,
            settings,
            gen_job: None,
            gen_note: String::new(),
            maps_open: false,
            zone_list: Vec::new(),
            zone_picked: Default::default(),
            zone_filter: String::new(),
        }
    }

    /// Reload the map list after a generation run, without restarting.
    fn rescan(&mut self) {
        let extra = gen::job::maps_dir();
        // Generated maps land in the data directory; prefer whatever base
        // already had maps, but fall back to the generated location.
        let _ = &extra;
        self.maps = discover_all();
        self.textures.clear();
        self.fitted = false;
        self.cur = self.cur.min(self.maps.len().saturating_sub(1));
        if let Some(p) = self.maps.get(self.cur) {
            self.mset = MarkerSet::load(&p.markers_path(&self.base));
        }
    }

    /// Re-read which zones exist and which are already built.
    fn refresh_zone_list(&mut self) {
        let Some(install) = self.settings.resolve() else {
            self.zone_list.clear();
            return;
        };
        self.zone_list = gen::job::list_zones(&install, &gen::job::maps_dir());
    }

    /// The maps window: pick zones and (re)build them. Reachable whether or
    /// not maps already exist, which the first-run panel alone was not.
    fn maps_window(&mut self, ctx: &egui::Context) {
        if !self.maps_open {
            return;
        }
        let mut open = self.maps_open;
        egui::Window::new("Maps")
            .open(&mut open)
            .default_width(420.0)
            .default_height(520.0)
            .collapsible(false)
            .show(ctx, |ui| {
                let found = self.settings.resolve();
                ui.horizontal(|ui| {
                    match &found {
                        Some(d) => {
                            ui.label(egui::RichText::new("Game install").strong());
                            ui.label(egui::RichText::new(
                                d.display().to_string()).small().weak());
                        }
                        None => {
                            ui.colored_label(egui::Color32::from_rgb(0xa0, 0x1b, 0x22),
                                "Game install not found");
                        }
                    }
                });
                ui.horizontal(|ui| {
                    if ui.button("Choose install folder...").clicked() {
                        if let Some(p) = rfd::FileDialog::new()
                            .set_title("Select the Monsters and Memories folder")
                            .pick_folder()
                        {
                            match gen::job::resolve_install(&p) {
                                Some(_) => {
                                    self.settings.install_dir = Some(p.to_path_buf());
                                    self.settings.save();
                                    self.gen_note.clear();
                                    self.refresh_zone_list();
                                }
                                None => self.gen_note = format!(
                                    "No game bundles under {}", p.display()),
                            }
                        }
                    }
                    if ui.button("Rescan").clicked() {
                        self.refresh_zone_list();
                    }
                });
                if !self.gen_note.is_empty() {
                    ui.colored_label(egui::Color32::from_rgb(0xa0, 0x1b, 0x22), &self.gen_note);
                }
                ui.separator();

                if self.zone_list.is_empty() {
                    ui.label("No zones listed yet.");
                    if found.is_some() && ui.button("Scan for zones").clicked() {
                        self.refresh_zone_list();
                    }
                } else {
                    let built = self.zone_list.iter().filter(|z| z.built).count();
                    ui.label(format!("{} zones, {} built", self.zone_list.len(), built));
                    ui.horizontal(|ui| {
                        if ui.button("All").clicked() {
                            self.zone_picked =
                                self.zone_list.iter().map(|z| z.name.clone()).collect();
                        }
                        if ui.button("None").clicked() {
                            self.zone_picked.clear();
                        }
                        if ui.button("Missing only").clicked() {
                            self.zone_picked = self.zone_list.iter()
                                .filter(|z| !z.built).map(|z| z.name.clone()).collect();
                        }
                    });
                    ui.horizontal(|ui| {
                        ui.label("Filter");
                        ui.text_edit_singleline(&mut self.zone_filter);
                    });
                    let needle = self.zone_filter.to_lowercase();
                    egui::ScrollArea::vertical().max_height(260.0).show(ui, |ui| {
                        for z in &self.zone_list {
                            if !needle.is_empty() && !z.name.to_lowercase().contains(&needle) {
                                continue;
                            }
                            let mut on = self.zone_picked.contains(&z.name);
                            let label = if z.built {
                                format!("{}  (built)", z.name)
                            } else {
                                z.name.clone()
                            };
                            if ui.checkbox(&mut on, label).changed() {
                                if on {
                                    self.zone_picked.insert(z.name.clone());
                                } else {
                                    self.zone_picked.remove(&z.name);
                                }
                            }
                        }
                    });
                }

                ui.separator();
                let busy = self.gen_job.as_ref().map_or(false, |j| !j.done);
                let n = self.zone_picked.len();
                ui.horizontal(|ui| {
                    if ui.add_enabled(found.is_some() && !busy && n > 0,
                        egui::Button::new(format!("Rebuild {n} selected"))).clicked()
                    {
                        if let Some(install) = found.clone() {
                            let out = gen::job::maps_dir();
                            std::fs::create_dir_all(&out).ok();
                            // Explicitly chosen zones are rebuilt even if present.
                            self.gen_job = Some(gen::job::Job::start(
                                install, out, Some(self.zone_picked.clone()), true));
                        }
                    }
                    if ui.add_enabled(found.is_some() && !busy,
                        egui::Button::new("Build missing")).clicked()
                    {
                        if let Some(install) = found.clone() {
                            let out = gen::job::maps_dir();
                            std::fs::create_dir_all(&out).ok();
                            self.gen_job = Some(gen::job::Job::start(install, out, None, false));
                        }
                    }
                });
                self.progress_ui(ui);
            });
        self.maps_open = open;
    }

    /// Progress, estimate and cancel. Shared by the first-run panel and the
    /// maps window so they cannot drift apart.
    fn progress_ui(&mut self, ui: &mut egui::Ui) {
        let dim = egui::Color32::from_rgb(0x6a, 0x5f, 0x50);
        let mut finished = false;
        if let Some(job) = &mut self.gen_job {
            job.poll();
            let (frac, eta) = job.progress();
            ui.add_space(10.0);
            ui.add(egui::ProgressBar::new(frac)
                .desired_width(ui.available_width().min(520.0))
                .show_percentage());
            match job.last.clone() {
                Some(gen::job::Stage::Survey { done, total, what }) => {
                    ui.label(egui::RichText::new(
                        format!("Looking for zones -- bundle {}/{}", done + 1, total)).color(INK));
                    ui.label(egui::RichText::new(what).small().color(dim));
                }
                Some(gen::job::Stage::Zone { done, total, name, level, levels }) => {
                    ui.label(egui::RichText::new(format!("Building {name}")).strong().color(INK));
                    ui.label(egui::RichText::new(format!(
                        "zone {}/{}  --  zoom level {}/{}",
                        done + 1, total, level.max(1), levels)).color(dim));
                }
                Some(gen::job::Stage::Finished { zones, tiles, bytes, failed }) => {
                    ui.label(egui::RichText::new(format!(
                        "Done: {zones} zones, {tiles} tiles, {:.0} MB",
                        bytes as f64 / 1e6)).strong().color(INK));
                    for f in failed.iter().take(4) {
                        ui.label(egui::RichText::new(f).small().color(dim));
                    }
                    finished = true;
                }
                Some(gen::job::Stage::Failed(e)) => {
                    ui.colored_label(egui::Color32::from_rgb(0xa0, 0x1b, 0x22), e);
                    finished = true;
                }
                None => { ui.label(egui::RichText::new("Starting...").color(INK)); }
            }
            let elapsed = job.started.elapsed().as_secs_f32();
            let fmt = |s: f32| {
                let s = s.max(0.0) as u32;
                if s >= 60 { format!("{}m {:02}s", s / 60, s % 60) } else { format!("{s}s") }
            };
            ui.label(egui::RichText::new(match eta {
                Some(e) => format!("elapsed {}  --  about {} remaining", fmt(elapsed), fmt(e)),
                None => format!("elapsed {}", fmt(elapsed)),
            }).small().color(dim));
            if !job.done && ui.button("Cancel").clicked() {
                job.cancel();
            }
            ui.ctx().request_repaint_after(std::time::Duration::from_millis(200));
        }
        if finished {
            self.gen_job = None;
            self.rescan();
            self.refresh_zone_list();
        }
    }

    /// The panel shown when there are no maps: locate the game, generate them.
    fn generation_ui(&mut self, ui: &mut egui::Ui) {
        // This panel sits on parchment, not egui's dark default, so every
        // piece of text needs an explicit ink colour or it washes out.
        let dim = egui::Color32::from_rgb(0x6a, 0x5f, 0x50);
        ui.vertical_centered(|ui| {
            ui.add_space(12.0);
            ui.label(egui::RichText::new("No maps yet").heading().color(INK));
            ui.add_space(6.0);
            ui.label(egui::RichText::new(
                "Maps are built from your own copy of the game. Nothing is\n\
                 downloaded, and the game does not need to be running.").color(dim));
            ui.add_space(14.0);

            let found = self.settings.resolve();
            match &found {
                Some(d) => {
                    ui.label(egui::RichText::new("Game install found").strong().color(INK));
                    ui.label(egui::RichText::new(d.display().to_string()).small().color(dim));
                }
                None => {
                    ui.colored_label(egui::Color32::from_rgb(0xa0, 0x1b, 0x22),
                        "Could not find the game automatically.");
                    ui.label(egui::RichText::new(
                        "Choose the Monsters and Memories install folder.").small().color(dim));
                }
            }
            ui.add_space(8.0);

            ui.horizontal(|ui| {
                ui.add_space(ui.available_width() / 2.0 - 150.0);
                if ui.button("Choose install folder...").clicked() {
                    if let Some(p) = rfd::FileDialog::new()
                        .set_title("Select the Monsters and Memories folder")
                        .pick_folder()
                    {
                        match gen::job::resolve_install(&p) {
                            Some(_) => {
                                self.settings.install_dir = Some(p.to_path_buf());
                                self.settings.save();
                                self.gen_note.clear();
                            }
                            None => {
                                self.gen_note = format!(
                                    "No game bundles under {} -- pick the folder containing \
                                     mnm_Data.", p.display());
                            }
                        }
                    }
                }
                let busy = self.gen_job.as_ref().map_or(false, |j| !j.done);
                if ui.add_enabled(found.is_some() && !busy,
                                  egui::Button::new("Generate maps")).clicked()
                {
                    if let Some(install) = found.clone() {
                        let out = gen::job::maps_dir();
                        std::fs::create_dir_all(&out).ok();
                        self.gen_note.clear();
                        self.gen_job = Some(gen::job::Job::start(install, out, None, false));
                    }
                }
            });

            if !self.gen_note.is_empty() {
                ui.add_space(8.0);
                ui.colored_label(egui::Color32::from_rgb(0xa0, 0x1b, 0x22), &self.gen_note);
            }

            self.progress_ui(ui);
        });
    }



    fn pyr(&self) -> Option<&Pyramid> { self.maps.get(self.cur) }

    fn open_zone(&mut self, i: usize) {
        if i >= self.maps.len() || i == self.cur && self.fitted { return }
        self.cur = i;
        self.textures.clear();
        self.fitted = false;
        let p = &self.maps[self.cur];
        self.mset = MarkerSet::load(&p.markers_path(&self.base));
    }

    fn world_to_screen(&self, wx: f64, wz: f64) -> egui::Pos2 {
        let s = world_to_scene(wx, wz);
        egui::pos2(s.x * self.scale + self.offset.x, s.y * self.scale + self.offset.y)
    }
    fn screen_to_world(&self, p: egui::Pos2) -> (f64, f64) {
        scene_to_world(egui::pos2(
            (p.x - self.offset.x) / self.scale,
            (p.y - self.offset.y) / self.scale,
        ))
    }

    /// Put a world position at the centre of the view, keeping the zoom.
    fn center_on(&mut self, wx: f64, wz: f64, vp: egui::Rect) {
        let s = world_to_scene(wx, wz);
        self.offset = vp.center().to_vec2()
            - egui::vec2(s.x * self.scale, s.y * self.scale);
        self.fitted = true;
        self.clamp(vp);
    }

    fn fit_scale(&self, vp: egui::Rect) -> f32 {
        let Some(p) = self.pyr() else { return 1.0 };
        let r = p.scene_rect();
        (vp.width() / r.width()).min(vp.height() / r.height())
    }

    fn fit(&mut self, vp: egui::Rect) {
        let Some(p) = self.pyr() else { return };
        let r = p.scene_rect();
        self.scale = self.fit_scale(vp);
        self.offset = vp.center().to_vec2()
            - egui::vec2(r.center().x * self.scale, r.center().y * self.scale);
        self.fitted = true;
    }

    /// Keep the map covering the viewport; centre it on an axis that fits.
    /// Keep the sheet in view. Once it is smaller than the viewport it is
    /// centred rather than pinned to an edge.
    fn clamp(&mut self, vp: egui::Rect) {
        let Some(p) = self.pyr() else { return };
        let r = p.scene_rect();
        let tl = self.world_to_screen(r.min.x as f64, -(r.min.y as f64));
        let br = self.world_to_screen(r.max.x as f64, -(r.max.y as f64));
        let map = egui::Rect::from_two_pos(tl, br);
        let mut d = egui::Vec2::ZERO;
        if map.width() <= vp.width() { d.x = vp.center().x - map.center().x }
        else if map.left() > vp.left() { d.x = vp.left() - map.left() }
        else if map.right() < vp.right() { d.x = vp.right() - map.right() }
        if map.height() <= vp.height() { d.y = vp.center().y - map.center().y }
        else if map.top() > vp.top() { d.y = vp.top() - map.top() }
        else if map.bottom() < vp.bottom() { d.y = vp.bottom() - map.bottom() }
        self.offset += d;
    }

    fn texture(&mut self, ctx: &egui::Context, z: i64, tx: i64, ty: i64) -> Option<egui::TextureHandle> {
        let key = (z, tx, ty);
        if let Some(t) = self.textures.get(&key) { return Some(t.clone()) }
        let bytes = self.pyr()?.tile_bytes(z, tx, ty)?;
        let img = image::load_from_memory(&bytes).ok()?.to_rgba8();
        let (w, h) = img.dimensions();
        let ci = egui::ColorImage::from_rgba_unmultiplied([w as usize, h as usize], img.as_raw());
        let t = ctx.load_texture(format!("t{z}_{tx}_{ty}"), ci, egui::TextureOptions::LINEAR);
        self.textures.insert(key, t.clone());
        Some(t)
    }
}

fn shape_points(shape: Shape, c: egui::Pos2, r: f32) -> Vec<egui::Pos2> {
    let poly = |n: usize, off: f32| -> Vec<egui::Pos2> {
        (0..n).map(|i| {
            let a = off + std::f32::consts::TAU * i as f32 / n as f32;
            egui::pos2(c.x + r * a.cos(), c.y + r * a.sin())
        }).collect()
    };
    match shape {
        Shape::Circle => poly(16, 0.0),
        Shape::Square => poly(4, std::f32::consts::FRAC_PI_4),
        Shape::Diamond => poly(4, 0.0),
        Shape::Triangle => poly(3, -std::f32::consts::FRAC_PI_2),
        Shape::Pentagon => poly(5, -std::f32::consts::FRAC_PI_2),
        Shape::Hexagon => poly(6, 0.0),
        Shape::Star => (0..10).map(|i| {
            let rr = if i % 2 == 0 { r } else { r * 0.45 };
            let a = -std::f32::consts::FRAC_PI_2 + std::f32::consts::PI * i as f32 / 5.0;
            egui::pos2(c.x + rr * a.cos(), c.y + rr * a.sin())
        }).collect(),
        Shape::Cross => {
            let w = r * 0.36;
            vec![
                egui::pos2(c.x - w, c.y - r), egui::pos2(c.x + w, c.y - r),
                egui::pos2(c.x + w, c.y - w), egui::pos2(c.x + r, c.y - w),
                egui::pos2(c.x + r, c.y + w), egui::pos2(c.x + w, c.y + w),
                egui::pos2(c.x + w, c.y + r), egui::pos2(c.x - w, c.y + r),
                egui::pos2(c.x - w, c.y + w), egui::pos2(c.x - r, c.y + w),
                egui::pos2(c.x - r, c.y - w), egui::pos2(c.x - w, c.y - w),
            ]
        }
    }
}

impl eframe::App for App {
    fn update(&mut self, ctx: &egui::Context, _f: &mut eframe::Frame) {
        // Drawn first so it floats above the map and the side panel.
        self.maps_window(ctx);
        // ---- follow the game between zones -------------------------------
        let t = ctx.input(|i| i.time);
        if self.follow && t - self.last_poll > 2.0 {
            self.last_poll = t;
            if let Some(z) = self.watcher.poll() {
                self.status = format!("game entered: {z}");
                let names: Vec<String> = self.maps.iter().map(|m| m.name.clone()).collect();
                if let Some(i) = watch::match_zone(&z, &names) {
                    self.open_zone(i);
                }
            }
            ctx.request_repaint_after(std::time::Duration::from_secs(2));
        }

        // Window title follows the zone, so a taskbar entry is identifiable.
        if let Some(p) = self.pyr() {
            let want = format!("{} - M&M Cartographer", self.graph.pretty(&p.name));
            if want != self.title_shown {
                ctx.send_viewport_cmd(egui::ViewportCommand::Title(want.clone()));
                self.title_shown = want;
            }
        }

        ctx.input(|i| {
            if i.key_pressed(egui::Key::Escape) { self.editing = None }
            if i.key_pressed(egui::Key::F) || i.key_pressed(egui::Key::Home) {
                self.fitted = false;
            }
        });

        self.sidebar(ctx);
        self.map(ctx);
        self.marker_window(ctx);

        if self.confirm_clear {
            let zone = self.pyr().map(|p| self.graph.pretty(&p.name)).unwrap_or_default();
            let n = self.mset.items.len();
            egui::Window::new("Clear markers")
                .collapsible(false).resizable(false)
                .show(ctx, |ui| {
                    ui.label(format!("Remove all {n} markers in {zone}?"));
                    ui.label(egui::RichText::new("This cannot be undone.").weak().size(11.0));
                    ui.horizontal(|ui| {
                        if ui.button("Remove them").clicked() {
                            self.mset.items.clear();
                            let _ = self.mset.save();
                            self.confirm_clear = false;
                        }
                        if ui.button("Keep").clicked() { self.confirm_clear = false }
                    });
                });
        }
    }
}

impl App {
    fn sidebar(&mut self, ctx: &egui::Context) {
        egui::SidePanel::left("side").exact_width(248.0).show(ctx, |ui| {
            ui.add_space(6.0);
            ui.heading("Zone");
            let pretty: Vec<String> =
                self.maps.iter().map(|m| self.graph.pretty(&m.name)).collect();
            let mut pick = self.cur;
            egui::ComboBox::from_id_salt("zone")
                .width(230.0)
                .selected_text(pretty.get(self.cur).cloned().unwrap_or_default())
                .show_ui(ui, |ui| {
                    let mut order: Vec<usize> = (0..pretty.len()).collect();
                    order.sort_by_key(|&i| pretty[i].to_lowercase());
                    for i in order {
                        ui.selectable_value(&mut pick, i, &pretty[i]);
                    }
                });
            if pick != self.cur { self.open_zone(pick) }

            // jump back to wherever the game actually is
            let live = self.watcher.zone.clone();
            let live_idx = live.as_deref().and_then(|z| {
                let names: Vec<String> = self.maps.iter().map(|m| m.name.clone()).collect();
                watch::match_zone(z, &names)
            });
            ui.add_enabled_ui(live_idx.is_some() && live_idx != Some(self.cur), |ui| {
                let label = match live_idx {
                    Some(i) => format!("\u{23ce} Go to live zone ({})", self.graph.pretty(&self.maps[i].name)),
                    None => "\u{23ce} Go to live zone".to_string(),
                };
                if ui.button(label).clicked() {
                    if let Some(i) = live_idx { self.open_zone(i) }
                }
            });

            ui.horizontal(|ui| {
                if ui.button("Fit view").on_hover_text("F or Home").clicked() {
                    self.fitted = false;
                }
                if ui.button("Clear markers\u{2026}").on_hover_text(
                    "Remove every marker in this zone").clicked() {
                    self.confirm_clear = true;
                }
                if ui.button("Maps\u{2026}").on_hover_text(
                    "Generate or rebuild zone maps from your game install").clicked() {
                    self.maps_open = true;
                    if self.zone_list.is_empty() {
                        self.refresh_zone_list();
                    }
                }
            });
            ui.checkbox(&mut self.follow, "Follow game (tail Player.log)");
            ui.checkbox(&mut self.show_legend, "Legend on map");
            if let Some(p) = &self.watcher.path {
                ui.add(egui::Label::new(
                    egui::RichText::new(format!("log: {}", p.display())).size(9.0).weak(),
                ).wrap());
            } else {
                ui.label(egui::RichText::new("Player.log not found").size(9.0).weak());
            }

            ui.add_space(6.0);
            ui.label(egui::RichText::new(
                "right-click the map to add a marker\n\
                 left-click a marker to open its note",
            ).size(10.0).weak());

            // ---- wiki connections for this zone --------------------------
            ui.add_space(8.0);
            ui.heading("Exits to find");
            let zone_key = self.pyr().map(|p| p.name.replace(' ', "")).unwrap_or_default();
            let placed: Vec<String> = self.mset.items.iter()
                .filter(|m| m.kind == "exit" && m.label.starts_with("to "))
                .map(|m| m.label[3..].to_lowercase())
                .collect();
            egui::ScrollArea::vertical().max_height(110.0).id_salt("conns").show(ui, |ui| {
                if self.graph.isolated.iter().any(|z| *z == zone_key) {
                    ui.label(egui::RichText::new("no connections known to the wiki").size(10.0).weak());
                }
                let boats = self.graph.boats.get(&zone_key).cloned().unwrap_or_default();
                for n in self.graph.adj.get(&zone_key).cloned().unwrap_or_default() {
                    let got = placed.iter().any(|p| *p == n.to_lowercase());
                    let mark = if got { "\u{2713}" } else { "\u{00b7}" };
                    let boat = if boats.contains(&n) { "  (boat)" } else { "" };
                    ui.label(
                        egui::RichText::new(format!("{mark} {}{boat}", self.graph.pretty(&n)))
                            .color(if got { egui::Color32::from_rgb(0x6f, 0xae, 0x5a) }
                                   else { egui::Color32::GRAY })
                            .size(11.0),
                    );
                }
                for (k, zs) in &self.graph.portals {
                    if zs.contains(&zone_key) {
                        let kp = format!("{}{}", k[..1].to_uppercase(), &k[1..]);
                        ui.label(egui::RichText::new(format!("\u{2736} {kp} portal here")).size(11.0).weak());
                    }
                }
            });

            // ---- markers -------------------------------------------------
            ui.add_space(8.0);
            ui.heading(format!("Markers ({})", self.mset.items.len()));
            let mut open_id = None;
            let mut del_id = None;
            egui::ScrollArea::vertical().id_salt("marks").show(ui, |ui| {
                let mut rows: Vec<_> = self.mset.items.iter()
                    .map(|m| (m.id.clone(), m.kind.clone(), m.label.clone(), m.x, m.z))
                    .collect();
                rows.sort_by(|a, b| (a.1.clone(), a.2.clone()).cmp(&(b.1.clone(), b.2.clone())));
                for (id, k, label, x, z) in rows {
                    let txt = format!("{:<13}{}  ({:.0}, {:.0})",
                        kind(&k).label,
                        if label.is_empty() { "\u{2014}" } else { &label }, x, z);
                    let r = ui.add(egui::Label::new(
                        egui::RichText::new(txt).color(kind(&k).color).size(11.0),
                    ).sense(egui::Sense::click()));
                    if r.clicked() { open_id = Some(id.clone()) }
                    r.context_menu(|ui| {
                        if ui.button("Delete marker").clicked() {
                            del_id = Some(id.clone());
                            ui.close_menu();
                        }
                    });
                }
            });
            if let Some(id) = del_id { self.mset.remove(&id) }
            if let Some(id) = open_id { self.begin_edit(&id, false) }

            if !self.status.is_empty() {
                ui.add_space(4.0);
                ui.label(egui::RichText::new(&self.status).size(10.0).weak());
            }
        });
    }

    /// Index of the marker under a screen position, if any.
    fn marker_at(&self, p: egui::Pos2) -> Option<usize> {
        self.mset.items.iter().position(|m| {
            (self.world_to_screen(m.x, m.z) - p).length() < 11.0
        })
    }

    /// Resolve a marker's `link` to (zone index, marker index).
    /// A bare id means this zone; "slug:id" crosses zones.
    fn resolve_link(&self, link: &str) -> Option<(usize, usize)> {
        if link.is_empty() { return None }
        let (slug, id) = match link.split_once(':') {
            Some((s, i)) => (Some(s), i),
            None => (None, link),
        };
        let zi = match slug {
            None => self.cur,
            Some(s) => self.maps.iter().position(|p| {
                p.path.file_stem().map(|f| f.to_string_lossy().eq_ignore_ascii_case(s))
                    .unwrap_or(false)
            })?,
        };
        if zi == self.cur {
            self.mset.items.iter().position(|m| m.id == id).map(|mi| (zi, mi))
        } else {
            // Another zone: we only need to know it exists, so load its file.
            let ms = MarkerSet::load(&self.maps[zi].markers_path(&self.base));
            ms.items.iter().position(|m| m.id == id).map(|mi| (zi, mi))
        }
    }

    /// The partner of a marker in THIS zone, following a link in either
    /// direction so only one end needs setting up.
    fn partner_here(&self, i: usize) -> Option<usize> {
        let m = &self.mset.items[i];
        if let Some((z, j)) = self.resolve_link(&m.link) {
            if z == self.cur && j != i { return Some(j) }
        }
        self.mset.items.iter().position(|o| {
            o.id != m.id && !o.link.is_empty()
                && self.resolve_link(&o.link).map_or(false, |(z, j)| z == self.cur && j == i)
        })
    }

    /// An `exit` marker labelled "to <Zone>" points at a map we can open.
    fn travel_target(&self, m: &markers::Marker) -> Option<usize> {
        if m.kind != "exit" { return None }
        let dest = m.label.strip_prefix("to ")?.trim();
        let key: String = dest.chars().filter(|c| c.is_ascii_alphanumeric())
            .collect::<String>().to_lowercase();
        self.maps.iter().position(|p| {
            p.name.chars().filter(|c| c.is_ascii_alphanumeric())
                .collect::<String>().to_lowercase() == key
        })
    }

    fn begin_edit(&mut self, id: &str, creating: bool) {
        if let Some(m) = self.mset.items.iter().find(|m| m.id == id) {
            self.editing = Some(Editing {
                id: id.to_string(),
                label: m.label.clone(),
                kind: KINDS.iter().position(|k| k.key == m.kind).unwrap_or(7),
                note: m.note.clone(),
                link: m.link.clone(),
                req_level: m.reqs.level.map(|l| l.to_string()).unwrap_or_default(),
                reqs: m.reqs.clone(),
                creating,
            });
        }
    }
}

impl App {
    fn map(&mut self, ctx: &egui::Context) {
        egui::CentralPanel::default()
            .frame(egui::Frame::none().fill(PAPER))
            .show(ctx, |ui| {
                let (resp, painter) =
                    ui.allocate_painter(ui.available_size(), egui::Sense::click_and_drag());
                let vp = resp.rect;

                // Parchment behind everything. Tiled in SCREEN space at 1:1 and
                // scrolled with the pan, so it slides like one big sheet under
                // the map rather than scaling with zoom.
                let tex = self.paper.get_or_insert_with(|| {
                    let img = egui::ColorImage::from_rgba_unmultiplied(
                        [paper::SIZE, paper::SIZE], &paper::texture());
                    ctx.load_texture("paper", img, egui::TextureOptions {
                        magnification: egui::TextureFilter::Linear,
                        minification: egui::TextureFilter::Linear,
                        wrap_mode: egui::TextureWrapMode::Repeat,
                        mipmap_mode: None,
                    })
                }).clone();
                let s = paper::SIZE as f32;
                let o = egui::vec2(-self.offset.x / s, -self.offset.y / s);
                painter.image(
                    tex.id(), vp,
                    egui::Rect::from_min_size(o.to_pos2(),
                        egui::vec2(vp.width() / s, vp.height() / s)),
                    egui::Color32::WHITE,
                );
                if self.maps.is_empty() {
                    // No maps: offer to build them from the user's own install
                    // rather than pointing at a download that may not exist.
                    let panel = egui::Rect::from_center_size(
                        vp.center(), egui::vec2(vp.width().min(620.0), vp.height().min(420.0)));
                    let mut ui = ui.new_child(
                        egui::UiBuilder::new()
                            .max_rect(panel)
                            .layout(egui::Layout::top_down(egui::Align::Center)),
                    );
                    self.generation_ui(&mut ui);
                    return;
                }
                if !self.fitted { self.fit(vp) }

                // ---- input -------------------------------------------------
                // Dragging a marker moves it; dragging anywhere else pans.
                if resp.drag_started_by(egui::PointerButton::Primary) {
                    self.dragging = resp.interact_pointer_pos()
                        .and_then(|p| self.marker_at(p))
                        .map(|i| self.mset.items[i].id.clone());
                }
                if resp.drag_stopped() {
                    if self.dragging.take().is_some() { let _ = self.mset.save(); }
                }
                if resp.dragged_by(egui::PointerButton::Primary) {
                    match self.dragging.clone() {
                        Some(id) => {
                            if let Some(p) = resp.interact_pointer_pos() {
                                let (wx, wz) = self.screen_to_world(p);
                                if let Some(m) = self.mset.get_mut(&id) {
                                    m.x = (wx * 100.0).round() / 100.0;
                                    m.z = (wz * 100.0).round() / 100.0;
                                }
                            }
                        }
                        None => {
                            self.offset += resp.drag_delta();
                            self.clamp(vp);
                        }
                    }
                }
                let scroll = ctx.input(|i| i.smooth_scroll_delta.y);
                if scroll.abs() > 0.1 {
                    if let Some(cursor) = resp.hover_pos() {
                        // Fit is the hard floor: the whole map, and no further.
                        // The parchment backdrop fills the letterbox bars where
                        // the map's aspect differs from the window's.
                        let floor = self.fit_scale(vp);
                        let top = self.pyr().map(|p| p.levels.last().map(|l| l.ppu).unwrap_or(4.0))
                            .unwrap_or(4.0) as f32 * 4.0;
                        let mut f = (scroll * 0.0025).exp();
                        if self.scale * f > top { f = top / self.scale }
                        if self.scale * f < floor { f = floor / self.scale }
                        if (f - 1.0).abs() > 1e-6 {
                            let before = self.screen_to_world(cursor);
                            self.scale *= f;
                            let after = self.world_to_screen(before.0, before.1);
                            self.offset += cursor - after;
                            self.clamp(vp);
                        }
                    }
                }

                // ---- tiles -------------------------------------------------
                let (lvl_z, want) = {
                    let p = self.pyr().unwrap();
                    let l = p.level_for(self.scale as f64);
                    let tl = self.screen_to_world(vp.left_top());
                    let br = self.screen_to_world(vp.right_bottom());
                    let pad = (br.0 - tl.0).abs() * 0.12;
                    (l.z, p.tiles_in(l, tl.0 - pad, br.0 + pad, br.1 - pad, tl.1 + pad))
                };
                let base_z = self.pyr().unwrap().levels[0].z;
                // keep the coarsest level resident so panning never shows gaps
                let base_tiles = {
                    let p = self.pyr().unwrap();
                    let l = &p.levels[0];
                    p.tiles_in(l, p.extent[0], p.extent[1], p.extent[2], p.extent[3])
                };
                for (z, list) in [(base_z, base_tiles), (lvl_z, want)] {
                    let lv = self.pyr().unwrap().levels.iter().find(|l| l.z == z).unwrap().clone();
                    let t = self.pyr().unwrap().tile;
                    for (tx, ty) in list {
                        let (wx, wz, ww, hh) = self.pyr().unwrap().tile_world_rect(&lv, tx, ty);
                        // Edge tiles were padded out to a full tile with paper
                        // colour when the pyramid was cut. That padding lies
                        // OUTSIDE the map and shows as a pale border, so draw
                        // only the valid part and sample only that much of the
                        // texture.
                        let vw = (((tx + 1) * t).min(lv.px[0]) - tx * t) as f64;
                        let vh = (((ty + 1) * t).min(lv.px[1]) - ty * t) as f64;
                        if vw <= 0.0 || vh <= 0.0 { continue }
                        let tf = t as f64;
                        let (short_x, short_y) = (vw < tf, vh < tf);
                        // Bleed one source pixel into the neighbour so
                        // fractional positioning cannot leave a hairline gap --
                        // but PER AXIS: a tile short in x still has a neighbour
                        // below it, and dropping its y bleed reopens that seam.
                        let bx = if short_x { 0.0 } else { 1.0 };
                        let by = if short_y { 0.0 } else { 1.0 };
                        let a = self.world_to_screen(wx, wz);
                        let b = self.world_to_screen(
                            wx + ww * (vw + bx) / tf,
                            wz - hh * (vh + by) / tf,
                        );
                        if let Some(tex) = self.texture(ctx, z, tx, ty) {
                            painter.image(
                                tex.id(),
                                egui::Rect::from_two_pos(a, b),
                                egui::Rect::from_min_max(
                                    egui::pos2(0.0, 0.0),
                                    egui::pos2((vw / tf) as f32, (vh / tf) as f32),
                                ),
                                egui::Color32::WHITE,
                            );
                        }
                    }
                    if z == lvl_z { break }
                }

                // ---- markers ------------------------------------------------
                let mut hovered: Option<usize> = None;
                let pointer = resp.hover_pos();
                let show_labels = self.scale >= self.fit_scale(vp) * 1.4
                    || self.mset.items.len() <= 6;
                self.cursor_world = pointer.map(|p| self.screen_to_world(p));
                for (i, m) in self.mset.items.iter().enumerate() {
                    let c = self.world_to_screen(m.x, m.z);
                    if !vp.expand(24.0).contains(c) { continue }
                    let ki = kind(&m.kind);
                    let pts = shape_points(ki.shape, c, 8.0);
                    painter.add(egui::Shape::convex_polygon(pts.clone(), ki.color,
                        egui::Stroke::new(2.0_f32, egui::Color32::from_rgb(0x15, 0x12, 0x0e))));
                    // Labels overlap badly at low zoom; past a threshold the
                    // glyphs alone carry it and the tooltip gives the rest.
                    if !m.label.is_empty() && show_labels {
                        painter.text(c + egui::vec2(11.0, -9.0), egui::Align2::LEFT_TOP,
                            &m.label, egui::FontId::proportional(11.0),
                            egui::Color32::from_rgb(0x1b, 0x15, 0x10));
                    }
                    if pointer.map_or(false, |p| (p - c).length() < 11.0) { hovered = Some(i) }
                }

                // A hovered marker shows its pair, so a teleporter link is
                // visible without opening anything.
                if let Some(i) = hovered {
                    if let Some(j) = self.partner_here(i) {
                        let a = self.world_to_screen(self.mset.items[i].x, self.mset.items[i].z);
                        let b = self.world_to_screen(self.mset.items[j].x, self.mset.items[j].z);
                        let col = kind(&self.mset.items[i].kind).color;
                        painter.add(egui::Shape::dashed_line(
                            &[a, b], egui::Stroke::new(2.5_f32, col), 9.0, 6.0));
                        painter.circle_stroke(b, 13.0, egui::Stroke::new(2.5_f32, col));
                    }
                }

                // hover text follows the cursor
                if let (Some(i), Some(p)) = (hovered, pointer) {
                    let m = &self.mset.items[i];
                    let ki = kind(&m.kind);
                    let brief = m.note.lines().next().filter(|s| !s.is_empty())
                        .unwrap_or(ki.about);
                    egui::show_tooltip_at(ctx, resp.layer_id, egui::Id::new("mk_tip"),
                        p + egui::vec2(14.0, 14.0), |ui| {
                            ui.label(egui::RichText::new(
                                if m.label.is_empty() { ki.label } else { &m.label }).strong());
                            ui.horizontal(|ui| {
                                let (r, _) = ui.allocate_exact_size(egui::vec2(9.0, 9.0), egui::Sense::hover());
                                ui.painter().rect_filled(r, 1.0, ki.color);
                                ui.label(egui::RichText::new(format!(
                                    "{}   X {:.0}, Z {:.0}", ki.label, m.x, m.z)).size(11.0).weak());
                            });
                            if !m.reqs.is_empty() {
                                ui.label(egui::RichText::new(
                                    format!("requires {}", m.reqs.summary()))
                                    .size(11.0)
                                    .color(egui::Color32::from_rgb(0xa8, 0x6a, 0x12)));
                            }
                            ui.label(egui::RichText::new(brief).size(11.0));
                            match self.travel_target(m) {
                                Some(z) => {
                                    ui.label(egui::RichText::new(format!(
                                        "click to travel to {}",
                                        self.graph.pretty(&self.maps[z].name)))
                                        .size(10.0).color(egui::Color32::from_rgb(0x1b,0xaf,0x7a)));
                                }
                                None if !m.note.is_empty() => {
                                    ui.label(egui::RichText::new("click for the full note")
                                        .size(10.0).weak());
                                }
                                None => {}
                            }
                        });
                }

                // ---- clicks -------------------------------------------------
                if resp.clicked() {
                    if let Some(i) = hovered {
                        let m = self.mset.items[i].clone();
                        // A link wins over the "to <Zone>" travel behaviour:
                        // it is more specific, pointing at a spot not a zone.
                        if let Some((z, j)) = self.resolve_link(&m.link) {
                            if z != self.cur { self.open_zone(z) }
                            let (tx, tz) = {
                                let ms = if z == self.cur { None }
                                    else { Some(MarkerSet::load(
                                        &self.maps[z].markers_path(&self.base))) };
                                match &ms {
                                    Some(o) => (o.items[j].x, o.items[j].z),
                                    None => (self.mset.items[j].x, self.mset.items[j].z),
                                }
                            };
                            self.center_on(tx, tz, vp);
                            self.status = format!("followed link to {}",
                                self.graph.pretty(&self.maps[z].name));
                        } else {
                            match self.travel_target(&m) {
                                Some(z) => {
                                    self.status = format!("travelled to {}",
                                        self.graph.pretty(&self.maps[z].name));
                                    self.open_zone(z);
                                }
                                None => self.begin_edit(&m.id, false),
                            }
                        }
                    }
                }
                if resp.clicked_by(egui::PointerButton::Secondary) {
                    if let Some(p) = resp.interact_pointer_pos() {
                        if let Some(i) = hovered {
                            let id = self.mset.items[i].id.clone();
                            self.begin_edit(&id, false);
                        } else {
                            let (wx, wz) = self.screen_to_world(p);
                            // create provisionally so the dialog edits a real
                            // record; removed again if the user cancels
                            let id = self.mset.add(wx, wz, "", KINDS[0].key, "");
                            self.begin_edit(&id, true);
                        }
                    }
                }

                if self.show_legend { self.legend(ui, vp) }
                self.scale_bar(ui, vp);
            });
    }

    /// Scale bar and cursor position, bottom right.
    ///
    /// A map without a scale is a picture. The readout is in game world
    /// coordinates so a position can be read off and shared directly.
    fn scale_bar(&self, ui: &mut egui::Ui, vp: egui::Rect) {
        if self.maps.is_empty() { return }
        // Pick a round number of world units that lands near 150px on screen.
        let target = 150.0 / self.scale.max(1e-6);
        let mut unit = 10.0f32;
        for c in [10.0, 20.0, 50.0, 100.0, 200.0, 500.0, 1000.0, 2000.0, 5000.0] {
            if c <= target { unit = c } else { break }
        }
        let w = unit * self.scale;
        let p = ui.painter();
        let (x1, y) = (vp.right() - 16.0, vp.bottom() - 18.0);
        let x0 = x1 - w;
        let bar = egui::Rect::from_min_max(egui::pos2(x0, y), egui::pos2(x1, y + 7.0));
        p.rect_filled(bar, 1.0, egui::Color32::from_rgb(0xfa, 0xf4, 0xe4));
        p.rect_filled(
            egui::Rect::from_min_max(bar.min, egui::pos2(x0 + w / 2.0, y + 7.0)), 1.0, INK);
        p.rect_stroke(bar, 1.0, egui::Stroke::new(1.0_f32, INK));
        p.text(egui::pos2(x1, y - 4.0), egui::Align2::RIGHT_BOTTOM,
            format!("{unit:.0} units"), egui::FontId::proportional(11.0), INK);
        if let Some((wx, wz)) = self.cursor_world {
            p.text(egui::pos2(x1, y - 20.0), egui::Align2::RIGHT_BOTTOM,
                format!("X {wx:.0}   Z {wz:.0}"),
                egui::FontId::monospace(11.0), INK);
        }
    }

    fn legend(&self, ui: &mut egui::Ui, vp: egui::Rect) {
        let mut counts: Vec<(&str, usize)> = Vec::new();
        for k in KINDS {
            let n = self.mset.items.iter().filter(|m| m.kind == k.key).count();
            if n > 0 { counts.push((k.key, n)) }
        }
        if counts.is_empty() { return }
        let row = 20.0;
        let h = row * (counts.len() as f32 + 1.0) + 16.0;
        let rect = egui::Rect::from_min_size(
            egui::pos2(vp.left() + 14.0, vp.bottom() - h - 14.0),
            egui::vec2(132.0, h),
        );
        let p = ui.painter();
        p.rect(rect, 5.0, PAPER.gamma_multiply(0.95),
            egui::Stroke::new(2.0_f32, INK));
        p.text(rect.left_top() + egui::vec2(10.0, 8.0), egui::Align2::LEFT_TOP,
            "Legend", egui::FontId::proportional(12.0), INK);
        for (i, (k, n)) in counts.iter().enumerate() {
            let cy = rect.top() + 10.0 + row * (i as f32 + 1.0) + 6.0;
            let ki = kind(k);
            p.add(egui::Shape::convex_polygon(
                shape_points(ki.shape, egui::pos2(rect.left() + 22.0, cy), 7.0),
                ki.color, egui::Stroke::new(2.0_f32, egui::Color32::from_rgb(0x15, 0x12, 0x0e))));
            p.text(egui::pos2(rect.left() + 40.0, cy), egui::Align2::LEFT_CENTER,
                format!("{}  {n}", ki.label), egui::FontId::proportional(11.0), INK);
        }
    }

    fn marker_window(&mut self, ctx: &egui::Context) {
        // Resolve the travel target before taking a mutable borrow of
        // self.editing -- it needs &self.maps and &self.graph.
        let Some(edit_id) = self.editing.as_ref().map(|e| e.id.clone()) else { return };
        let travel = self.mset.items.iter().find(|m| m.id == edit_id)
            .and_then(|m| self.travel_target(m));
        let zname = travel.map(|z| self.graph.pretty(&self.maps[z].name)).unwrap_or_default();
        // Other markers in this zone, to pair with.
        let others: Vec<(String, String, egui::Color32)> = self.mset.items.iter()
            .filter(|m| m.id != edit_id)
            .map(|m| (m.id.clone(),
                      format!("{}  {}  ({:.0}, {:.0})", kind(&m.kind).label,
                              if m.label.is_empty() { "\u{2014}" } else { &m.label }, m.x, m.z),
                      kind(&m.kind).color))
            .collect();
        let link_label = self.editing.as_ref()
            .and_then(|e| others.iter().find(|(id, _, _)| *id == e.link))
            .map(|(_, t, _)| t.clone())
            .unwrap_or_else(|| "(another zone)".to_string());

        let Some(ed) = &mut self.editing else { return };
        let mut open = true;
        let mut save = false;
        let mut cancel = false;
        let mut delete = false;
        let mut go: Option<usize> = None;
        let title = if ed.creating { "New marker".to_string() } else {
            if ed.label.is_empty() { KINDS[ed.kind].label.to_string() } else { ed.label.clone() }
        };
        egui::Window::new(title)
            .open(&mut open)
            .resizable(true)
            .default_width(420.0)
            .show(ctx, |ui| {
                egui::Grid::new("mk").num_columns(2).spacing([8.0, 6.0]).show(ui, |ui| {
                    ui.label("Label");
                    ui.text_edit_singleline(&mut ed.label);
                    ui.end_row();
                    ui.label("Kind");
                    egui::ComboBox::from_id_salt("mkkind")
                        .width(280.0)
                        .selected_text(KINDS[ed.kind].label)
                        .show_ui(ui, |ui| {
                            for (i, k) in KINDS.iter().enumerate() {
                                ui.selectable_value(
                                    &mut ed.kind, i,
                                    egui::RichText::new(format!("{}  -  {}", k.label, k.about))
                                        .color(k.color),
                                );
                            }
                        });
                    ui.end_row();
                    ui.label("Linked to");
                    egui::ComboBox::from_id_salt("mklink")
                        .width(280.0)
                        .selected_text(if ed.link.is_empty() { "\u{2014} none".to_string() }
                                       else { link_label.clone() })
                        .show_ui(ui, |ui| {
                            ui.selectable_value(&mut ed.link, String::new(), "\u{2014} none");
                            for (id, txt, col) in &others {
                                ui.selectable_value(&mut ed.link, id.clone(),
                                    egui::RichText::new(txt).color(*col));
                            }
                        });
                    ui.end_row();
                });
                ui.label(egui::RichText::new(
                    "Pair teleporters: hovering either end draws the connection, \
                     clicking jumps to the other.").size(10.0).weak());
                ui.add_space(6.0);
                // Requirements as fields rather than prose, so "who can do
                // this" is readable at a glance instead of buried in the note.
                ui.label("Requirements");
                egui::Grid::new("reqs").num_columns(2).spacing([8.0, 4.0]).show(ui, |ui| {
                    ui.label("Level");
                    ui.add(egui::TextEdit::singleline(&mut ed.req_level)
                        .desired_width(60.0).hint_text("any"));
                    ui.end_row();
                    ui.label("Class");
                    ui.add(egui::TextEdit::singleline(&mut ed.reqs.class)
                        .desired_width(f32::INFINITY).hint_text("any -- or Cleric, Druid"));
                    ui.end_row();
                    ui.label("Faction");
                    ui.add(egui::TextEdit::singleline(&mut ed.reqs.faction)
                        .desired_width(f32::INFINITY).hint_text("any -- or Ashira, amiable"));
                    ui.end_row();
                    ui.label("Other");
                    ui.add(egui::TextEdit::singleline(&mut ed.reqs.other)
                        .desired_width(f32::INFINITY)
                        .hint_text("a prerequisite quest, an item, a key..."));
                    ui.end_row();
                });
                ui.add_space(4.0);
                ui.label("Note");
                ui.add(egui::TextEdit::multiline(&mut ed.note)
                    .desired_rows(8).desired_width(f32::INFINITY)
                    .hint_text("Spawn timer, pathing, who to bring, what drops here..."));
                ui.add_space(6.0);
                ui.horizontal(|ui| {
                    if ui.button(if ed.creating { "Create" } else { "Save" }).clicked() { save = true }
                    if ui.button("Cancel").clicked() { cancel = true }
                    if !ed.creating && ui.button("Delete").clicked() { delete = true }
                    if let Some(z) = travel {
                        if ui.button(format!("Open {zname}")).clicked() { go = Some(z) }
                    }
                });
            });

        let creating = ed.creating;
        let id = ed.id.clone();
        let (label, kindi, note, link) =
            (ed.label.clone(), ed.kind, ed.note.clone(), ed.link.clone());
        let mut reqs = ed.reqs.clone();
        // A blank or unparseable level means "no level requirement" rather
        // than zero, which would read as a real gate.
        reqs.level = ed.req_level.trim().parse::<u32>().ok().filter(|l| *l > 0);
        if save {
            if let Some(m) = self.mset.get_mut(&id) {
                m.label = label; m.kind = KINDS[kindi].key.into(); m.note = note;
                m.link = link; m.reqs = reqs;
            }
            let _ = self.mset.save();
            self.editing = None;
        } else if delete {
            self.mset.remove(&id);
            self.editing = None;
        } else if cancel || !open {
            if creating { self.mset.remove(&id) }
            self.editing = None;
        }
        if let Some(z) = go {
            self.editing = None;
            self.open_zone(z);
        }
    }
}

fn main() -> eframe::Result<()> {
    // Rust ignores SIGPIPE, so piping this program's output into `head` makes
    // the next write fail and panic. Restore the default so it exits quietly
    // like every other command-line tool.
    #[cfg(unix)]
    unsafe {
        libc::signal(libc::SIGPIPE, libc::SIG_DFL);
    }
    let args: Vec<String> = std::env::args().skip(1).collect();
    if args.iter().any(|a| a == "--version" || a == "-V") {
        println!("mnm-cartographer {VERSION}");
        return Ok(());
    }
    let check = args.iter().any(|a| a == "--check");
    // Several flags take a value, and those values must not be mistaken for the
    // positional annotation directory. Mark every index that belongs to a flag
    // before looking for the lone bare argument.
    //
    // Getting this wrong is not harmless: `--generate barracks` once read
    // "barracks" as the annotation root and created a directory of that name.
    let mut consumed = std::collections::HashSet::new();
    for (i, a) in args.iter().enumerate() {
        let takes = match a.as_str() {
            "--log" | "--out" => 1,
            // Optional value: only if the next argument is not itself a flag.
            "--generate" => usize::from(args.get(i + 1).is_some_and(|n| !n.starts_with("--"))),
            "--extract" => 2,
            "--render" => 4,
            _ => 0,
        };
        for k in 1..=takes {
            consumed.insert(i + k);
        }
    }
    let log = args
        .iter()
        .position(|a| a == "--log")
        .and_then(|i| args.get(i + 1))
        .map(PathBuf::from);
    let explicit_base = args
        .iter()
        .enumerate()
        .find(|(i, a)| !a.starts_with("--") && !consumed.contains(i))
        .map(|(_, a)| PathBuf::from(a));
    let base = explicit_base
        .clone()
        // `base` is the ANNOTATION root -- markers and connections.json. Maps
        // are discovered separately, because generated maps live in the data
        // directory while a release keeps its markers beside the binary.
        .or_else(|| find_base().filter(|d| {
            d.join("markers").is_dir() || d.join("connections.json").is_file()
        }))
        .unwrap_or_else(asset_base);
    // A bare executable has neither file next to it; plant the built-in copies
    // in the data directory so it works wherever the user put it. Only ever
    // into a directory we chose -- never into a path the user named, where
    // creating one would be a surprise.
    if explicit_base.is_none()
        && !(base.join("markers").is_dir() || base.join("connections.json").is_file())
    {
        seed_assets(&base);
    }

    // Developer check: extract geometry and report what the scene yielded,
    // so Rust output can be compared against the Python generator it replaces.
    if let Some(i) = args.iter().position(|a| a == "--extract") {
        let bundle = args.get(i + 1).expect("--extract <bundle> <scene substring>");
        let filter = args.get(i + 2).map(|s| s.to_lowercase()).unwrap_or_default();
        let t0 = std::time::Instant::now();
        let env = gen::bundle::Env::open(std::path::Path::new(bundle)).unwrap();
        println!("opened in {:.2}s, {} serialized files", t0.elapsed().as_secs_f32(), env.file_count());
        let scenes = env.scene_index();
        let mut want: Vec<usize> = Vec::new();
        for (path, cab) in &scenes {
            if (filter.is_empty() || path.to_lowercase().contains(&filter))
                && gen::is_geometry_scene(path)
            {
                println!("  scene {path}");
                if let Some(fi) = env.file_index(cab) { want.push(fi) }
            }
        }
        want.sort_unstable();
        want.dedup();
        let t1 = std::time::Instant::now();
        let f = gen::extract::extract_floors(&env, &want);
        println!("up-facing tris: {}  unresolved meshes: {}", f.tris.len(), f.unresolved);
        println!("{:#?}", f.stats);
        println!("extracted in {:.2}s (total {:.2}s)", t1.elapsed().as_secs_f32(), t0.elapsed().as_secs_f32());
        if !f.tris.is_empty() {
            let (mut lo, mut hi) = ([f32::MAX; 3], [f32::MIN; 3]);
            for t in &f.tris { for v in t { for k in 0..3 {
                lo[k] = lo[k].min(v[k]); hi[k] = hi[k].max(v[k]); } } }
            println!("bounds x {:.1}..{:.1}  y {:.1}..{:.1}  z {:.1}..{:.1}",
                     lo[0], hi[0], lo[1], hi[1], lo[2], hi[2]);
        }
        return Ok(());
    }

    // Compare the hand-written primitives against the scipy originals.
    // Render one zone to a PNG at a given resolution, for eyeball comparison
    // against the maps the Python generator produced.
    if let Some(i) = args.iter().position(|a| a == "--render") {
        let bundle = args.get(i + 1).expect("--render <bundle> <scene> <ppu> <out.png>");
        let filter = args.get(i + 2).map(|s| s.to_lowercase()).unwrap_or_default();
        let ppu: f64 = args.get(i + 3).and_then(|s| s.parse().ok()).unwrap_or(1.0);
        let out = args.get(i + 4).cloned().unwrap_or_else(|| "zone.png".into());
        let t0 = std::time::Instant::now();
        let env = gen::bundle::Env::open(std::path::Path::new(bundle)).unwrap();
        let scenes = env.scene_index();
        let mut files: Vec<usize> = Vec::new();
        for (path, cab) in &scenes {
            if (filter.is_empty() || path.to_lowercase().contains(&filter))
                && gen::is_geometry_scene(path)
            {
                if let Some(fi) = env.file_index(cab) { files.push(fi) }
            }
        }
        files.sort_unstable();
        files.dedup();
        let f = gen::extract::extract_floors(&env, &files);
        println!("tris {} (unresolved {})", f.tris.len(), f.unresolved);
        let sea = gen::scene::find_sea_level(&env, &files);
        println!("sea level {:?}", sea);
        let bbox = gen::raster::auto_bbox(&f.tris);
        println!("bbox {:.1}..{:.1} x {:.1}..{:.1}", bbox.0, bbox.1, bbox.2, bbox.3);
        let edges = gen::raster::global_band_edges(&f.tris, 8);
        let hgt = gen::raster::rasterize(&f.tris, ppu, bbox);
        println!("raster {}x{} ({:.1}s)", hgt.w, hgt.h, t0.elapsed().as_secs_f32());
        let props = gen::scene::scene_props(&env, &files);
        let props: std::collections::BTreeMap<_, _> = props.iter()
            .map(|(k, v)| (k.clone(), gen::scene::thin(v, 55.0))).collect();
        let r = gen::ink::render(&hgt, &edges, &gen::ink::InkOptions {
            ppu, step_thresh: 3.2, sea, seed: 5,
        });
        let mut rgb = r.img;
        let on_land = gen::ink::props_on_land(&props, &r.dry, bbox);
        println!("props {:?}", on_land.iter().map(|(k, v)| (k.as_str(), v.len())).collect::<Vec<_>>());
        gen::ink::draw_props(&mut rgb, bbox, ppu, &on_land);
        let mut buf = image::RgbImage::new(rgb.w as u32, rgb.h as u32);
        for (i, p) in rgb.v.iter().enumerate() {
            buf.put_pixel((i % rgb.w) as u32, (i / rgb.w) as u32,
                image::Rgb([(p[0]*255.0) as u8, (p[1]*255.0) as u8, (p[2]*255.0) as u8]));
        }
        buf.save(&out).unwrap();
        println!("wrote {out} in {:.1}s total", t0.elapsed().as_secs_f32());
        return Ok(());
    }

    // Wipe generated maps so a run can be tested from nothing. Only ever
    // touches the containers it generated -- markers and connections.json are
    // hand-made and are never deleted here.
    if args.iter().any(|a| a == "--clean") {
        let dir = gen::job::maps_dir();
        let mut n = 0usize;
        let mut bytes = 0u64;
        if let Ok(rd) = std::fs::read_dir(&dir) {
            for e in rd.flatten() {
                let p = e.path();
                let name = p.file_name().and_then(|s| s.to_str()).unwrap_or("");
                if name.ends_with(".mbtiles") || name.ends_with(".mbtiles.part") {
                    bytes += e.metadata().map(|m| m.len()).unwrap_or(0);
                    if std::fs::remove_file(&p).is_ok() {
                        println!("  removed {name}");
                        n += 1;
                    }
                }
            }
        }
        if args.iter().any(|a| a == "--all") {
            // The survey cache is derived from the bundles; dropping it forces
            // the next run to rescan, which is what a cold-start test wants.
            let c = dir.parent().map(|d| d.join("zone-cache.json"));
            if let Some(c) = c {
                if c.exists() && std::fs::remove_file(&c).is_ok() {
                    println!("  removed zone-cache.json (next run rescans bundles)");
                }
            }
        }
        println!("{n} map(s) removed, {:.0} MB freed, from {}", bytes as f64 / 1e6, dir.display());
        if n == 0 {
            println!("(nothing to remove -- markers and connections.json are never touched)");
        }
        return Ok(());
    }

    // What zones does this install actually ship? Read from the bundles, so
    // the answer comes from the game rather than a list we maintain.
    if args.iter().any(|a| a == "--list-zones") {
        let settings = gen::job::Settings::load();
        let Some(install) = settings.resolve() else {
            eprintln!("Game install not found. Set MNM_BUNDLES or choose it in the app.");
            return Ok(());
        };
        let out = gen::job::maps_dir();
        let zs = gen::job::list_zones(&install, &out);
        println!("{} zones in {}", zs.len(), install.display());
        for z in &zs {
            println!("  [{}] {}", if z.built { "x" } else { " " }, z.name);
        }
        println!("{} built, {} missing",
                 zs.iter().filter(|z| z.built).count(),
                 zs.iter().filter(|z| !z.built).count());
        return Ok(());
    }

    // Headless generation, same machinery the button uses.
    if let Some(i) = args.iter().position(|a| a == "--generate") {
        let only = args.get(i + 1)
            .filter(|s| !s.starts_with("--") && args.get(i) != Some(&"--out".to_string()))
            .cloned();
        let settings = gen::job::Settings::load();
        let Some(install) = settings.resolve() else {
            eprintln!("Game install not found. Set MNM_BUNDLES, or choose the folder in the app.");
            return Ok(());
        };
        println!("install  {}", install.display());
        let out = args.iter().position(|a| a == "--out")
            .and_then(|i| args.get(i + 1))
            .map(std::path::PathBuf::from)
            .unwrap_or_else(gen::job::maps_dir);
        std::fs::create_dir_all(&out).ok();
        println!("output   {}", out.display());
        let t0 = std::time::Instant::now();
        let located = gen::zones::survey_cached(&install, |d, t, what| {
            if !what.is_empty() { println!("  scanning {}/{}  {}", d + 1, t, what) }
        });
        let wanted: Vec<_> = located.into_iter()
            .filter(|l| only.as_ref().map_or(true, |o| l.zone.to_lowercase().contains(&o.to_lowercase())))
            .collect();
        println!("{} zones to build", wanted.len());
        let mut by_bundle: std::collections::BTreeMap<std::path::PathBuf, Vec<gen::zones::Located>> =
            Default::default();
        let many = wanted.len() > 1;
        for l in wanted { by_bundle.entry(l.bundle.clone()).or_default().push(l) }
        let (mut tiles_n, mut bytes_n, mut zones_n) = (0usize, 0u64, 0usize);
        use rayon::prelude::*;
        let lanes = if gen::profiling() && many { 1 } else { gen::job::zone_concurrency() };
        let pool = rayon::ThreadPoolBuilder::new().num_threads(lanes).build().unwrap();
        println!("building {lanes} zone(s) at a time");
        // Profiling forces one zone at a time so stage lines do not interleave --
        // but only when there is more than one zone to build. Shrinking the pool
        // also shrinks the pool that the *nested* parallel sections run on, which
        // made the profile report serial times for work that is parallel in a
        // real run.
        let force = args.iter().any(|a| a == "--force");
        for (bundle, group) in by_bundle {
            // Decide what is left to do BEFORE opening the bundle: these files
            // run to 2 GB, and a resumed run should not pay to open one just to
            // discover every zone in it is already built.
            let todo: Vec<_> = group.into_iter().filter(|l| {
                let dest = out.join(format!("{}.mbtiles", gen::zones::slug(&l.zone)));
                if dest.exists() && !force {
                    println!("  {:<24} already built (--force to rebuild)", l.zone);
                    false
                } else { true }
            }).collect();
            if todo.is_empty() { continue }
            let env = match gen::bundle::Env::open(&bundle) {
                Ok(e) => e,
                Err(e) => { eprintln!("  !! {}: {e}", bundle.display()); continue }
            };
            let out = &out;
            let res: Vec<(usize, u64, bool)> = pool.install(|| todo.par_iter().map(|l| {
                let t1 = std::time::Instant::now();
                let dest = out.join(format!("{}.mbtiles", gen::zones::slug(&l.zone)));
                let (tris, sea, props) = gen::tiles::load_zone(&env, &l.group);
                if tris.is_empty() {
                    println!("  {:<24} skipped (no geometry)", l.zone);
                    return (0, 0, false);
                }
                match gen::tiles::build(&dest,
                    &gen::tiles::ZoneInput { name: &l.zone, tris: &tris, sea, props: &props },
                    &gen::tiles::Settings::default(), |_, _| {})
                {
                    Ok((n, b)) => {
                        println!("  {:<24} {:>7} tris {:>5} tiles {:>6.1} MB  {:.1}s",
                                 l.zone, tris.len(), n, b as f64 / 1e6, t1.elapsed().as_secs_f32());
                        (n, b, true)
                    }
                    Err(e) => { eprintln!("  !! {}: {e}", l.zone); (0, 0, false) }
                }
            }).collect());
            for (n, b, ok) in res {
                tiles_n += n; bytes_n += b; if ok { zones_n += 1 }
            }
        }
        println!("\n{zones_n} zones, {tiles_n} tiles, {:.0} MB in {:.0}s",
                 bytes_n as f64 / 1e6, t0.elapsed().as_secs_f32());
        return Ok(());
    }

    if args.iter().any(|a| a == "--selftest") {
        use gen::grid::*;
        let (w, h) = (64usize, 48usize);
        let src = noise(w, h, 7);
        let m = Mask { w, h, v: src.v.iter().map(|t| *t > 0.55).collect() };
        let sum = |g: &Grid| g.v.iter().map(|t| *t as f64).sum::<f64>();
        println!("input_sum {:.6}", sum(&src));
        println!("mask_count {}", m.count());
        println!("gauss2_sum {:.6}", sum(&gaussian(&src, 2.0)));
        println!("gauss_aniso_sum {:.6}", sum(&gaussian_xy(&src, 0.6, 3.0)));
        println!("edt_sum {:.6}", sum(&distance_to_background(&m)));
        println!("maxf5_sum {:.6}", sum(&max_filter(&src, 5)));
        println!("minf5_sum {:.6}", sum(&min_filter(&src, 5)));
        println!("median3_sum {:.6}", sum(&median3(&src)));
        let (lab, n) = label(&m);
        println!("label_n {}", n);
        println!("label_sum {}", lab.iter().map(|t| *t as u64).sum::<u64>());
        println!("skel_count {}", skeletonize(&m).count());
        // A thick diagonal band, which is what the wall detector actually emits.
        let mut band = Mask::new(w, h, false);
        for y in 0..h { for x in 0..w {
            let d = (x as f32 * 0.6 + y as f32 * 0.8 - 30.0).abs();
            if d < 6.0 { band.v[y * w + x] = true }
        }}
        println!("band_count {}", band.count());
        println!("band_skel_count {}", skeletonize(&band).count());
        println!("dilate1_count {}", dilate(&m, &disk(1)).count());
        println!("erode1_count {}", erode(&m, &disk(1)).count());
        println!("closing2_count {}", closing(&m, &disk(2)).count());
        return Ok(());
    }

    if args.iter().any(|a| a == "--dump-paper") {
        // Write the generated tile out so it can be inspected for seams.
        let data = paper::texture();
        let img = image::RgbaImage::from_raw(
            paper::SIZE as u32, paper::SIZE as u32, data).unwrap();
        img.save("paper.png").unwrap();
        println!("wrote paper.png ({0}x{0})", paper::SIZE);
        return Ok(());
    }

    if check {
        // Diagnostics without opening a window: what did it find, and where?
        println!("version       {VERSION}");
        println!("base          {}", base.display());
        println!("searched      {}", data_dirs().iter()
            .map(|p| p.display().to_string()).collect::<Vec<_>>().join("\n              "));
        let maps = discover_all();
        println!("zones         {}", maps.len());
        println!("markers from  {}", base.display());
        let g = connections::Graph::load(&base);
        println!("wiki edges    {}", g.adj.values().map(|v| v.len()).sum::<usize>() / 2);
        let w = watch::ZoneWatcher::new(log.clone());
        println!("Player.log    {}", w.path.as_ref()
            .map(|p| p.display().to_string()).unwrap_or_else(|| "not found".into()));
        if w.path.is_none() {
            // Say where it looked, so a tester can report the real location.
            println!("log searched  {}", watch::candidates().iter()
                .map(|p| p.display().to_string())
                .collect::<Vec<_>>().join("\n              "));
            println!("              (override with --log <path to Player.log>)");
        }
        println!("current zone  {}", w.zone.clone().unwrap_or_else(|| "-".into()));
        let mut total = 0;
        for m in &maps {
            let ms = MarkerSet::load(&m.markers_path(&base));
            total += ms.items.len();
            let lv: Vec<String> = m.levels.iter().map(|l| format!("{}", l.ppu)).collect();
            if ms.items.is_empty() { continue }
            println!("  {:<22} ppu {:<22} {} markers",
                m.name, lv.join("/"), ms.items.len());
        }
        println!("markers       {total}");
        // Report pairings so links can be verified without opening the UI.
        for m in &maps {
            let ms = MarkerSet::load(&m.markers_path(&base));
            for mk in ms.items.iter().filter(|k| !k.link.is_empty()) {
                let target = ms.items.iter().find(|o| o.id == mk.link);
                println!("  link  {:<16} {} -> {}", m.name,
                    if mk.label.is_empty() { &mk.kind } else { &mk.label },
                    target.map(|t| if t.label.is_empty() { t.kind.clone() }
                                   else { t.label.clone() })
                          .unwrap_or_else(|| format!("{} (other zone or missing)", mk.link)));
            }
        }
        return Ok(());
    }
    let opts = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_inner_size([1280.0, 860.0])
            .with_min_inner_size([720.0, 520.0])
            .with_title(format!("M&M Cartographer {VERSION}")),
        ..Default::default()
    };
    eframe::run_native(
        "M&M Cartographer",
        opts,
        Box::new(move |_cc| Ok(Box::new(App::new(base, log)))),
    )
}
