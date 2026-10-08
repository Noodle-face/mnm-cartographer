#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

//! M&M Cartographer -- zone maps for Monsters & Memories.

mod connections;
mod gen;
mod share;
mod subscribe;

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
mod icons;
mod markers;
mod places;
mod overlay;
mod pyramid;
mod update;
mod watch;

use markers::{kind, MarkerSet, KINDS};
use pyramid::{discover, scene_to_world, world_to_scene, Pyramid};
use std::collections::HashMap;
use std::path::{Path, PathBuf};

const PAPER: egui::Color32 = egui::Color32::from_rgb(0xef, 0xe6, 0xcf);
const INK: egui::Color32 = egui::Color32::from_rgb(0x1d, 0x1a, 0x16);

/// The lettering face for place names: IM FELL English SC (SIL OFL 1.1, see
/// assets/fonts/OFL.txt), a cut of 17th-century printing type that sits
/// naturally on the parchment the maps are rendered on.
const MAP_FONT: &[u8] = include_bytes!("../assets/fonts/IMFellEnglishSC.ttf");

/// Place names on the map: darker than the kind's colour, which also has to
/// read on the dark side panel.
const PLACE_INK: egui::Color32 = egui::Color32::from_rgb(0x4e, 0x35, 0x20);

fn install_fonts(ctx: &egui::Context) {
    let mut fonts = egui::FontDefinitions::default();
    fonts.font_data.insert("map".into(), egui::FontData::from_static(MAP_FONT));
    // Proportional as a fallback for glyphs the face lacks, such as arrows.
    let mut chain = vec!["map".to_owned()];
    chain.extend(fonts.families[&egui::FontFamily::Proportional].iter().cloned());
    fonts.families.insert(egui::FontFamily::Name("map".into()), chain);
    ctx.set_fonts(fonts);
}

fn map_font(size: f32) -> egui::FontId {
    egui::FontId::new(size, egui::FontFamily::Name("map".into()))
}

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

struct PendingImport {
    label: String,
    pack: share::Pack,
    /// (zone slug, would add, already present)
    effect: Vec<(String, usize, usize)>,
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
    /// Which floor it is on, in a zone with floors; see Marker::floor.
    floor: Option<u8>,
    creating: bool,
}

struct App {
    base: PathBuf,
    maps: Vec<Pyramid>,
    cur: usize,
    /// The current zone's floor maps, lowest first; empty for most zones.
    floors: Vec<Pyramid>,
    /// Which floor is shown, or None for the top-level map.
    floor: Option<usize>,
    /// The current zone's painted map, if one has been made, and whether it
    /// is the one on screen. An experiment; see gen::paint.
    painted: Option<Pyramid>,
    /// The painted version of each floor, where one has been made.
    floors_painted: Vec<Option<Pyramid>>,
    /// The Lamplight versions of the zone's and each floor's painted map.
    lamplit: Option<Pyramid>,
    floors_lamplit: Vec<Option<Pyramid>>,
    show_painted: bool,
    paint_job: Option<gen::job::PaintJob>,
    paint_note: String,
    mset: MarkerSet,
    graph: connections::Graph,
    watcher: watch::ZoneWatcher,
    follow: bool,
    show_legend: bool,
    /// world units -> screen px
    scale: f32,
    /// screen position of world origin
    offset: egui::Vec2,
    /// Clockwise rotation of the map on screen, radians. Applied after scale,
    /// about the world origin, so `offset` still places the origin.
    rot: f32,
    /// A middle-drag or compass drag is turning the map.
    rotating: bool,
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
    /// Share window: open state, the paste box, and the last result message.
    share_open: bool,
    share_input: String,
    share_note: String,
    /// Marker kinds hidden from the map and list. A zone carrying a community
    /// pack can hold hundreds; without this the map becomes unreadable.
    hidden_kinds: std::collections::HashSet<String>,
    /// The zone's place names, from the binary. Read-only by design.
    places: Vec<places::Place>,
    show_places: bool,
    /// Show markers you placed / markers that came from a pack.
    show_mine: bool,
    show_imported: bool,
    /// Live filter over marker labels and notes.
    marker_find: String,
    /// Live filter over the zone dropdown.
    zone_find: String,
    /// A world position to bring into view on the next frame, once the
    /// viewport size is known.
    pending_center: Option<(f64, f64)>,
    /// A pack waiting for confirmation, with what it would do per zone.
    /// Importing is a bulk edit of hand-made data; showing the effect first
    /// is cheaper than undoing it afterwards.
    pending_import: Option<PendingImport>,
    zone_list: Vec<gen::job::ZoneEntry>,
    zone_picked: std::collections::HashSet<String>,
    zone_filter: String,
    cursor_world: Option<(f64, f64)>,
    title_shown: String,
    confirm_clear: bool,
    /// Checking for and installing new releases; see update.rs.
    updater: update::Updater,
    /// The launch check has been started.
    update_checked: bool,
    /// The banner was closed for this session.
    update_hidden: bool,
    /// The keyboard and mouse reference, opened with ? or F1.
    help_open: bool,
    /// The ring of marker kinds a right-click opens, at this world position.
    /// Placing a marker mid-fight should be two clicks, not a form.
    radial: Option<(f64, f64)>,
    /// Markers in OTHER zones matching the find box, and the query they were
    /// found for. Recomputed only when the query changes: it reads every
    /// zone's file.
    elsewhere: (String, Vec<(usize, markers::Marker)>),
    /// Subscribed packs being fetched, and the URL box in the Share window.
    refresh: Option<subscribe::Refresh>,
    sub_url: String,
    /// Overlay mode, and within it whether the player is playing (clicks go
    /// through to the game) rather than using the map. See overlay.rs.
    overlay: bool,
    playing: bool,
    hotkey: Option<overlay::Hotkey>,
    /// Frames drawn, for `dev_shot`.
    frames: u32,
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
        let settings = gen::job::Settings::load();
        // Prefer wherever the game says you are; otherwise resume the zone
        // that was open last.
        let cur = watcher
            .zone
            .as_deref()
            .and_then(|z| watch::match_zone(z, &names))
            .or_else(|| {
                let want = settings.last_zone.to_lowercase();
                (!want.is_empty())
                    .then(|| maps.iter().position(|m| m.name.to_lowercase() == want))
                    .flatten()
            })
            .unwrap_or(0);
        let rot = maps.get(cur)
            .and_then(|p| settings.rotations.get(&p.name))
            .map_or(0.0, |d| d.to_radians());
        let mset = maps
            .get(cur)
            .map(|p| MarkerSet::load(&p.markers_path(&base)))
            .unwrap_or_else(|| MarkerSet::empty(base.join("markers/none.json")));
        let floors = maps.get(cur).map(|p| pyramid::floors_of(&p.path)).unwrap_or_default();
        let places = maps.get(cur).map(|p| places::for_zone(&p.slug())).unwrap_or_default();
        let floors_painted = floors.iter().map(|f| pyramid::painted_of(&f.path)).collect();
        let floors_lamplit = floors.iter().map(|f| pyramid::lamplight_of(&f.path)).collect();
        let lamplit = maps.get(cur).and_then(|p| pyramid::lamplight_of(&p.path));
        let painted = maps.get(cur).and_then(|p| pyramid::painted_of(&p.path));
        Self {
            base, maps, cur, floors, floor: None, mset, graph, watcher,
            floors_painted, floors_lamplit, lamplit,
            painted, show_painted: false, paint_job: None, paint_note: String::new(),
            follow: true, show_legend: true,
            scale: 1.0, offset: egui::Vec2::ZERO, rot, rotating: false,
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
            share_open: false,
            share_input: String::new(),
            share_note: String::new(),
            hidden_kinds: Default::default(),
            places,
            show_places: true,
            show_mine: true,
            show_imported: true,
            marker_find: String::new(),
            zone_find: String::new(),
            pending_center: None,
            pending_import: None,
            zone_list: Vec::new(),
            zone_picked: Default::default(),
            zone_filter: String::new(),
            updater: update::Updater::new(),
            update_checked: false,
            update_hidden: false,
            help_open: false,
            radial: None,
            elsewhere: (String::new(), Vec::new()),
            refresh: None,
            sub_url: String::new(),
            overlay: false,
            playing: false,
            hotkey: None,
            frames: 0,
        }
    }

    /// Reload the map list while or after maps are generated, without
    /// restarting. `built` names zones whose maps were just (re)written.
    ///
    /// The zone on screen stays on screen, found again by name since new maps
    /// shift the list; its view is kept unless its own map was rebuilt. With
    /// nothing on screen yet -- a first run -- the first zone built opens.
    fn rescan(&mut self, built: &[String]) {
        let slug_of = |p: &Pyramid| p.path.file_stem().unwrap_or_default().to_string_lossy().to_lowercase();
        let rebuilt = |p: &Pyramid| built.iter().any(|b| gen::zones::slug(b) == slug_of(p));
        let was = self.pyr().map(|p| p.name.clone());
        self.maps = discover_all();
        let here = was.as_ref().and_then(|n| self.maps.iter().position(|m| &m.name == n));
        if let Some(i) = here {
            self.cur = i;
            if !rebuilt(&self.maps[i]) { return }
        }
        let i = here
            .or_else(|| self.maps.iter().position(|m| rebuilt(m)))
            .unwrap_or(0);
        if i < self.maps.len() {
            // Not the current index, so open_zone reloads everything.
            self.cur = usize::MAX;
            self.fitted = false;
            self.open_zone(i);
        } else {
            self.cur = 0;
        }
    }

    /// Take finished maps from a running generation, and its result once it
    /// ends. Every frame, not only while a progress panel is open: a first run
    /// swaps that panel for the map as soon as one zone is ready.
    fn poll_gen(&mut self, ctx: &egui::Context) {
        let Some(job) = &mut self.gen_job else { return };
        job.poll();
        let built = std::mem::take(&mut job.built);
        let done = job.done;
        let summary = match &job.last {
            Some(gen::job::Stage::Finished { zones, failed, .. }) if failed.is_empty() =>
                format!("maps done: {zones} zones"),
            Some(gen::job::Stage::Finished { zones, failed, .. }) =>
                format!("maps done: {zones} zones, {} failed (see Maps\u{2026})", failed.len()),
            Some(gen::job::Stage::Failed(e)) => format!("generation failed: {e}"),
            _ => String::new(),
        };
        if !built.is_empty() { self.rescan(&built) }
        if done {
            self.gen_job = None;
            self.rescan(&[]);
            self.refresh_zone_list();
            if !summary.is_empty() { self.status = summary }
        } else {
            ctx.request_repaint_after(std::time::Duration::from_millis(200));
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

    /// Share window: paste a code in, or move whole packs in and out.
    fn share_window(&mut self, ctx: &egui::Context) {
        if !self.share_open {
            return;
        }
        // The window is on egui's dark theme, not parchment.
        let dim = egui::Color32::from_gray(150);
        let mut open = self.share_open;
        egui::Window::new("Share markers")
            .open(&mut open)
            .default_width(460.0)
            .collapsible(false)
            .show(ctx, |ui| {
                ui.label(egui::RichText::new("Paste a marker").strong());
                ui.label(egui::RichText::new(
                    "Someone sends you a line starting mnm1| -- paste it here.")
                    .small().color(dim));
                ui.add(egui::TextEdit::multiline(&mut self.share_input)
                    .desired_rows(2).desired_width(f32::INFINITY)
                    .hint_text("mnm1|underdocks|camp|-2500.0|2000.0|Griffon camp"));
                ui.horizontal(|ui| {
                    if ui.add_enabled(!self.share_input.trim().is_empty(),
                        egui::Button::new("Add to map")).clicked()
                    {
                        self.share_note = self.accept_code();
                    }
                    if ui.button("Clear").clicked() {
                        self.share_input.clear();
                        self.share_note.clear();
                    }
                });

                ui.separator();
                ui.label(egui::RichText::new("Packs").strong());
                ui.label(egui::RichText::new(
                    "A pack is a file of many markers. Importing skips anything \
                     you already have within 12 units.").small().color(dim));
                ui.horizontal(|ui| {
                    if ui.button("Export all\u{2026}").clicked() {
                        if let Some(p) = rfd::FileDialog::new()
                            .set_file_name("markers-pack.json")
                            .add_filter("marker pack", &["json"])
                            .save_file()
                        {
                            self.share_note = self.export_pack(&p, None);
                        }
                    }
                    let zone = self.pyr().map(|p| p.name.clone());
                    if ui.add_enabled(zone.is_some(),
                        egui::Button::new("Export this zone\u{2026}")).clicked()
                    {
                        if let Some(p) = rfd::FileDialog::new()
                            .set_file_name("zone-markers.json")
                            .add_filter("marker pack", &["json"])
                            .save_file()
                        {
                            self.share_note = self.export_pack(&p, self.cur_slug());
                        }
                    }
                    if ui.button("Import\u{2026}").clicked() {
                        if let Some(p) = rfd::FileDialog::new()
                            .add_filter("marker pack", &["json"])
                            .pick_file()
                        {
                            self.share_note = self.preview_pack(&p);
                        }
                    }
                });

                ui.separator();
                ui.label(egui::RichText::new("Subscriptions").strong());
                ui.label(egui::RichText::new(
                    "Follow a pack someone publishes by its link. It is checked again \
                     every time the app starts, adding only markers you do not have.")
                    .small().color(dim));
                let busy = self.refresh.is_some();
                ui.horizontal(|ui| {
                    ui.add(egui::TextEdit::singleline(&mut self.sub_url)
                        .desired_width(300.0).hint_text("https://\u{2026}/pack.json"));
                    let url = subscribe::normalise(&self.sub_url);
                    let dup = self.settings.subscriptions.iter().any(|s| s.url == url);
                    let b = ui.add_enabled(!busy && !url.is_empty() && !dup, egui::Button::new("Subscribe"));
                    let b = if dup { b.on_disabled_hover_text("already subscribed") } else { b };
                    if b.clicked() {
                        self.share_note = "Fetching\u{2026}".into();
                        self.refresh = Some(subscribe::Refresh::start(vec![url], ui.ctx()));
                    }
                });
                let mut drop: Option<(usize, bool)> = None;
                for (i, sub) in self.settings.subscriptions.iter().enumerate() {
                    ui.horizontal(|ui| {
                        ui.label(egui::RichText::new(&sub.name));
                        ui.label(egui::RichText::new(&sub.last).small().color(dim));
                        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                            ui.menu_button("\u{2026}", |ui| {
                                ui.label(egui::RichText::new(&sub.url).small());
                                if ui.button("Unsubscribe, keep its markers").clicked() {
                                    drop = Some((i, false)); ui.close_menu();
                                }
                                if ui.button("Unsubscribe and remove its markers").clicked() {
                                    drop = Some((i, true)); ui.close_menu();
                                }
                            });
                        });
                    });
                }
                if !self.settings.subscriptions.is_empty() {
                    if ui.add_enabled(!busy, egui::Button::new(if busy { "Checking\u{2026}" } else { "Check all now" }))
                        .clicked()
                    {
                        let urls = self.settings.subscriptions.iter().map(|s| s.url.clone()).collect();
                        self.refresh = Some(subscribe::Refresh::start(urls, ui.ctx()));
                    }
                }
                if let Some((i, purge)) = drop {
                    let sub = self.settings.subscriptions.remove(i);
                    self.settings.save();
                    self.share_note = if purge {
                        let n = self.remove_from_source(&sub.name);
                        format!("Unsubscribed from {}; removed its {n} marker(s).", sub.name)
                    } else {
                        format!("Unsubscribed from {}; its markers stay.", sub.name)
                    };
                }

                if !self.share_note.is_empty() {
                    ui.add_space(6.0);
                    ui.label(egui::RichText::new(&self.share_note));
                }
                let mut apply = false;
                let mut cancel = false;
                if let Some(p) = &self.pending_import {
                    ui.separator();
                    ui.label(egui::RichText::new(format!("Pack: {}", p.label))
                        .strong());
                    if !p.pack.author.is_empty() {
                        ui.label(egui::RichText::new(format!("by {}", p.pack.author))
                            .small().color(dim));
                    }
                    egui::ScrollArea::vertical().max_height(160.0)
                        .id_salt("imp").show(ui, |ui| {
                        for (slug, add, had) in &p.effect {
                            if *add == 0 && *had == 0 { continue }
                            ui.label(egui::RichText::new(format!(
                                "{slug:<22} +{add}   ({had} already there)"))
                                .size(11.0)
                                .color(if *add > 0 { INK } else { dim }));
                        }
                    });
                    ui.horizontal(|ui| {
                        if ui.button("Import these").clicked() { apply = true }
                        if ui.button("Cancel").clicked() { cancel = true }
                    });
                }
                if apply { self.share_note = self.apply_import(); }
                if cancel { self.pending_import = None; self.share_note.clear(); }
            });
        self.share_open = open;
    }

    /// Whether a marker passes the current filters.
    ///
    /// Shared by the map and the list deliberately: two separate conditions
    /// drift, and a marker visible in one but not the other is baffling.
    fn visible(&self, m: &markers::Marker) -> bool {
        // A floor shows what is on it; the top level shows the whole zone.
        if let Some(n) = self.floor_number() {
            if m.floor != Some(n) { return false }
        }
        self.passes_filters(m)
    }

    /// The kind, source and text filters alone: what the search of other
    /// zones applies, where the floor on screen means nothing.
    fn passes_filters(&self, m: &markers::Marker) -> bool {
        if self.hidden_kinds.contains(&m.kind) {
            return false;
        }
        let imported = !m.src.is_empty();
        if imported && !self.show_imported {
            return false;
        }
        if !imported && !self.show_mine {
            return false;
        }
        let q = self.marker_find.trim().to_lowercase();
        if !q.is_empty() {
            let hay = format!("{} {} {}", m.label, m.note, m.reqs.summary()).to_lowercase();
            if !hay.contains(&q) {
                return false;
            }
        }
        true
    }

    /// Slug of the zone on screen, as marker files are named.
    fn cur_slug(&self) -> Option<String> {
        self.pyr().map(|p| {
            p.path.file_stem().unwrap_or_default().to_string_lossy().to_lowercase()
        })
    }

    fn accept_code(&mut self) -> String {
        let text = self.share_input.clone();
        let d = match share::decode(&text) {
            Ok(d) => d,
            Err(e) => return format!("Could not read that: {e}"),
        };
        // Switch to the zone it belongs to, so the marker is visible at once.
        let target = self.maps.iter().position(|m| {
            m.path.file_stem().unwrap_or_default().to_string_lossy().to_lowercase()
                == d.zone_slug
        });
        let Some(target) = target else {
            return format!("That marker is for \"{}\", which you have no map for yet.",
                           d.zone_slug);
        };
        if target != self.cur {
            self.open_zone(target);
        }
        let stamp = markers::now_stamp();
        self.mset.checkpoint();
        let r = share::merge_from(
            &mut self.mset.items, std::slice::from_ref(&d.marker), &stamp, "shared code");
        let _ = self.mset.save();
        self.share_input.clear();
        if r.added == 0 {
            "You already have a marker of that kind there.".to_string()
        } else {
            let (x, z) = (d.marker.x, d.marker.z);
            format!("Added \"{}\" at {:.0}, {:.0}.",
                    if d.marker.label.is_empty() { "marker" } else { &d.marker.label }, x, z)
        }
    }

    fn export_pack(&self, path: &std::path::Path, only: Option<String>) -> String {
        let mut pack = share::Pack {
            format: 1, name: String::new(), author: String::new(), note: String::new(),
            created: markers::now_stamp(), zones: Default::default(),
        };
        let dir = self.base.join("markers");
        if let Ok(rd) = std::fs::read_dir(&dir) {
            for e in rd.flatten() {
                let p = e.path();
                if p.extension().and_then(|s| s.to_str()) != Some("json") { continue }
                let slug = p.file_stem().unwrap_or_default().to_string_lossy().to_lowercase();
                if only.as_ref().is_some_and(|o| *o != slug) { continue }
                let set = MarkerSet::load(&p);
                if !set.items.is_empty() { pack.zones.insert(slug.to_string(), set.items); }
            }
        }
        match pack.write(path) {
            Ok(()) => format!("Exported {} markers from {} zone(s).",
                              pack.count(), pack.zones.len()),
            Err(e) => format!("Could not write that file: {e}"),
        }
    }

    /// Work out what a pack would do, without writing anything.
    fn preview_pack(&mut self, path: &std::path::Path) -> String {
        let pack = match share::Pack::read(path) {
            Ok(p) => p,
            Err(e) => return format!("Could not read that pack: {e}"),
        };
        let label = if pack.name.is_empty() {
            path.file_stem().unwrap_or_default().to_string_lossy().to_string()
        } else {
            pack.name.clone()
        };
        let stamp = markers::now_stamp();
        let mut effect = Vec::new();
        for (slug, items) in &pack.zones {
            // Merge into a throwaway copy purely to count.
            let p = self.base.join("markers").join(format!("{slug}.json"));
            let mut items_now = MarkerSet::load(&p).items;
            let r = share::merge_from(&mut items_now, items, &stamp, &label);
            effect.push((slug.clone(), r.added, r.duplicates));
        }
        effect.sort_by(|a, b| b.1.cmp(&a.1));
        let total: usize = effect.iter().map(|e| e.1).sum();
        self.pending_import = Some(PendingImport { label, pack, effect });
        format!("{total} marker(s) would be added \u{2014} review below.")
    }

    /// Actually write the previewed pack.
    fn apply_import(&mut self) -> String {
        let Some(p) = self.pending_import.take() else { return String::new() };
        let (added, dup) = self.merge_pack(&p.pack, &p.label);
        format!("Imported {added} marker(s); {dup} were already on your map.")
    }

    /// Merge a pack into every zone's markers, tagging new ones with `label`.
    /// Returns (added, already there).
    fn merge_pack(&mut self, pack: &share::Pack, label: &str) -> (usize, usize) {
        let stamp = markers::now_stamp();
        let (mut added, mut dup) = (0usize, 0usize);
        for (slug, items) in &pack.zones {
            let path = self.base.join("markers").join(format!("{slug}.json"));
            let mut set = MarkerSet::load(&path);
            let r = share::merge_from(&mut set.items, items, &stamp, label);
            if r.added > 0 { let _ = set.save(); }
            added += r.added; dup += r.duplicates;
        }
        self.reload_markers();
        (added, dup)
    }

    /// Re-read the open zone's markers after something else wrote them.
    fn reload_markers(&mut self) {
        if let Some(m) = self.maps.get(self.cur) {
            self.mset = MarkerSet::load(&m.markers_path(&self.base));
        }
        self.elsewhere.0.clear();
    }

    /// Remove every marker that came from `src`, in every zone.
    fn remove_from_source(&mut self, src: &str) -> usize {
        let mut n = 0;
        for e in std::fs::read_dir(self.base.join("markers")).into_iter().flatten().flatten() {
            let p = e.path();
            if p.extension().and_then(|s| s.to_str()) != Some("json") { continue }
            let mut set = MarkerSet::load(&p);
            let before = set.items.len();
            set.items.retain(|m| m.src != src);
            if set.items.len() != before {
                n += before - set.items.len();
                let _ = set.save();
            }
        }
        self.reload_markers();
        n
    }

    /// Merge subscribed packs as their fetches come back. A URL not yet in
    /// the list is a new subscription, added only once it proves to be a pack.
    fn poll_subscriptions(&mut self) {
        let Some(r) = &mut self.refresh else { return };
        let got = r.take();
        if r.left == 0 { self.refresh = None }
        for (url, res) in got {
            let known = self.settings.subscriptions.iter().position(|s| s.url == url);
            match (res, known) {
                (Ok(pack), Some(i)) => {
                    let name = self.settings.subscriptions[i].name.clone();
                    let (added, _) = self.merge_pack(&pack, &name);
                    self.settings.subscriptions[i].last = format!(
                        "{} \u{2014} {added} new", markers::now_stamp());
                    if added > 0 { self.status = format!("{added} new marker(s) from {name}") }
                }
                (Ok(pack), None) => {
                    let mut name = if pack.name.trim().is_empty() {
                        subscribe::fallback_name(&url)
                    } else {
                        pack.name.trim().to_string()
                    };
                    // Names tag markers, so two packs must not share one.
                    let base = name.clone();
                    let mut k = 2;
                    while self.settings.subscriptions.iter().any(|s| s.name == name) {
                        name = format!("{base} ({k})");
                        k += 1;
                    }
                    let (added, dup) = self.merge_pack(&pack, &name);
                    self.share_note = format!(
                        "Subscribed to {name}: {added} marker(s) added, {dup} already there.");
                    self.settings.subscriptions.push(subscribe::Subscription {
                        url, name, last: format!("{} \u{2014} {added} new", markers::now_stamp()),
                    });
                    self.sub_url.clear();
                }
                (Err(e), Some(i)) => {
                    self.settings.subscriptions[i].last = format!("could not fetch: {e}");
                }
                (Err(e), None) => self.share_note = format!("Could not subscribe: {e}"),
            }
            self.settings.save();
        }
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
                            // Floors, for a zone built on top of itself. Each
                            // costs about as much as the zone, so they can be
                            // left out one by one.
                            let plan = gen::floors::plan(&z.name);
                            if !plan.is_empty() {
                                ui.indent(("floors", &z.name), |ui| {
                                    ui.horizontal_wrapped(|ui| {
                                        ui.label(egui::RichText::new("floors").small().weak());
                                        for (k, f) in plan.iter().enumerate() {
                                            let n = k + 1;
                                            let off = self.settings.floors_off
                                                .get(&z.name).is_some_and(|v| v.contains(&n));
                                            let mut want = !off;
                                            if ui.checkbox(&mut want, egui::RichText::new(
                                                format!("{n} {}", f.name)).small()).changed()
                                            {
                                                let v = self.settings.floors_off
                                                    .entry(z.name.clone()).or_default();
                                                if want { v.retain(|x| *x != n) } else { v.push(n) }
                                                if v.is_empty() { self.settings.floors_off.remove(&z.name); }
                                                self.settings.save();
                                            }
                                        }
                                    });
                                });
                            }
                        }
                    });
                }

                ui.separator();
                self.build_heat_note(ui, egui::Color32::from_gray(150));
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
                    let with_floors = self.zone_picked.iter()
                        .filter(|z| !gen::floors::plan(z).is_empty()).count();
                    if ui.add_enabled(found.is_some() && !busy && with_floors > 0,
                        egui::Button::new(format!("Floors only ({with_floors})")))
                        .on_hover_text("Rebuild just the ticked floors of the selected zones, \
                                        keeping their top-level maps")
                        .clicked()
                    {
                        if let Some(install) = found.clone() {
                            let out = gen::job::maps_dir();
                            self.gen_job = Some(gen::job::Job::start_with(
                                install, out, Some(self.zone_picked.clone()), true, true));
                        }
                    }
                    let live = self.watcher.zone.clone();
                    if ui.add_enabled(found.is_some() && !busy && live.is_some(),
                        egui::Button::new("Just my current zone"))
                        .on_hover_text("Build only the zone the game says you are in")
                        .clicked()
                    {
                        if let (Some(install), Some(z)) = (found.clone(), live) {
                            let out = gen::job::maps_dir();
                            std::fs::create_dir_all(&out).ok();
                            // Match loosely: the log name and the scene name
                            // are not always spelled the same.
                            let want = gen::zones::slug(&z);
                            let pick: std::collections::HashSet<String> = self.zone_list.iter()
                                .filter(|e| gen::zones::slug(&e.name) == want)
                                .map(|e| e.name.clone()).collect();
                            self.gen_job = Some(gen::job::Job::start(
                                install, out, Some(pick), true));
                        }
                    }
                    if ui.add_enabled(found.is_some() && !busy,
                        egui::Button::new("Build missing")).clicked()
                    {
                        if let Some(install) = found.clone() {
                            let out = gen::job::maps_dir();
                            std::fs::create_dir_all(&out).ok();
                            self.gen_job = Some(gen::job::Job::start_missing_first(
                                install, out, self.watcher.zone.clone()));
                        }
                    }
                });
                self.progress_ui(ui);
            });
        self.maps_open = open;
    }

    /// What building does to the CPU, and the setting that tempers it. Shown
    /// wherever a build can be started, so nobody is surprised by the fans.
    fn build_heat_note(&mut self, ui: &mut egui::Ui, dim: egui::Color32) {
        ui.label(egui::RichText::new(
            "Building maps works every CPU core hard for a few minutes: expect the \
             fans to spin up and the CPU to run hot -- 90\u{b0}C or more at Full \
             speed, which modern CPUs are built for. Balanced runs cooler; if your \
             PC is prone to overheating, choose Cool.").small().color(dim));
        ui.label(egui::RichText::new(
            "Building while the game is running may make the game or your PC \
             unstable: both need a lot of memory and CPU. Close the game first if \
             you can.").small().color(egui::Color32::from_rgb(0xd0, 0x8a, 0x30)));
        ui.horizontal(|ui| {
            ui.label(egui::RichText::new("CPU while building").color(dim));
            let cur = self.settings.build_cpu();
            let name = gen::throttle::LEVELS.iter().find(|l| l.1 == cur)
                .map_or_else(|| format!("{cur}%"), |l| l.0.to_string());
            egui::ComboBox::from_id_salt("cpu").selected_text(name).show_ui(ui, |ui| {
                for (label, pct, about) in gen::throttle::LEVELS {
                    if ui.selectable_label(cur == *pct, *label).on_hover_text(*about).clicked() {
                        self.settings.build_cpu = Some(*pct);
                        self.settings.save();
                        // Takes effect at once, even mid-build.
                        gen::throttle::set_limit(*pct);
                    }
                }
            }).response.on_hover_text(
                "Every setting uses every core. Below full speed they rest in short, \
                 regular pauses: cooler, and slower in proportion.");
        });
    }

    /// Progress, estimate and cancel. Shared by the first-run panel and the
    /// maps window so they cannot drift apart.
    fn progress_ui(&mut self, ui: &mut egui::Ui) {
        let dim = egui::Color32::from_rgb(0x6a, 0x5f, 0x50);
        if let Some(job) = &self.gen_job {
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
                }
                Some(gen::job::Stage::Failed(e)) => {
                    ui.colored_label(egui::Color32::from_rgb(0xa0, 0x1b, 0x22), e);
                }
                Some(gen::job::Stage::Built(_)) | None => {
                    ui.label(egui::RichText::new("Starting...").color(INK));
                }
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
            ui.scope(|ui| {
                ui.set_max_width(460.0);
                self.build_heat_note(ui, dim);
            });
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
                        self.gen_job = Some(gen::job::Job::start_missing_first(
                            install, out, self.watcher.zone.clone()));
                    }
                }
            });

            if let Some(z) = &self.watcher.zone {
                ui.add_space(6.0);
                ui.label(egui::RichText::new(format!(
                    "{z} is built first, since the game says you are there. It opens \
                     as soon as it is ready; the rest carry on behind it.")).small().color(dim));
            }
            if !self.gen_note.is_empty() {
                ui.add_space(8.0);
                ui.colored_label(egui::Color32::from_rgb(0xa0, 0x1b, 0x22), &self.gen_note);
            }

            self.progress_ui(ui);
        });
    }



    fn pyr(&self) -> Option<&Pyramid> { self.maps.get(self.cur) }

    /// The pyramid whose tiles are on screen: the chosen floor, or the zone's
    /// top-level map. Floors share the zone's frame exactly, so everything
    /// else -- extent, zoom levels, markers -- still comes from `pyr`.
    fn view_pyr(&self) -> Option<&Pyramid> {
        if self.show_painted {
            if let Some(p) = self.painted_here() { return Some(p) }
        }
        match self.floor {
            Some(i) => self.floors.get(i),
            None => self.pyr(),
        }
    }

    /// The painted version of what is on screen -- the zone, or the floor
    /// shown -- if one has been made.
    fn painted_here(&self) -> Option<&Pyramid> {
        if self.settings.lamplight {
            if let Some(p) = self.lamplit_here() { return Some(p) }
        }
        match self.floor {
            Some(i) => self.floors_painted.get(i).and_then(|p| p.as_ref()),
            None => self.painted.as_ref(),
        }
    }

    /// The Lamplight version of what is on screen, if one has been made.
    fn lamplit_here(&self) -> Option<&Pyramid> {
        match self.floor {
            Some(i) => self.floors_lamplit.get(i).and_then(|p| p.as_ref()),
            None => self.lamplit.as_ref(),
        }
    }

    /// Start painting what is on screen -- the zone, or the floor shown --
    /// in the background.
    fn start_paint(&mut self) {
        let Some(zone) = self.pyr().map(|p| p.name.clone()) else { return };
        let (p, floor) = match self.floor {
            Some(i) => match self.floors.get(i) {
                Some(f) => (f, f.floor.as_ref().map(|(n, _)| n.saturating_sub(1))),
                None => return,
            },
            None => match self.pyr() { Some(p) => (p, None), None => return },
        };
        let Some(install) = self.settings.resolve() else {
            self.paint_note = "Game install not found \u{2014} set it in Maps\u{2026}".into();
            return;
        };
        let frame = gen::job::PaintFrame {
            extent: p.extent,
            base_ppu: p.levels.first().map_or(1.0, |l| l.ppu),
            zooms: p.levels.len(),
            floor,
        };
        let out = pyramid::painted_path(&p.path);
        let name = zone;
        // A rebuild replaces the file; let go of the old one first.
        match self.floor {
            Some(i) => {
                if let Some(slot) = self.floors_painted.get_mut(i) { *slot = None }
                if let Some(slot) = self.floors_lamplit.get_mut(i) { *slot = None }
            }
            None => { self.painted = None; self.lamplit = None }
        }
        self.show_painted = false;
        self.textures.clear();
        self.paint_note.clear();
        self.paint_job = Some(gen::job::PaintJob::start(install, name, out, frame));
    }

    /// Pick up a finished paint job.
    fn poll_paint(&mut self, ctx: &egui::Context) {
        let Some(job) = &self.paint_job else { return };
        ctx.request_repaint_after(std::time::Duration::from_millis(250));
        let Some(res) = job.poll() else { return };
        let zone = job.zone.clone();
        let job_floor = job.floor;
        let secs = job.started.elapsed().as_secs_f32();
        self.paint_job = None;
        match res {
            Ok(path) => {
                // Only show it if the zone is still the one open.
                if self.pyr().is_some_and(|p| p.name == zone) {
                    let made = Pyramid::open(&path).ok();
                    let lamp = Pyramid::open(&gen::tiles::lamplight_path(&path)).ok();
                    match job_floor {
                        Some(fi) => {
                            let at = self.floors.iter()
                                .position(|f| f.floor.as_ref().is_some_and(|(n, _)| n.saturating_sub(1) == fi));
                            if let Some(i) = at {
                                if let Some(slot) = self.floors_painted.get_mut(i) { *slot = made }
                                if let Some(slot) = self.floors_lamplit.get_mut(i) { *slot = lamp }
                            }
                        }
                        None => { self.painted = made; self.lamplit = lamp }
                    }
                    self.show_painted = self.painted_here().is_some();
                    self.textures.clear();
                }
                self.paint_note = format!("painted {} in {secs:.0}s", self.graph.pretty(&zone));
            }
            Err(e) => self.paint_note = format!("painting failed: {e}"),
        }
    }

    fn set_floor(&mut self, floor: Option<usize>) {
        if floor == self.floor { return }
        self.floor = floor;
        // Tiles are cached by position only, and every floor has the same
        // positions.
        self.textures.clear();
    }

    fn open_zone(&mut self, i: usize) {
        if i >= self.maps.len() || i == self.cur && self.fitted { return }
        self.cur = i;
        if self.settings.last_zone != self.maps[i].name {
            self.settings.last_zone = self.maps[i].name.clone();
            self.settings.save();
        }
        self.textures.clear();
        self.fitted = false;
        self.floors = pyramid::floors_of(&self.maps[i].path);
        self.floor = None;
        self.painted = pyramid::painted_of(&self.maps[i].path);
        self.floors_painted = self.floors.iter().map(|f| pyramid::painted_of(&f.path)).collect();
        self.lamplit = pyramid::lamplight_of(&self.maps[i].path);
        self.floors_lamplit = self.floors.iter().map(|f| pyramid::lamplight_of(&f.path)).collect();
        self.show_painted = false;
        self.rot = self.settings.rotations.get(&self.maps[i].name)
            .map_or(0.0, |d| d.to_radians());
        let p = &self.maps[self.cur];
        self.mset = MarkerSet::load(&p.markers_path(&self.base));
        self.places = places::for_zone(&p.slug());
        self.radial = None;
        self.elsewhere.0.clear();
    }

    /// Turn a scene-space vector by the view rotation.
    fn rotv(&self, v: egui::Vec2) -> egui::Vec2 {
        let (s, c) = self.rot.sin_cos();
        egui::vec2(v.x * c - v.y * s, v.x * s + v.y * c)
    }
    fn unrotv(&self, v: egui::Vec2) -> egui::Vec2 {
        let (s, c) = self.rot.sin_cos();
        egui::vec2(v.x * c + v.y * s, -v.x * s + v.y * c)
    }
    fn scene_to_screen(&self, s: egui::Pos2) -> egui::Pos2 {
        self.offset.to_pos2() + self.rotv(s.to_vec2() * self.scale)
    }
    fn world_to_screen(&self, wx: f64, wz: f64) -> egui::Pos2 {
        self.scene_to_screen(world_to_scene(wx, wz))
    }
    fn screen_to_world(&self, p: egui::Pos2) -> (f64, f64) {
        scene_to_world((self.unrotv(p - self.offset.to_pos2()) / self.scale).to_pos2())
    }

    /// Put a world position at the centre of the view, keeping the zoom.
    fn center_on(&mut self, wx: f64, wz: f64, vp: egui::Rect) {
        let s = world_to_scene(wx, wz);
        self.offset = vp.center().to_vec2() - self.rotv(s.to_vec2() * self.scale);
        self.fitted = true;
        self.clamp(vp);
    }

    /// Size of the map's bounding box on screen at scale 1. Turned, a
    /// rectangle needs more room than its own width and height.
    fn turned_size(&self, r: egui::Rect) -> egui::Vec2 {
        let (s, c) = (self.rot.sin().abs(), self.rot.cos().abs());
        egui::vec2(r.width() * c + r.height() * s, r.width() * s + r.height() * c)
    }

    fn fit_scale(&self, vp: egui::Rect) -> f32 {
        let Some(p) = self.pyr() else { return 1.0 };
        let sz = self.turned_size(p.scene_rect());
        (vp.width() / sz.x).min(vp.height() / sz.y)
    }

    fn fit(&mut self, vp: egui::Rect) {
        let Some(p) = self.pyr() else { return };
        let r = p.scene_rect();
        self.scale = self.fit_scale(vp);
        self.offset = vp.center().to_vec2() - self.rotv(r.center().to_vec2() * self.scale);
        self.fitted = true;
    }

    /// Turn the map by `da` radians about a screen point, which stays put.
    /// The scale never changes: turning is turning, not zooming.
    fn rotate_about(&mut self, da: f32, pivot: egui::Pos2, vp: egui::Rect) {
        let before = self.screen_to_world(pivot);
        self.rot = wrap_angle(self.rot + da);
        let after = self.world_to_screen(before.0, before.1);
        self.offset += pivot - after;
        self.clamp(vp);
    }

    /// How far out the wheel may zoom: whichever is further of fitting the
    /// map north-up and fitting it as turned now. Taking the north-up fit too
    /// means turning a fitted map never leaves the view below the floor.
    fn zoom_floor(&self, vp: egui::Rect) -> f32 {
        self.north_fit(vp).min(self.fit_scale(vp))
    }

    /// The scale that fits the map north-up, whatever the rotation.
    fn north_fit(&self, vp: egui::Rect) -> f32 {
        let Some(p) = self.pyr() else { return 1.0 };
        let r = p.scene_rect();
        (vp.width() / r.width()).min(vp.height() / r.height())
    }

    /// Remember this zone's rotation for next time.
    fn save_rotation(&mut self) {
        let Some(name) = self.pyr().map(|p| p.name.clone()) else { return };
        let deg = (self.rot.to_degrees() * 10.0).round() / 10.0;
        if deg == 0.0 { self.settings.rotations.remove(&name); }
        else { self.settings.rotations.insert(name, deg); }
        self.settings.save();
    }

    /// Keep the map covering the viewport; centre it on an axis that fits.
    /// Keep the sheet in view. Once it is smaller than the viewport it is
    /// centred rather than pinned to an edge. Turned, the map's bounding box
    /// is what is kept in view.
    fn clamp(&mut self, vp: egui::Rect) {
        let Some(p) = self.pyr() else { return };
        let r = p.scene_rect();
        let map = egui::Rect::from_points(
            &[r.left_top(), r.right_top(), r.right_bottom(), r.left_bottom()]
                .map(|c| self.scene_to_screen(c)));
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
        let bytes = self.view_pyr()?.tile_bytes(z, tx, ty)?;
        let img = image::load_from_memory(&bytes).ok()?.to_rgba8();
        let (w, h) = img.dimensions();
        let ci = egui::ColorImage::from_rgba_unmultiplied([w as usize, h as usize], img.as_raw());
        let t = ctx.load_texture(format!("t{z}_{tx}_{ty}"), ci, egui::TextureOptions::LINEAR);
        self.textures.insert(key, t.clone());
        Some(t)
    }
}

/// Mouse turning: radians per pixel of sideways drag, about 0.3 degrees, so
/// a full turn is a long sweep across a wide window.
const ROT_PER_PX: f32 = 0.3 * std::f32::consts::PI / 180.0;

/// Angle into (-PI, PI].
fn wrap_angle(a: f32) -> f32 {
    use std::f32::consts::{PI, TAU};
    let a = a.rem_euclid(TAU);
    if a > PI { a - TAU } else { a }
}


impl eframe::App for App {
    /// See-through behind the panels. Normally they cover the window and
    /// this never shows; an overlay paints the map translucent over it.
    fn clear_color(&self, _visuals: &egui::Visuals) -> [f32; 4] {
        [0.0; 4]
    }

    fn update(&mut self, ctx: &egui::Context, _f: &mut eframe::Frame) {
        if cfg!(debug_assertions) { self.dev_shot(ctx) }
        // Generation can run for minutes; put its progress in the title so a
        // minimised or background window still says how far along it is.
        let want_title = match self.gen_job.as_ref().filter(|j| !j.done) {
            Some(j) => {
                let (frac, _) = j.progress();
                match &j.last {
                    Some(gen::job::Stage::Zone { name, .. }) =>
                        format!("M&M Cartographer {VERSION} \u{2014} {:.0}% {name}",
                                frac * 100.0),
                    _ => format!("M&M Cartographer {VERSION} \u{2014} starting\u{2026}"),
                }
            }
            None => format!("M&M Cartographer {VERSION}"),
        };
        if want_title != self.title_shown {
            self.title_shown = want_title.clone();
            ctx.send_viewport_cmd(egui::ViewportCommand::Title(want_title));
        }

        if self.overlay && self.hotkey.as_ref().is_some_and(|h| h.pressed()) {
            self.set_playing(ctx, !self.playing);
        }
        if self.overlay && self.playing {
            // Playing: the map alone, if it shows at all, and nothing that
            // takes input. The game's zone is still followed.
            self.follow_game(ctx);
            self.poll_gen(ctx);
            if self.map_alpha() > 0.0 {
                self.map(ctx);
            } else {
                egui::CentralPanel::default().frame(egui::Frame::none()).show(ctx, |_| {});
            }
            return;
        }

        // Drawn first so it floats above the map and the side panel.
        self.maps_window(ctx);
        self.share_window(ctx);
        self.help_window(ctx);
        self.follow_game(ctx);

        // Window title follows the zone, so a taskbar entry is identifiable.
        if let Some(p) = self.pyr() {
            let want = format!("{} - M&M Cartographer", self.graph.pretty(&p.name));
            if want != self.title_shown {
                ctx.send_viewport_cmd(egui::ViewportCommand::Title(want.clone()));
                self.title_shown = want;
            }
        }

        let mut undo = false;
        ctx.input(|i| {
            if i.key_pressed(egui::Key::Escape) {
                self.editing = None;
                self.help_open = false;
                self.radial = None;
            }
            if i.key_pressed(egui::Key::F) || i.key_pressed(egui::Key::Home) {
                self.fitted = false;
            }
            // Only when no text field has focus, or Ctrl+Z in a note would
            // undo the whole edit instead of a few characters.
            if i.modifiers.command && i.key_pressed(egui::Key::Z) {
                undo = true;
            }
        });
        // Not while typing: a note can hold a question mark.
        if !ctx.wants_keyboard_input()
            && ctx.input(|i| i.key_pressed(egui::Key::Questionmark) || i.key_pressed(egui::Key::F1))
        {
            self.help_open = !self.help_open;
        }
        if undo && self.editing.is_none() && self.mset.undo() {
            self.status = "undid the last marker edit".into();
        }

        if !ctx.wants_keyboard_input() && ctx.input(|i| i.key_pressed(egui::Key::O)) {
            self.set_overlay(ctx, !self.overlay);
        }
        self.poll_paint(ctx);
        self.poll_gen(ctx);
        if !self.update_checked {
            self.update_checked = true;
            if !self.settings.updates_off { self.updater.check(ctx, true) }
            if !self.settings.subscriptions.is_empty() {
                let urls = self.settings.subscriptions.iter().map(|s| s.url.clone()).collect();
                self.refresh = Some(subscribe::Refresh::start(urls, ctx));
            }
        }
        self.poll_subscriptions();
        if self.overlay {
            self.overlay_bar(ctx);
        } else {
            // Before the side panel, so the banner spans the whole window.
            self.update_banner(ctx);
            self.sidebar(ctx);
        }
        self.map(ctx);
        if self.overlay { self.resize_grip(ctx) }
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
                            self.mset.checkpoint();
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

/// Every key and mouse action, for the help window. Kept beside the code
/// rather than only in the README so the app cannot forget one it gains.
const SHORTCUTS: &[(&str, &[(&str, &str)])] = &[
    ("Map", &[
        ("drag", "pan"),
        ("wheel", "zoom"),
        ("F / Home", "fit the whole map"),
        ("Q / E", "turn the map 15\u{b0} left / right"),
        ("middle-drag", "turn the map freely"),
        ("N, or click the compass", "north up"),
        ("PgUp / PgDn", "step through floors"),
        ("shift + right-click", "copy a location to paste in chat"),
    ]),
    ("Markers", &[
        ("right-click the map", "pick a marker kind to drop there"),
        ("left-click a marker", "open its note, or follow its link"),
        ("right-click a marker", "edit or delete"),
        ("drag a marker", "move it"),
        ("Ctrl+Z", "undo the last marker edit"),
    ]),
    ("Window", &[
        ("? / F1", "this list"),
        ("Esc", "close a dialog"),
        ("O", "overlay: the map on top of the game, borderless"),
        ("Ctrl+Shift+M", "in an overlay, from the game: switch between playing and using the map (changeable)"),
    ]),
];

impl App {
    /// Debug builds only: `MNM_SHOT=out.png` puts the app in the states
    /// listed in `MNM_SHOT_STATE` (comma-separated: help, ring, overlay,
    /// playing, find=<text>, share, maps, zone=<name>, painted, lamp, zoom=<factor>,
    /// at=<x>:<z>), saves a screenshot of its own window and
    /// exits. For checking the UI without a person at the screen.
    fn dev_shot(&mut self, ctx: &egui::Context) {
        let Ok(out) = std::env::var("MNM_SHOT") else { return };
        self.frames += 1;
        if self.frames == 3 {
            for st in std::env::var("MNM_SHOT_STATE").unwrap_or_default().split(',') {
                match st.split_once('=').unwrap_or((st, "")) {
                    ("help", _) => self.help_open = true,
                    ("share", _) => self.share_open = true,
                    ("maps", _) => { self.maps_open = true; self.refresh_zone_list() }
                    ("zone", z) => if let Some(i) = self.maps.iter()
                        .position(|m| m.name.eq_ignore_ascii_case(z)) { self.open_zone(i) },
                    ("lamp", _) => self.settings.lamplight = true,
                    ("painted", _) => if self.painted_here().is_some() {
                        self.show_painted = true;
                        self.textures.clear();
                    },
                    ("overlay", _) => self.set_overlay(ctx, true),
                    ("playing", _) => self.set_playing(ctx, true),
                    ("find", q) => self.marker_find = q.to_string(),
                    ("ring", _) => if let Some(p) = self.pyr() {
                        let e = p.extent;
                        self.radial = Some(((e[0] + e[1]) / 2.0, (e[2] + e[3]) / 2.0));
                    },
                    _ => {}
                }
            }
        }
        // After the zone above has opened and fitted, or the fit undoes these.
        if self.frames == 10 {
            for st in std::env::var("MNM_SHOT_STATE").unwrap_or_default().split(',') {
                match st.split_once('=').unwrap_or((st, "")) {
                    ("zoom", f) => if let Ok(f) = f.parse::<f32>() { self.scale *= f },
                    ("at", xz) => if let Some((x, z)) = xz.split_once(':')
                        .and_then(|(x, z)| Some((x.parse().ok()?, z.parse().ok()?))) {
                        self.pending_center = Some((x, z));
                    },
                    _ => {}
                }
            }
        }
        if self.frames == 40 { ctx.send_viewport_cmd(egui::ViewportCommand::Screenshot) }
        let shot = ctx.input(|i| i.raw.events.iter().find_map(|e| match e {
            egui::Event::Screenshot { image, .. } => Some(image.clone()),
            _ => None,
        }));
        if let Some(img) = shot {
            let rgba: Vec<u8> = img.pixels.iter().flat_map(|c| c.to_array()).collect();
            image::RgbaImage::from_raw(img.size[0] as u32, img.size[1] as u32, rgba)
                .expect("screenshot size").save(&out).expect("write screenshot");
            std::process::exit(0);
        }
        ctx.request_repaint();
    }

    /// Switch zones with the game, every couple of seconds.
    fn follow_game(&mut self, ctx: &egui::Context) {
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
    }

    /// How opaque the map is drawn: fully, unless this is an overlay.
    fn map_alpha(&self) -> f32 {
        match (self.overlay, self.playing) {
            (false, _) => 1.0,
            (true, false) => self.settings.overlay_opacity.unwrap_or(overlay::DEFAULT_OPACITY),
            (true, true) => self.settings.overlay_passive.unwrap_or(overlay::DEFAULT_PASSIVE),
        }
    }

    fn hotkey_name(&self) -> String {
        if self.settings.overlay_hotkey.is_empty() { overlay::DEFAULT_HOTKEY.to_string() }
        else { self.settings.overlay_hotkey.clone() }
    }

    fn set_overlay(&mut self, ctx: &egui::Context, on: bool) {
        self.overlay = on;
        self.radial = None;
        ctx.send_viewport_cmd(egui::ViewportCommand::Decorations(!on));
        ctx.send_viewport_cmd(egui::ViewportCommand::WindowLevel(
            if on { egui::WindowLevel::AlwaysOnTop } else { egui::WindowLevel::Normal }));
        let name = self.hotkey_name();
        let hk = self.hotkey.get_or_insert_with(|| overlay::Hotkey::new(ctx));
        hk.set(on.then_some(name.as_str()));
        self.set_playing(ctx, false);
    }

    fn set_playing(&mut self, ctx: &egui::Context, playing: bool) {
        self.playing = playing;
        ctx.send_viewport_cmd(egui::ViewportCommand::MousePassthrough(playing));
    }

    /// The overlay's strip: drag it to move the window, and the controls
    /// that replace the sidebar.
    fn overlay_bar(&mut self, ctx: &egui::Context) {
        let fill = egui::Color32::from_rgba_unmultiplied(0x1d, 0x1a, 0x16, 200);
        egui::TopBottomPanel::top("overlay_bar")
            .frame(egui::Frame::none().fill(fill).inner_margin(egui::Margin::symmetric(8.0, 4.0)))
            .show(ctx, |ui| {
                // Behind the controls, so empty parts of the strip drag the
                // window: it has no title bar to drag by.
                let bg = ui.interact(ui.max_rect(), ui.id().with("drag"), egui::Sense::drag());
                if bg.drag_started() { ctx.send_viewport_cmd(egui::ViewportCommand::StartDrag) }
                ui.horizontal(|ui| {
                    ui.label(egui::RichText::new("\u{2630}").color(PAPER))
                        .on_hover_text("drag the bar to move the window");
                    let zone = self.pyr().map(|p| self.graph.pretty(&p.name)).unwrap_or_default();
                    ui.label(egui::RichText::new(zone).strong().color(PAPER));
                    ui.separator();
                    let mut a = self.map_alpha();
                    ui.label(egui::RichText::new("map").small().color(PAPER));
                    if ui.add(egui::Slider::new(&mut a, 0.2..=1.0).show_value(false))
                        .on_hover_text("opacity").changed()
                    {
                        self.settings.overlay_opacity = Some(a);
                    }
                    let mut p = self.settings.overlay_passive.unwrap_or(overlay::DEFAULT_PASSIVE);
                    ui.label(egui::RichText::new("playing").small().color(PAPER));
                    if ui.add(egui::Slider::new(&mut p, 0.0..=1.0).show_value(false))
                        .on_hover_text("opacity while playing (clicks go to the game); \
                                        all the way left hides the map").changed()
                    {
                        self.settings.overlay_passive = Some(p);
                    }
                    if ui.input(|i| i.pointer.any_released()) { self.settings.save() }
                    let mut key = self.hotkey_name();
                    egui::ComboBox::from_id_salt("hotkey").selected_text(&key).width(110.0)
                        .show_ui(ui, |ui| {
                            for h in overlay::HOTKEYS {
                                ui.selectable_value(&mut key, h.to_string(), *h);
                            }
                        }).response.on_hover_text("press it in the game to switch between \
                                                    playing and using the map");
                    if key != self.hotkey_name() {
                        self.settings.overlay_hotkey = key.clone();
                        self.settings.save();
                        if let Some(h) = &mut self.hotkey { h.set(Some(&key)) }
                    }
                    if ui.button("Play").on_hover_text(format!(
                        "Let clicks through to the game. {key} brings the map back.")).clicked()
                    {
                        self.set_playing(ctx, true);
                    }
                    if ui.button("Exit overlay").on_hover_text("O").clicked() {
                        self.set_overlay(ctx, false);
                    }
                });
                if let Some(e) = self.hotkey.as_ref().map(|h| h.error.clone()).filter(|e| !e.is_empty()) {
                    ui.label(egui::RichText::new(e).small().color(egui::Color32::from_rgb(0xe0, 0x90, 0x70)));
                }
            });
    }

    /// A corner to resize the borderless window by.
    fn resize_grip(&mut self, ctx: &egui::Context) {
        egui::Area::new(egui::Id::new("grip"))
            .anchor(egui::Align2::RIGHT_BOTTOM, [0.0, 0.0])
            .show(ctx, |ui| {
                let (r, resp) = ui.allocate_exact_size(egui::vec2(18.0, 18.0), egui::Sense::drag());
                let c = if resp.hovered() { PAPER } else { egui::Color32::from_gray(140) };
                for k in 1..=3 {
                    let d = k as f32 * 5.0;
                    ui.painter().line_segment(
                        [r.right_bottom() - egui::vec2(d, 2.0), r.right_bottom() - egui::vec2(2.0, d)],
                        egui::Stroke::new(1.5_f32, c));
                }
                if resp.drag_started() {
                    ctx.send_viewport_cmd(egui::ViewportCommand::BeginResize(
                        egui::ResizeDirection::SouthEast));
                }
            });
    }

    fn help_window(&mut self, ctx: &egui::Context) {
        if !self.help_open { return }
        let mut open = true;
        egui::Window::new("Shortcuts")
            .open(&mut open)
            .collapsible(false)
            .resizable(false)
            .show(ctx, |ui| {
                for (section, rows) in SHORTCUTS {
                    ui.label(egui::RichText::new(*section).strong());
                    egui::Grid::new(section).num_columns(2).spacing([18.0, 3.0]).show(ui, |ui| {
                        for (keys, what) in *rows {
                            ui.label(egui::RichText::new(*keys).monospace());
                            ui.label(*what);
                            ui.end_row();
                        }
                    });
                    ui.add_space(6.0);
                }
            });
        if !open { self.help_open = false }
    }

    /// The strip across the top that offers a new release, shows the download
    /// and says what happened. Absent when there is nothing to say.
    fn update_banner(&mut self, ctx: &egui::Context) {
        let state = self.updater.state();
        match &state {
            update::State::Idle | update::State::Checking | update::State::UpToDate => return,
            update::State::Available(r) if r.tag == self.settings.skip_update => return,
            _ if self.update_hidden => return,
            _ => {}
        }
        egui::TopBottomPanel::top("update").show(ctx, |ui| {
            ui.horizontal(|ui| {
                match state {
                    update::State::Available(r) => {
                        ui.label(egui::RichText::new(format!(
                            "M&M Cartographer {} is available (you have {VERSION}).", r.tag)).strong());
                        if r.installable() {
                            if ui.button("Update now").on_hover_text(
                                "Download it, check the maintainer's signature, and replace \
                                 this program. Takes effect when you restart.").clicked()
                            {
                                self.updater.install(r.clone(), ctx);
                            }
                        } else if ui.button("Download\u{2026}").on_hover_text(
                            "This release cannot be installed from here; open its page").clicked()
                        {
                            ctx.open_url(egui::OpenUrl::new_tab(&r.page));
                        }
                        if ui.button("What's new").clicked() {
                            ctx.open_url(egui::OpenUrl::new_tab(&r.page));
                        }
                        if ui.button("Skip this version").clicked() {
                            self.settings.skip_update = r.tag.clone();
                            self.settings.save();
                        }
                    }
                    update::State::Downloading { tag, done, total } => {
                        ui.spinner();
                        let mb = |b: u64| b as f64 / (1024.0 * 1024.0);
                        ui.label(match total {
                            Some(t) => format!("Downloading {tag}\u{2026} {:.1} of {:.1} MB", mb(done), mb(t)),
                            None => format!("Downloading {tag}\u{2026} {:.1} MB", mb(done)),
                        });
                    }
                    update::State::Installed { tag } => {
                        ui.label(egui::RichText::new(format!(
                            "Updated to {tag}. Restart to use it.")).strong());
                        if ui.button("Restart now").clicked() {
                            match self.updater.restart() {
                                Ok(()) => ctx.send_viewport_cmd(egui::ViewportCommand::Close),
                                Err(e) => self.status = format!("restart failed: {e}; start it again yourself"),
                            }
                        }
                    }
                    update::State::Failed { msg, page } => {
                        ui.label(egui::RichText::new(msg).color(egui::Color32::from_rgb(0xc0, 0x50, 0x40)));
                        if let Some(p) = page {
                            if ui.button("Open release page").clicked() {
                                ctx.open_url(egui::OpenUrl::new_tab(&p));
                            }
                        }
                    }
                    _ => {}
                }
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    if ui.small_button("\u{d7}").on_hover_text("Hide until next launch").clicked() {
                        self.update_hidden = true;
                    }
                });
            });
        });
    }

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
                    // 43 zones is too many to scan by eye.
                    let r = ui.add(egui::TextEdit::singleline(&mut self.zone_find)
                        .desired_width(f32::INFINITY).hint_text("type to filter\u{2026}"));
                    r.request_focus();
                    ui.separator();
                    let q = self.zone_find.trim().to_lowercase();
                    let mut order: Vec<usize> = (0..pretty.len()).collect();
                    order.sort_by_key(|&i| pretty[i].to_lowercase());
                    let mut shown = 0;
                    for i in order {
                        if !q.is_empty() && !pretty[i].to_lowercase().contains(&q) {
                            continue;
                        }
                        shown += 1;
                        ui.selectable_value(&mut pick, i, &pretty[i]);
                    }
                    if shown == 0 {
                        ui.label(egui::RichText::new("no zone matches").weak().size(11.0));
                    }
                });
            if pick != self.cur {
                self.open_zone(pick);
                self.zone_find.clear();
            }

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
                if ui.add_enabled(self.mset.can_undo(), egui::Button::new("Undo"))
                    .on_hover_text("Ctrl+Z \u{2014} step back one marker edit").clicked()
                {
                    self.mset.undo();
                    self.status = "undid the last marker edit".into();
                }
            });
            ui.horizontal(|ui| {
                if ui.button("Overlay").on_hover_text(
                    "The map on top of the game, borderless and see-through (O)").clicked() {
                    self.set_overlay(ctx, true);
                }
                if ui.button("Maps\u{2026}").on_hover_text(
                    "Generate or rebuild zone maps from your game install").clicked() {
                    self.maps_open = true;
                    if self.zone_list.is_empty() {
                        self.refresh_zone_list();
                    }
                }
            });
            // Maps from before north was corrected draw their symbols for the
            // old north, so they lie on their sides now. Say why, and offer
            // the fix rather than leave it to be found in the Maps panel.
            let stale = self.maps.iter().filter(|m| !m.upright).count();
            if stale > 0 && self.gen_job.is_none() && self.pyr().is_some_and(|p| !p.upright) {
                ui.label(egui::RichText::new(format!(
                    "North is fixed in this version. {stale} map(s) were built before it, so \
                     palms and tents on them lie sideways.")).size(10.0)
                    .color(egui::Color32::from_rgb(0xd0, 0x8a, 0x30)));
                let found = self.settings.resolve();
                if ui.add_enabled(found.is_some(), egui::Button::new("Rebuild maps").small())
                    .on_hover_text("Rebuild every map, at the CPU setting in Maps\u{2026}. \
                                    Painted maps need Repaint this map as well.")
                    .clicked()
                {
                    if let Some(install) = found {
                        let out = gen::job::maps_dir();
                        self.gen_job = Some(gen::job::Job::start(install, out, None, true));
                    }
                }
            }
            // A first run keeps building after its first map appears; say so
            // here, since the panel that showed progress has made way for it.
            if self.gen_job.is_some() && !self.maps_open && !self.maps.is_empty() {
                self.progress_ui(ui);
                ui.add_space(4.0);
            }
            ui.checkbox(&mut self.follow, "Follow game (tail Player.log)");
            ui.checkbox(&mut self.show_legend, "Legend on map");
            if !self.places.is_empty() {
                ui.checkbox(&mut self.show_places, "Place names on map");
            }
            ui.horizontal(|ui| {
                let mut on = !self.settings.updates_off;
                if ui.checkbox(&mut on, "Check for updates on launch").changed() {
                    self.settings.updates_off = !on;
                    self.settings.save();
                }
                if ui.add_enabled(!self.updater.busy(), egui::Button::new("Now").small())
                    .on_hover_text("Ask GitHub for a newer release").clicked()
                {
                    // An explicit check shows even a skipped release.
                    self.settings.skip_update.clear();
                    self.settings.save();
                    self.update_hidden = false;
                    self.updater.check(ctx, false);
                }
            });
            ui.collapsing("Experimental", |ui| {
                match &self.paint_job {
                    Some(job) => {
                        ui.horizontal(|ui| {
                            ui.spinner();
                            ui.label(egui::RichText::new(format!("{} \u{2014} {}s",
                                job.progress(), job.started.elapsed().as_secs())).size(11.0));
                        });
                    }
                    None => {
                        let what = if self.floor.is_some() { "floor" } else { "map" };
                        let label = if self.painted_here().is_some() { format!("Repaint this {what}") }
                                    else { format!("Paint this {what}") };
                        if ui.add_enabled(self.pyr().is_some(), egui::Button::new(label))
                            .on_hover_text("Render this zone in the painted style from your game \
                                            files. Takes a minute or so; the inked map stays as it is.")
                            .clicked()
                        {
                            self.start_paint();
                        }
                    }
                }
                if self.painted_here().is_some() {
                    let before = self.show_painted;
                    ui.checkbox(&mut self.show_painted, "Show painted style");
                    if before != self.show_painted { self.textures.clear() }
                    if self.show_painted {
                        let has_lamp = self.lamplit_here().is_some();
                        let mut lamp = self.settings.lamplight;
                        ui.horizontal(|ui| {
                            ui.radio_value(&mut lamp, false, "Daylight")
                                .on_hover_text("What everything is made of, as if in full daylight");
                            ui.add_enabled_ui(has_lamp, |ui| {
                                ui.radio_value(&mut lamp, true, "Lamplight")
                                    .on_hover_text("Coloured by the zone's own lamps, torches and glows, \
                                                    as it looks underground or at night. Everything stays visible.")
                                    .on_disabled_hover_text("No Lamplight for this map: either the zone has \
                                                             no lamps of its own to colour it, or it was painted \
                                                             before Lamplight existed -- Repaint to make one.");
                            });
                        });
                        if lamp != self.settings.lamplight {
                            self.settings.lamplight = lamp;
                            self.settings.save();
                            self.textures.clear();
                        }
                    }
                }
                if !self.paint_note.is_empty() {
                    ui.label(egui::RichText::new(&self.paint_note).size(10.0).weak());
                }
            });
            if let Some(p) = &self.watcher.path {
                ui.add(egui::Label::new(
                    egui::RichText::new(format!("log: {}", p.display())).size(9.0).weak(),
                ).wrap());
            } else {
                ui.label(egui::RichText::new("Player.log not found").size(9.0).weak());
            }

            ui.add_space(6.0);
            ui.horizontal(|ui| {
                ui.label(egui::RichText::new(
                    "right-click the map to add a marker\n\
                     left-click a marker to open its note",
                ).size(10.0).weak());
                if ui.small_button("?").on_hover_text("Every shortcut (? or F1)").clicked() {
                    self.help_open = !self.help_open;
                }
            });

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
            ui.horizontal(|ui| {
                ui.heading(format!("Markers ({})", self.mset.items.len()));
                if ui.small_button("Share\u{2026}").on_hover_text(
                    "Paste a marker someone sent you, or export your own").clicked() {
                    self.share_open = true;
                }
            });
            // Filters. A zone carrying a community pack can hold hundreds of
            // markers, and hiding is better than deleting someone else's work.
            ui.horizontal(|ui| {
                ui.add(egui::TextEdit::singleline(&mut self.marker_find)
                    .desired_width(130.0).hint_text("find\u{2026}"));
                if ui.small_button("\u{d7}").on_hover_text("clear filters").clicked() {
                    self.marker_find.clear();
                    self.hidden_kinds.clear();
                    self.show_mine = true;
                    self.show_imported = true;
                }
            });
            ui.horizontal_wrapped(|ui| {
                for ki in markers::KINDS {
                    let mut on = !self.hidden_kinds.contains(ki.key);
                    if ui.add(egui::Checkbox::new(&mut on,
                        egui::RichText::new(ki.label).color(ki.color).size(11.0))).changed()
                    {
                        if on { self.hidden_kinds.remove(ki.key); }
                        else { self.hidden_kinds.insert(ki.key.to_string()); }
                    }
                }
            });
            ui.horizontal(|ui| {
                ui.checkbox(&mut self.show_mine, egui::RichText::new("mine").size(11.0));
                ui.checkbox(&mut self.show_imported,
                    egui::RichText::new("imported").size(11.0));
            });
            self.elsewhere_list(ui);
            let mut open_id = None;
            let mut del_id = None;
            egui::ScrollArea::vertical().id_salt("marks").show(ui, |ui| {
                let mut rows: Vec<_> = self.mset.items.iter()
                    .filter(|m| self.visible(m))
                    .map(|m| (m.id.clone(), m.kind.clone(), m.label.clone(), m.x, m.z,
                              m.src.clone()))
                    .collect();
                rows.sort_by(|a, b| (a.1.clone(), a.2.clone()).cmp(&(b.1.clone(), b.2.clone())));
                for (id, k, label, x, z, src) in rows {
                    let txt = format!("{:<13}{}{}  ({:.0}, {:.0})",
                        kind(&k).label,
                        if label.is_empty() { "\u{2014}" } else { &label },
                        if src.is_empty() { "" } else { " \u{2022}" }, x, z);
                    let r = ui.add(egui::Label::new(
                        egui::RichText::new(txt).color(kind(&k).color).size(11.0),
                    ).sense(egui::Sense::click()));
                    let r = if src.is_empty() { r } else {
                        r.on_hover_text(format!("imported from {src}"))
                    };
                    if r.clicked() { open_id = Some(id.clone()) }
                    r.context_menu(|ui| {
                        if ui.button("Delete marker").clicked() {
                            del_id = Some(id.clone());
                            ui.close_menu();
                        }
                    });
                }
            });
            if let Some(id) = del_id { self.mset.checkpoint(); self.mset.remove(&id) }
            if let Some(id) = open_id {
                // Centre on it too: finding a marker in the list and then
                // hunting for it on the map was the obvious missing half.
                if let Some(m) = self.mset.items.iter().find(|m| m.id == id) {
                    self.pending_center = Some((m.x, m.z));
                }
                self.begin_edit(&id, false);
            }

            if !self.status.is_empty() {
                ui.add_space(4.0);
                ui.label(egui::RichText::new(&self.status).size(10.0).weak());
            }
        });
    }

    /// Matches for the find box in every other zone, so "banker" finds the
    /// banker wherever it is rather than only where you happen to be.
    fn elsewhere_list(&mut self, ui: &mut egui::Ui) {
        let q = self.marker_find.trim().to_lowercase();
        if q.is_empty() { return }
        if self.elsewhere.0 != q {
            let mut hits = Vec::new();
            for (zi, p) in self.maps.iter().enumerate() {
                if zi == self.cur { continue }
                for m in MarkerSet::load(&p.markers_path(&self.base)).items {
                    if self.passes_filters(&m) { hits.push((zi, m)) }
                }
            }
            hits.sort_by(|a, b| (self.graph.pretty(&self.maps[a.0].name), &a.1.label)
                .cmp(&(self.graph.pretty(&self.maps[b.0].name), &b.1.label)));
            self.elsewhere = (q, hits);
        }
        if self.elsewhere.1.is_empty() { return }
        ui.label(egui::RichText::new(format!("In other zones ({})", self.elsewhere.1.len()))
            .size(11.0).strong());
        let mut go: Option<(usize, String, f64, f64)> = None;
        egui::ScrollArea::vertical().id_salt("elsewhere").max_height(120.0).show(ui, |ui| {
            for (zi, m) in &self.elsewhere.1 {
                let ki = kind(&m.kind);
                let txt = format!("{}  \u{2014}  {}",
                    self.graph.pretty(&self.maps[*zi].name),
                    if m.label.is_empty() { ki.label } else { &m.label });
                if ui.add(egui::Label::new(egui::RichText::new(txt).color(ki.color).size(11.0))
                    .sense(egui::Sense::click())).on_hover_text("go there").clicked()
                {
                    go = Some((*zi, m.id.clone(), m.x, m.z));
                }
            }
        });
        ui.separator();
        if let Some((zi, id, x, z)) = go {
            self.open_zone(zi);
            self.pending_center = Some((x, z));
            self.begin_edit(&id, false);
            // Its zone is now this one, so it moves out of this list.
            self.elsewhere.0.clear();
        }
    }

    /// Index of the marker under a screen position, if any.
    fn marker_at(&self, p: egui::Pos2) -> Option<usize> {
        // Only what is drawn can be grabbed: a marker hidden by a filter or
        // on another floor must not be dragged from under the cursor.
        self.mset.items.iter().position(|m| {
            self.visible(m) && (self.world_to_screen(m.x, m.z) - p).length() < icons::RADIUS + 2.0
        })
    }

    /// The number of the floor on screen (1 = lowest), or None at the top
    /// level.
    fn floor_number(&self) -> Option<u8> {
        let i = self.floor?;
        self.floors.get(i).and_then(|p| p.floor.as_ref()).map(|(n, _)| *n as u8)
    }

    /// A marker just placed belongs to the floor it was placed on.
    fn place_on_floor(&mut self, id: &str) {
        let n = self.floor_number();
        if n.is_none() { return }
        if let Some(m) = self.mset.get_mut(id) { m.floor = n }
        let _ = self.mset.save();
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
                floor: m.floor,
                creating,
            });
        }
    }
}

impl App {
    fn map(&mut self, ctx: &egui::Context) {
        let alpha = self.map_alpha();
        let tint = egui::Color32::from_white_alpha((alpha * 255.0) as u8);
        egui::CentralPanel::default()
            .frame(egui::Frame::none().fill(PAPER.gamma_multiply(alpha)))
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
                    tint,
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
                if let Some((wx, wz)) = self.pending_center.take() {
                    self.center_on(wx, wz, vp);
                }

                // ---- input -------------------------------------------------
                // Dragging a marker moves it; dragging anywhere else pans.
                if resp.drag_started_by(egui::PointerButton::Primary) {
                    // Snapshot before the drag, not after: the position to
                    // restore is the one it had when the drag began.
                    let grabbing = resp.interact_pointer_pos()
                        .and_then(|p| self.marker_at(p)).is_some();
                    if grabbing { self.mset.checkpoint() }
                    self.dragging = resp.interact_pointer_pos()
                        .and_then(|p| self.marker_at(p))
                        .map(|i| self.mset.items[i].id.clone());
                }
                if resp.drag_stopped() {
                    if self.dragging.take().is_some() { let _ = self.mset.save(); }
                }
                // Middle-drag turns the map about the window centre, by an
                // even amount per pixel of sideways movement wherever the
                // pointer is.
                if resp.dragged_by(egui::PointerButton::Middle) {
                    let dx = resp.drag_delta().x;
                    if dx != 0.0 {
                        self.rotate_about(dx * ROT_PER_PX, vp.center(), vp);
                        self.rotating = true;
                    }
                }
                if self.rotating && !ctx.input(|i| i.pointer.any_down()) {
                    self.rotating = false;
                    self.save_rotation();
                }
                // Q / E step a quarter of a right angle and land on a multiple
                // of it, so a free-hand angle can be squared up again. N is
                // north up. Not while typing, or a note could not hold a Q.
                if !ctx.wants_keyboard_input() {
                    let step = 15.0f32;
                    let deg = self.rot.to_degrees();
                    let want = ctx.input(|i| {
                        if i.key_pressed(egui::Key::E) { Some(((deg / step + 1e-3).floor() + 1.0) * step) }
                        else if i.key_pressed(egui::Key::Q) { Some(((deg / step - 1e-3).ceil() - 1.0) * step) }
                        else if i.key_pressed(egui::Key::N) { Some(0.0) }
                        else { None }
                    });
                    if let Some(d) = want {
                        self.rotate_about(wrap_angle(d.to_radians()) - self.rot, vp.center(), vp);
                        self.save_rotation();
                    }
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
                        let floor = self.zoom_floor(vp);
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
                    let p = self.view_pyr().unwrap();
                    let l = p.level_for(self.scale as f64);
                    // Turned, the window covers a tilted patch of the world;
                    // ask for everything in that patch's bounding box.
                    let cs = [vp.left_top(), vp.right_top(), vp.right_bottom(), vp.left_bottom()]
                        .map(|c| self.screen_to_world(c));
                    let (mut x0, mut x1, mut z0, mut z1) = (f64::MAX, f64::MIN, f64::MAX, f64::MIN);
                    for (x, z) in cs {
                        x0 = x0.min(x); x1 = x1.max(x); z0 = z0.min(z); z1 = z1.max(z);
                    }
                    let pad = (x1 - x0) * 0.12;
                    (l.z, p.tiles_in(l, x0 - pad, x1 + pad, z0 - pad, z1 + pad))
                };
                let base_z = self.view_pyr().unwrap().levels[0].z;
                // keep the coarsest level resident so panning never shows gaps
                let base_tiles = {
                    let p = self.view_pyr().unwrap();
                    let l = &p.levels[0];
                    p.tiles_in(l, p.extent[0], p.extent[1], p.extent[2], p.extent[3])
                };
                for (z, list) in [(base_z, base_tiles), (lvl_z, want)] {
                    let lv = self.view_pyr().unwrap().levels.iter().find(|l| l.z == z).unwrap().clone();
                    let t = self.view_pyr().unwrap().tile;
                    for (tx, ty) in list {
                        let (wx, wz, ww, hh) = self.view_pyr().unwrap().tile_world_rect(&lv, tx, ty);
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
                        let (ex, ez) = (wx + ww * (vw + bx) / tf, wz - hh * (vh + by) / tf);
                        // A quad rather than an image rect, so it can turn.
                        let corners = [(wx, wz), (ex, wz), (ex, ez), (wx, ez)]
                            .map(|(x, z)| self.world_to_screen(x, z));
                        let (u, v) = ((vw / tf) as f32, (vh / tf) as f32);
                        let uvs = [(0.0, 0.0), (u, 0.0), (u, v), (0.0, v)];
                        if let Some(tex) = self.texture(ctx, z, tx, ty) {
                            let mut mesh = egui::Mesh::with_texture(tex.id());
                            for (pos, (u, v)) in corners.into_iter().zip(uvs) {
                                mesh.vertices.push(egui::epaint::Vertex {
                                    pos, uv: egui::pos2(u, v), color: tint,
                                });
                            }
                            mesh.indices.extend([0, 1, 2, 0, 2, 3]);
                            painter.add(mesh);
                        }
                    }
                    if z == lvl_z { break }
                }

                // ---- markers ------------------------------------------------
                let mut hovered: Option<usize> = None;
                let pointer = resp.hover_pos();
                let show_labels = self.scale >= self.north_fit(vp) * 1.4
                    || self.mset.items.len() <= 6;
                self.cursor_world = pointer.map(|p| self.screen_to_world(p));
                for (i, m) in self.mset.items.iter().enumerate() {
                    if !self.visible(m) { continue }
                    let c = self.world_to_screen(m.x, m.z);
                    if !vp.expand(24.0).contains(c) { continue }
                    let ki = kind(&m.kind);
                    icons::draw(&painter, ki.key, ki.color, c, icons::RADIUS);
                    // Labels overlap badly at low zoom; past a threshold the
                    // glyphs alone carry it and the tooltip gives the rest.
                    if !m.label.is_empty() && show_labels {
                        painter.text(c + egui::vec2(icons::RADIUS + 4.0, -icons::RADIUS), egui::Align2::LEFT_TOP,
                            &m.label, egui::FontId::proportional(11.0),
                            egui::Color32::from_rgb(0x1b, 0x15, 0x10));
                    }
                    if pointer.map_or(false, |p| (p - c).length() < icons::RADIUS + 2.0) { hovered = Some(i) }
                }
                // Place names last, lettered over the badges as a printed map
                // would. A name that would collide with one already lettered
                // is left out until zooming in makes room. Part of the map,
                // so nothing here reacts to the pointer.
                if self.show_places {
                    let floor = self.floor_number();
                    let mut placed: Vec<egui::Rect> = Vec::new();
                    for pl in &self.places {
                        if floor.is_some() && pl.floor != floor { continue }
                        let c = self.world_to_screen(pl.x, pl.z);
                        if !vp.expand(200.0).contains(c) { continue }
                        let ink = painter.layout_no_wrap(pl.name.clone(), map_font(17.0), PLACE_INK);
                        let rect = egui::Align2::CENTER_CENTER.anchor_size(c, ink.size());
                        if placed.iter().any(|r| r.intersects(rect.expand(2.0))) { continue }
                        let halo = painter.layout_no_wrap(pl.name.clone(), map_font(17.0),
                            PAPER.gamma_multiply(0.85));
                        for (dx, dy) in [(-1.2, 0.0), (1.2, 0.0), (0.0, -1.2), (0.0, 1.2),
                                         (-0.9, -0.9), (0.9, -0.9), (-0.9, 0.9), (0.9, 0.9)] {
                            painter.galley(rect.min + egui::vec2(dx, dy), halo.clone(), PAPER);
                        }
                        painter.galley(rect.min, ink, INK);
                        placed.push(rect);
                    }
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
                                let (r, _) = ui.allocate_exact_size(egui::vec2(13.0, 13.0), egui::Sense::hover());
                                icons::draw(ui.painter(), ki.key, ki.color, r.center(), 6.0);
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
                // The marker ring sits above the map, so its clicks are taken
                // before the map's own.
                let ring_took = self.radial_ring(ui, vp);
                if !ring_took && (resp.clicked() || resp.drag_started()) && self.radial.is_some() {
                    // Clicking away dismisses the ring and does nothing else.
                    self.radial = None;
                } else if resp.clicked() && !ring_took {
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
                        // Shift: copy the spot rather than mark it. "Meet me
                        // here" usually wants a message, not a map pin.
                        if ctx.input(|i| i.modifiers.shift) {
                            let (wx, wz) = self.screen_to_world(p);
                            let zone = self.pyr().map(|m| self.graph.pretty(&m.name))
                                .unwrap_or_default();
                            let txt = format!("{zone} ({wx:.0}, {wz:.0})");
                            ctx.output_mut(|o| o.copied_text = txt.clone());
                            self.status = format!("copied \"{txt}\"");
                        } else if let Some(i) = hovered {
                            let id = self.mset.items[i].id.clone();
                            self.begin_edit(&id, false);
                        } else {
                            self.radial = Some(self.screen_to_world(p));
                        }
                    }
                }

                if self.show_legend { self.legend(ui, vp) }
                self.scale_bar(ui, vp);
                self.compass(ui, vp);
                self.floor_table(ui, vp);
            });
    }

    /// The ring of marker kinds around a right-clicked spot. Clicking a kind
    /// places that marker at once; the middle opens the full dialog. Returns
    /// whether it took this frame's click.
    fn radial_ring(&mut self, ui: &mut egui::Ui, vp: egui::Rect) -> bool {
        let Some((wx, wz)) = self.radial else { return false };
        let c = self.world_to_screen(wx, wz);
        if !vp.contains(c) { self.radial = None; return false }
        // Pulled inside the window so a ring opened at the edge is whole.
        const R: f32 = 46.0;
        const ICON: f32 = 13.0;
        let reach = R + ICON + 4.0;
        let c = egui::pos2(
            c.x.clamp(vp.left() + reach, (vp.right() - reach).max(vp.left() + reach)),
            c.y.clamp(vp.top() + reach, (vp.bottom() - reach).max(vp.top() + reach)));
        let painter = ui.painter().clone();
        painter.circle_filled(c, reach, egui::Color32::from_black_alpha(70));
        painter.circle_stroke(self.world_to_screen(wx, wz), 3.0, egui::Stroke::new(2.0_f32, INK));
        let mut took = false;
        let mut place: Option<usize> = None;
        let mut details = false;
        for (k, ki) in KINDS.iter().enumerate() {
            // Clockwise from the top, in the order of the kinds list.
            let a = k as f32 / KINDS.len() as f32 * std::f32::consts::TAU - std::f32::consts::FRAC_PI_2;
            let p = c + R * egui::vec2(a.cos(), a.sin());
            let r = ui.interact(egui::Rect::from_center_size(p, egui::vec2(ICON * 2.4, ICON * 2.4)),
                ui.id().with(("ring", k)), egui::Sense::click());
            let grow = if r.hovered() { 1.25 } else { 1.0 };
            icons::draw(&painter, ki.key, ki.color, p, ICON * grow);
            if r.hovered() {
                egui::show_tooltip_at_pointer(ui.ctx(), ui.layer_id(), egui::Id::new("ring_tip"), |ui| {
                    ui.label(egui::RichText::new(ki.label).strong());
                    ui.label(egui::RichText::new(ki.about).size(11.0));
                });
            }
            if r.clicked() { place = Some(k); took = true }
        }
        let mid = ui.interact(egui::Rect::from_center_size(c, egui::vec2(26.0, 26.0)),
            ui.id().with("ring_mid"), egui::Sense::click());
        painter.circle(c, 12.0, if mid.hovered() { PAPER } else { egui::Color32::from_rgb(0xd8, 0xcc, 0xb0) },
            egui::Stroke::new(1.5_f32, INK));
        painter.text(c, egui::Align2::CENTER_CENTER, "\u{2026}",
            egui::FontId::proportional(14.0), INK);
        if mid.on_hover_text("A marker with a label and note").clicked() { details = true; took = true }
        if let Some(k) = place {
            self.mset.checkpoint();
            let id = self.mset.add(wx, wz, "", KINDS[k].key, "");
            self.place_on_floor(&id);
            self.status = format!("added {} \u{2014} click it to add a note", KINDS[k].label.to_lowercase());
            self.radial = None;
        } else if details {
            // Created provisionally so the dialog edits a real record; removed
            // again if the user cancels.
            let id = self.mset.add(wx, wz, "", KINDS[0].key, "");
            self.place_on_floor(&id);
            self.begin_edit(&id, true);
            self.radial = None;
        }
        took
    }

    /// Rows of the floor table, top to bottom: the top-level map, then the
    /// floors highest first, as a building directory reads.
    fn floor_rows(&self) -> Vec<Option<usize>> {
        std::iter::once(None).chain((0..self.floors.len()).rev().map(Some)).collect()
    }

    /// The floor table, top left, for a zone built on top of itself. Click a
    /// row to show that floor; PageUp / PageDown step through the rows.
    fn floor_table(&mut self, ui: &mut egui::Ui, vp: egui::Rect) {
        if self.floors.is_empty() || self.maps.is_empty() { return }
        let rows = self.floor_rows();
        if !ui.ctx().wants_keyboard_input() {
            let at = rows.iter().position(|r| *r == self.floor).unwrap_or(0);
            let step = ui.ctx().input(|i| {
                if i.key_pressed(egui::Key::PageDown) { 1 }
                else if i.key_pressed(egui::Key::PageUp) { -1 }
                else { 0 }
            });
            let next = (at as i32 + step).clamp(0, rows.len() as i32 - 1) as usize;
            if next != at { self.set_floor(rows[next]) }
        }

        let (row_h, head_h, w) = (22.0, 26.0, 196.0);
        let rect = egui::Rect::from_min_size(
            vp.left_top() + egui::vec2(14.0, 14.0),
            egui::vec2(w, head_h + row_h * rows.len() as f32 + 6.0),
        );
        // Swallow clicks and drags on the panel so they do not reach the map.
        ui.interact(rect, ui.id().with("floors_bg"), egui::Sense::click_and_drag());
        let p = ui.painter().clone();
        p.rect(rect, 5.0, PAPER.gamma_multiply(0.95), egui::Stroke::new(2.0_f32, INK));
        p.text(rect.left_top() + egui::vec2(10.0, 7.0), egui::Align2::LEFT_TOP,
            "Floors", egui::FontId::proportional(12.0), INK);
        p.text(rect.right_top() + egui::vec2(-10.0, 8.0), egui::Align2::RIGHT_TOP,
            "PgUp / PgDn", egui::FontId::proportional(9.5), INK.gamma_multiply(0.55));

        let mut picked = None;
        for (k, row) in rows.iter().enumerate() {
            let r = egui::Rect::from_min_size(
                egui::pos2(rect.left() + 4.0, rect.top() + head_h + row_h * k as f32),
                egui::vec2(w - 8.0, row_h),
            );
            let resp = ui.interact(r, ui.id().with(("floor_row", k)), egui::Sense::click())
                .on_hover_cursor(egui::CursorIcon::PointingHand);
            let on = *row == self.floor;
            if on {
                p.rect_filled(r, 3.0, INK.gamma_multiply(0.85));
            } else if resp.hovered() {
                p.rect_filled(r, 3.0, INK.gamma_multiply(0.12));
            }
            // A rule between the overview and the floors proper.
            if k == 1 {
                p.line_segment([egui::pos2(r.left() + 4.0, r.top()), egui::pos2(r.right() - 4.0, r.top())],
                    egui::Stroke::new(0.75_f32, INK.gamma_multiply(0.4)));
            }
            let col = if on { PAPER } else { INK };
            let (num, name) = match row {
                None => (String::new(), "Top level"),
                Some(i) => match &self.floors[*i].floor {
                    Some((n, name)) => (n.to_string(), name.as_str()),
                    None => (String::new(), ""),
                },
            };
            let font = egui::FontId::proportional(12.0);
            p.text(egui::pos2(r.left() + 14.0, r.center().y), egui::Align2::CENTER_CENTER,
                num, font.clone(), col);
            p.text(egui::pos2(r.left() + 28.0, r.center().y), egui::Align2::LEFT_CENTER,
                name, font, col);
            if resp.clicked() { picked = Some(*row) }
        }
        if let Some(f) = picked { self.set_floor(f) }
    }

    /// Compass rose, top right. It turns with the map so north is always
    /// readable; drag it to turn the map, click it for north up.
    fn compass(&mut self, ui: &mut egui::Ui, vp: egui::Rect) {
        if self.maps.is_empty() { return }
        let r = 26.0;
        let c = egui::pos2(vp.right() - r - 16.0, vp.top() + r + 16.0);
        let rect = egui::Rect::from_center_size(c, egui::vec2(r * 2.0, r * 2.0));
        let resp = ui.interact(rect, ui.id().with("compass"), egui::Sense::click_and_drag())
            .on_hover_text("Drag to turn the map \u{2014} click for north up\nQ / E turn, N north up");
        if resp.dragged() {
            let dx = resp.drag_delta().x;
            if dx != 0.0 {
                self.rotate_about(dx * ROT_PER_PX, vp.center(), vp);
                self.rotating = true;
            }
        }
        if resp.clicked() && self.rot != 0.0 {
            self.rotate_about(-self.rot, vp.center(), vp);
            self.save_rotation();
        }
        if resp.hovered() || resp.dragged() {
            ui.ctx().set_cursor_icon(if resp.dragged() { egui::CursorIcon::Grabbing }
                else { egui::CursorIcon::Grab });
        }

        let p = ui.painter();
        let ink = egui::Stroke::new(1.5_f32, INK);
        p.circle(c, r, PAPER.gamma_multiply(0.95), egui::Stroke::new(2.0_f32, INK));
        p.circle_stroke(c, r - 5.0, egui::Stroke::new(0.75_f32, INK.gamma_multiply(0.6)));
        // Directions on screen of world north and east.
        let n = self.rotv(egui::vec2(0.0, -1.0));
        let e = self.rotv(egui::vec2(1.0, 0.0));
        // Ticks at the eight points, longer on the four cardinals.
        for k in 0..8 {
            let a = self.rot + std::f32::consts::FRAC_PI_4 * k as f32;
            let d = egui::vec2(a.sin(), -a.cos());
            let inner = if k % 2 == 0 { r - 9.0 } else { r - 7.0 };
            p.line_segment([c + d * inner, c + d * (r - 5.0)], ink);
        }
        // The needle: a red north half and an ink south half, each a pair of
        // facets so it reads as a raised point rather than a flat arrow.
        let (tip, tail, w) = (r - 10.0, r - 10.0, 5.0);
        let red = egui::Color32::from_rgb(0xb0, 0x2a, 0x1e);
        let dark_red = egui::Color32::from_rgb(0x7a, 0x1a, 0x12);
        let nt = c + n * tip;
        let st = c - n * tail;
        let (l, rr) = (c - e * w, c + e * w);
        p.add(egui::Shape::convex_polygon(vec![nt, rr, c], red, egui::Stroke::NONE));
        p.add(egui::Shape::convex_polygon(vec![nt, c, l], dark_red, egui::Stroke::NONE));
        p.add(egui::Shape::convex_polygon(vec![st, c, rr], INK, egui::Stroke::NONE));
        p.add(egui::Shape::convex_polygon(vec![st, l, c],
            egui::Color32::from_rgb(0x5a, 0x4c, 0x3c), egui::Stroke::NONE));
        p.add(egui::Shape::closed_line(vec![nt, rr, st, l], egui::Stroke::new(1.0_f32, INK)));
        p.circle_filled(c, 1.8, PAPER);
        // Letters sit outside the ring, upright, in the direction they name.
        let font = egui::FontId::proportional(12.0);
        for (txt, d, col) in [("N", n, red), ("E", e, INK), ("S", -n, INK), ("W", -e, INK)] {
            let at = c + d * (r + 9.0);
            p.text(at + egui::vec2(1.0, 1.0), egui::Align2::CENTER_CENTER, txt, font.clone(),
                PAPER);
            p.text(at, egui::Align2::CENTER_CENTER, txt, font.clone(), col);
        }
        if self.rot != 0.0 {
            let mut deg = self.rot.to_degrees().round() as i32;
            if deg < 0 { deg += 360 }
            p.text(egui::pos2(c.x, c.y + r + 22.0), egui::Align2::CENTER_TOP,
                format!("{deg}\u{b0}"), egui::FontId::proportional(10.0), INK);
        }
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
            // What is on the map, not what is in the file: a kind hidden by a
            // filter, or on another floor, has nothing to explain.
            let n = self.mset.items.iter().filter(|m| m.kind == k.key && self.visible(m)).count();
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
            icons::draw(p, ki.key, ki.color, egui::pos2(rect.left() + 22.0, cy), 8.5);
            p.text(egui::pos2(rect.left() + 40.0, cy), egui::Align2::LEFT_CENTER,
                format!("{}  {n}", ki.label), egui::FontId::proportional(11.0), INK);
        }
    }

    fn marker_window(&mut self, ctx: &egui::Context) {
        // Resolve the travel target before taking a mutable borrow of
        // self.editing -- it needs &self.maps and &self.graph.
        let Some(edit_id) = self.editing.as_ref().map(|e| e.id.clone()) else { return };
        // The zone's floors, to say which one a marker is on.
        let floor_opts: Vec<(Option<u8>, String)> = if self.floors.is_empty() { Vec::new() } else {
            std::iter::once((None, "Whole zone (top level)".to_string()))
                .chain(self.floors.iter().filter_map(|p| p.floor.as_ref())
                    .map(|(n, name)| (Some(*n as u8), format!("{n}  {name}"))))
                .collect()
        };
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
        let mut copy_code = false;
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
                    if !floor_opts.is_empty() {
                        ui.label("Floor");
                        let cur = floor_opts.iter().find(|o| o.0 == ed.floor)
                            .map_or_else(|| "?".to_string(), |o| o.1.clone());
                        egui::ComboBox::from_id_salt("mkfloor").width(280.0).selected_text(cur)
                            .show_ui(ui, |ui| {
                                for (v, txt) in &floor_opts {
                                    ui.selectable_value(&mut ed.floor, *v, txt.as_str());
                                }
                            });
                        ui.end_row();
                    }
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
                    if !ed.creating && ui.button("Copy share code")
                        .on_hover_text("A line you can paste into chat for someone \
                                        else to import").clicked()
                    {
                        copy_code = true;
                    }
                    if let Some(z) = travel {
                        if ui.button(format!("Open {zname}")).clicked() { go = Some(z) }
                    }
                });
            });

        let creating = ed.creating;
        let id = ed.id.clone();

        let (label, kindi, note, link, floor) =
            (ed.label.clone(), ed.kind, ed.note.clone(), ed.link.clone(), ed.floor);
        let mut reqs = ed.reqs.clone();
        // A blank or unparseable level means "no level requirement" rather
        // than zero, which would read as a real gate.
        reqs.level = ed.req_level.trim().parse::<u32>().ok().filter(|l| *l > 0);
        if copy_code {
            // Encode what is on screen rather than what was last saved, so
            // editing a label and sharing in one go does what it looks like.
            let slug = self.cur_slug();
            let found = self.mset.items.iter().find(|m| m.id == id).cloned();
            if let (Some(slug), Some(mut snap)) = (slug, found) {
                snap.label = label.clone();
                snap.note = note.clone();
                snap.kind = KINDS[kindi].key.to_string();
                snap.reqs = reqs.clone();
                ctx.output_mut(|o| o.copied_text = share::encode(&slug, &snap));
                self.status = "share code copied to the clipboard".into();
            }
        }
        if save {
            self.mset.checkpoint();
            if let Some(m) = self.mset.get_mut(&id) {
                m.label = label; m.kind = KINDS[kindi].key.into(); m.note = note;
                m.link = link; m.reqs = reqs; m.floor = floor;
            }
            let _ = self.mset.save();
            self.editing = None;
        } else if delete {
            self.mset.checkpoint();
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
    if let Some(a) = args.iter().find(|a| *a == "--update" || *a == "--check-update") {
        return match update::cli(a == "--update") {
            Ok(()) => Ok(()),
            Err(e) => { eprintln!("update: {e}"); std::process::exit(1) }
        };
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
            "--log" | "--out" | "--export" | "--import" | "--zone" | "--share" | "--cpu"
            | "--debug-markers" => 1,
            // Optional value: only if the next argument is not itself a flag.
            "--generate" => usize::from(args.get(i + 1).is_some_and(|n| !n.starts_with("--"))),
            "--extract" => 2,
            "--dump-tris" | "--dump-all-tris" => 3,
            "--classes" | "--terrains" | "--externals" => 2,
            "--dump-zones" | "--paint" | "--materials" | "--floor" => 1,
            "--render-tris" => 7,
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
        let shared = gen::bundle::Shared::open(std::path::Path::new(bundle).parent().unwrap());
        let f = gen::extract::extract_floors(&env, &shared, &want);
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

    // Inventory of what every zone's walkable surfaces are made of, as
    // <dir>/<slug>.materials.json. See gen::materials.
    if let Some(i) = args.iter().position(|a| a == "--materials") {
        let dir = PathBuf::from(args.get(i + 1).expect("--materials <dir>"));
        std::fs::create_dir_all(&dir).unwrap();
        let install = gen::job::Settings::load().resolve().expect("game install not found");
        let located = gen::zones::survey_cached(&install, |_, _, _| {});
        let no_cancel = std::sync::atomic::AtomicBool::new(false);
        let out = &dir;
        gen::job::run_zones(located, 3, &install, &gen::job::maps_dir(), &no_cancel, |env, shared, l| {
            let t = std::time::Instant::now();
            let idx = env.scene_index();
            let mut files: Vec<usize> = l.group.scenes().iter()
                .filter_map(|p| idx.get(*p).and_then(|c| env.file_index(c))).collect();
            files.sort_unstable();
            files.dedup();
            let inv = gen::materials::inventory(env, shared, &files);
            let slug = gen::zones::slug(&l.zone);
            std::fs::write(out.join(format!("{slug}.materials.json")),
                serde_json::to_string_pretty(&inv).unwrap()).unwrap();
            println!("{:<22} {:>4} materials  {:>3} terrain layers  {:.1}s",
                     l.zone, inv.materials.len(), inv.terrain_layers.len(), t.elapsed().as_secs_f32());
        }, |l, m| eprintln!("{}: {m}", l.zone));
        return Ok(());
    }

    // Paint one zone, as the app's "Paint this map" does, and wait for it.
    if let Some(i) = args.iter().position(|a| a == "--paint") {
        let want = args.get(i + 1).expect("--paint <Zone>").to_lowercase();
        let Some(p) = discover_all().into_iter().find(|p| p.name.to_lowercase() == want) else {
            eprintln!("no map for {want}; generate it first");
            return Ok(());
        };
        let install = gen::job::Settings::load().resolve().expect("game install not found");
        // --floor N paints floor N (1 = lowest) into that floor's frame.
        let floor = args.iter().position(|a| a == "--floor")
            .and_then(|i| args.get(i + 1)).and_then(|v| v.parse::<usize>().ok());
        let target = match floor {
            Some(n) => match pyramid::floors_of(&p.path).into_iter()
                .find(|f| f.floor.as_ref().is_some_and(|(k, _)| *k == n)) {
                Some(f) => f,
                None => { eprintln!("{} has no built floor {n}", p.name); return Ok(()) }
            },
            None => Pyramid::open(&p.path).expect("open map"),
        };
        let out = pyramid::painted_path(&target.path);
        let job = gen::job::PaintJob::start(install, p.name.clone(), out, gen::job::PaintFrame {
            extent: target.extent,
            base_ppu: target.levels.first().map_or(1.0, |l| l.ppu),
            zooms: target.levels.len(),
            floor: floor.map(|n| n - 1),
        });
        let mut last = String::new();
        loop {
            if let Some(r) = job.poll() {
                match r {
                    Ok(path) => println!("wrote {} in {:.1}s", path.display(), job.started.elapsed().as_secs_f32()),
                    Err(e) => eprintln!("failed: {e}"),
                }
                break;
            }
            let p = job.progress();
            if p != last { println!("  {p}"); last = p }
            std::thread::sleep(std::time::Duration::from_millis(200));
        }
        return Ok(());
    }

    // Every zone's walkable triangles, as generation loads them, to
    // <dir>/<slug>.bin (see --dump-tris) with <slug>.json beside it.
    if let Some(i) = args.iter().position(|a| a == "--dump-zones") {
        let dir = PathBuf::from(args.get(i + 1).expect("--dump-zones <dir>"));
        std::fs::create_dir_all(&dir).unwrap();
        let install = gen::job::Settings::load().resolve().expect("game install not found");
        let located = gen::zones::survey_cached(&install, |_, _, _| {});
        let shared = gen::bundle::Shared::open(&install);
        let mut by_bundle: std::collections::BTreeMap<PathBuf, Vec<gen::zones::Located>> =
            Default::default();
        for l in located { by_bundle.entry(l.bundle.clone()).or_default().push(l) }
        for (bundle, group) in by_bundle {
            let env = gen::bundle::Env::open(&bundle).unwrap();
            for l in group {
                let gen::tiles::LoadedZone { tris, sea, props, bbox, bounds, .. } =
                    gen::tiles::load_zone(&env, &shared, &l.group);
                let slug = gen::zones::slug(&l.zone);
                let bytes: Vec<u8> = tris.iter().flatten().flatten()
                    .flat_map(|v| v.to_le_bytes()).collect();
                std::fs::write(dir.join(format!("{slug}.bin")), bytes).unwrap();
                let meta = serde_json::json!({
                    "zone": l.zone, "tris": tris.len(), "sea": sea, "bounds": bounds,
                    "bbox": [bbox.0, bbox.1, bbox.2, bbox.3],
                    "props": props.iter().map(|(k, v)| (k.clone(), v.len()))
                        .collect::<std::collections::BTreeMap<_, _>>(),
                    "scenes": l.group.scenes(),
                });
                let mut files: Vec<usize> = Vec::new();
                let idx = env.scene_index();
                for path in l.group.scenes() {
                    if let Some(fi) = idx.get(path).and_then(|c| env.file_index(c)) { files.push(fi) }
                }
                files.sort_unstable();
                files.dedup();
                if std::env::var("MNM_OBJECTS").is_ok() {
                    let objs = gen::extract::objects(&env, &files);
                    std::fs::write(dir.join(format!("{slug}.objects.json")),
                        serde_json::to_string(&objs).unwrap()).unwrap();
                }
                std::fs::write(dir.join(format!("{slug}.json")),
                    serde_json::to_string_pretty(&meta).unwrap()).unwrap();
                println!("{:<24} {:>8} tris", l.zone, tris.len());
            }
        }
        return Ok(());
    }

    // Every bundle's serialized files (CABs), to find where shared assets live.
    if args.iter().any(|a| a == "--cab-index") {
        let install = gen::job::Settings::load().resolve().expect("game install not found");
        let mut all: Vec<PathBuf> = std::fs::read_dir(&install).unwrap().flatten()
            .map(|e| e.path())
            .filter(|p| p.extension().is_some_and(|x| x == "bundle")).collect();
        all.sort();
        for b in all {
            let t = std::time::Instant::now();
            match gen::bundle::Env::open(&b) {
                Ok(env) => {
                    let name = b.file_name().unwrap().to_string_lossy();
                    for c in env.cabs() { println!("{c}\t{name}") }
                    eprintln!("{name}: {} files, {:.1}s", env.file_count(), t.elapsed().as_secs_f32());
                }
                Err(e) => eprintln!("{}: {e}", b.display()),
            }
        }
        return Ok(());
    }

    // Which external files a scene's mesh colliders point into that its own
    // bundle lacks, and how many colliders each.
    if let Some(i) = args.iter().position(|a| a == "--externals") {
        let bundle = args.get(i + 1).expect("--externals <bundle> <scene substring>");
        let filter = args.get(i + 2).map(|s| s.to_lowercase()).unwrap_or_default();
        let env = gen::bundle::Env::open(std::path::Path::new(bundle)).unwrap();
        for (path, cab) in env.scene_index() {
            if !path.to_lowercase().contains(&filter) { continue }
            let Some(fi) = env.file_index(&cab) else { continue };
            let missing = env.missing_externals(fi);
            let mut counts = std::collections::BTreeMap::<String, usize>::new();
            let n = env.col.serialized_files()[fi].file.objects.len();
            for oi in 0..n {
                if env.class_id(fi, oi) != Some(64) { continue }
                let Some(mc) = env.read(fi, oi) else { continue };
                let Some(p) = gen::bundle::as_pptr(gen::bundle::field(&mc, "m_Mesh")) else { continue };
                if let Some((_, name)) = missing.iter().find(|(k, _)| *k as i64 == p.file) {
                    *counts.entry(name.clone()).or_default() += 1;
                }
            }
            println!("{path}: {} missing externals", missing.len());
            for (k, v) in counts { println!("  {v:6} meshes in {k}") }
        }
        return Ok(());
    }

    // Describe the TerrainData behind a scene's terrain colliders.
    if let Some(i) = args.iter().position(|a| a == "--terrains") {
        let bundle = args.get(i + 1).expect("--terrains <bundle> <scene substring>");
        let filter = args.get(i + 2).map(|s| s.to_lowercase()).unwrap_or_default();
        let env = gen::bundle::Env::open(std::path::Path::new(bundle)).unwrap();
        let mut files: Vec<usize> = env.scene_index().iter()
            .filter(|(p, _)| p.to_lowercase().contains(&filter))
            .filter_map(|(_, c)| env.file_index(c)).collect();
        files.sort_unstable();
        for s in gen::extract::terrain_probe(&env, &files) { println!("{s}") }
        return Ok(());
    }

    // Count the Unity object classes in a zone's scene files.
    if let Some(i) = args.iter().position(|a| a == "--classes") {
        let bundle = args.get(i + 1).expect("--classes <bundle> <scene substring>");
        let filter = args.get(i + 2).map(|s| s.to_lowercase()).unwrap_or_default();
        let env = gen::bundle::Env::open(std::path::Path::new(bundle)).unwrap();
        for (path, cab) in env.scene_index() {
            if !path.to_lowercase().contains(&filter) { continue }
            let Some(fi) = env.file_index(&cab) else { continue };
            let n = env.col.serialized_files()[fi].file.objects.len();
            let mut counts = std::collections::BTreeMap::<i32, usize>::new();
            for oi in 0..n {
                if let Some(c) = env.class_id(fi, oi) { *counts.entry(c).or_default() += 1 }
            }
            println!("{path}: {counts:?}");
        }
        println!("all files:");
        for (fi, sf) in env.col.serialized_files().iter().enumerate() {
            let navs = sf.file.objects.iter().filter(|o| o.class_id == 238).count();
            if navs > 0 { println!("  file {fi}: {navs} NavMeshData") }
        }
        return Ok(());
    }

    // Write a zone's walkable triangles as raw little-endian f32, nine per
    // triangle (x, y, z for each corner), for analysis outside the app.
    if let Some(i) = args.iter().position(|a| a == "--dump-tris" || a == "--dump-all-tris") {
        let min_ny = if args[i] == "--dump-all-tris" { -2.0 } else { 0.5 };
        let bundle = args.get(i + 1).expect("--dump-tris <bundle> <scene substring> <out.bin>");
        let filter = args.get(i + 2).map(|s| s.to_lowercase()).unwrap_or_default();
        let out = args.get(i + 3).cloned().unwrap_or_else(|| "tris.bin".into());
        let env = gen::bundle::Env::open(std::path::Path::new(bundle)).unwrap();
        let mut files: Vec<usize> = env.scene_index().iter()
            .filter(|(path, _)| (filter.is_empty() || path.to_lowercase().contains(&filter))
                && gen::is_geometry_scene(path))
            .filter_map(|(_, cab)| env.file_index(cab))
            .collect();
        files.sort_unstable();
        files.dedup();
        let shared = gen::bundle::Shared::open(std::path::Path::new(bundle).parent().unwrap());
        let f = gen::extract::extract_faces(&env, &shared, &files, min_ny);
        let bytes: Vec<u8> = f.tris.iter().flatten().flatten()
            .flat_map(|v| v.to_le_bytes()).collect();
        std::fs::write(&out, bytes).unwrap();
        println!("wrote {} triangles to {out}", f.tris.len());
        return Ok(());
    }

    // Render triangles written by --dump-tris (or a subset of them) into a
    // fixed world box, so separately rendered subsets line up.
    if let Some(i) = args.iter().position(|a| a == "--render-tris") {
        let a = |k: usize| args.get(i + k).cloned().unwrap_or_default();
        let (src, ppu, out) = (a(1), a(2).parse::<f64>().unwrap_or(2.0), a(3));
        let bb: Vec<f64> = (4..8).map(|k| a(k).parse().unwrap_or(0.0)).collect();
        let raw = std::fs::read(&src).unwrap();
        let fl: Vec<f32> = raw.chunks_exact(4)
            .map(|c| f32::from_le_bytes([c[0], c[1], c[2], c[3]])).collect();
        let tris: Vec<gen::extract::Tri> = fl.chunks_exact(9)
            .map(|c| [[c[0], c[1], c[2]], [c[3], c[4], c[5]], [c[6], c[7], c[8]]]).collect();
        let bbox = (bb[0], bb[1], bb[2], bb[3]);
        let edges = gen::raster::global_band_edges(&tris, 8);
        // MNM_FLOOR: draw as one floor of a multi-storey zone.
        let hgt = if std::env::var("MNM_FLOOR").is_ok() {
            gen::raster::rasterize_floor(&tris, ppu, bbox, 2.5, 20.0)
        } else {
            gen::raster::rasterize(&tris, ppu, bbox)
        };
        let r = gen::ink::render(&hgt, &edges, &gen::ink::InkOptions {
            ppu, step_thresh: 3.2, sea: None, seed: 5,
        });
        let rgb = r.img;
        let mut buf = image::RgbImage::new(rgb.w as u32, rgb.h as u32);
        for (i, p) in rgb.v.iter().enumerate() {
            buf.put_pixel((i % rgb.w) as u32, (i / rgb.w) as u32,
                image::Rgb([(p[0]*255.0) as u8, (p[1]*255.0) as u8, (p[2]*255.0) as u8]));
        }
        buf.save(&out).unwrap();
        println!("wrote {out}: {} tris, {}x{}", tris.len(), rgb.w, rgb.h);
        return Ok(());
    }

    // Paint windows of one zone straight to PNGs, north up as the viewer
    // shows them, for working on the painted style without building a whole
    // pyramid: the zone loads once, then each window renders in seconds.
    //   --paint-preview <Zone> <out-prefix> <ppu>:<x0>:<x1>:<z0>:<z1> ...
    // A window of just <ppu> paints the whole play area.
    if let Some(i) = args.iter().position(|a| a == "--paint-preview") {
        let zone = args.get(i + 1).expect("--paint-preview <Zone> <out-prefix> <ppu>[:x0:x1:z0:z1]...");
        let prefix = args.get(i + 2).expect("--paint-preview <Zone> <out-prefix> ...");
        let wins: Vec<Vec<f64>> = args[i + 3..].iter().take_while(|a| !a.starts_with("--"))
            .map(|a| a.split(':').map(|v| v.parse().expect("number")).collect()).collect();
        let install = gen::job::Settings::load().resolve().expect("game install not found");
        let t0 = std::time::Instant::now();
        let l = gen::zones::survey_cached(&install, |_, _, _| {}).into_iter()
            .find(|l| l.zone.eq_ignore_ascii_case(zone)).expect("no such zone");
        let env = gen::bundle::Env::open(&l.bundle).unwrap();
        let shared = gen::bundle::Shared::open(&install);
        let lz = gen::tiles::load_zone_with(&env, &shared, &l.group, true);
        println!("loaded {} in {:.1}s", l.zone, t0.elapsed().as_secs_f32());
        for (k, w) in wins.iter().enumerate() {
            let t1 = std::time::Instant::now();
            let ppu = w[0];
            let bbox = if w.len() == 5 { (w[1], w[2], w[3], w[4]) } else { lz.bbox };
            let height = gen::raster::rasterize(&lz.tris, ppu, bbox);
            let tags = gen::raster::rasterize_tagged(&lz.tris, &lz.mats, ppu, bbox).1;
            let ground = gen::raster::rasterize_low(&lz.tris, ppu, bbox);
            let mut img = gen::paint::render(&height, &lz.all_props, &gen::paint::PaintOptions {
                ppu, sea: lz.sea, bbox, ground: Some(&ground),
                classes: Some((tags.as_slice(), lz.mat_table.as_slice())),
            });
            // MNM_LAMP=1 shows the lamplight version, exposed as a build
            // exposes it: once, from the whole zone at a coarse resolution.
            if std::env::var("MNM_LAMP").is_ok() && !lz.lights.is_empty() {
                let h0 = gen::raster::rasterize(&lz.tris, 0.25, lz.bbox);
                if let Some(e) = gen::light::exposure(&gen::light::light_map(&lz.lights, &h0, 0.25, lz.bbox), &h0) {
                    gen::light::apply(&mut img, &gen::light::light_map(&lz.lights, &height, ppu, bbox),
                                      &gen::light::coverage(&height, ppu), e);
                }
            }
            // The render's rows run from high world Z down, its columns along
            // world X; the viewer puts north (+X) up and east (-Z) right.
            let (w_, h_) = (img.h as u32, img.w as u32);
            let mut buf = image::RgbImage::new(w_, h_);
            for row in 0..h_ {
                for col in 0..w_ {
                    let p = img.v[col as usize * img.w + (img.w - 1 - row as usize)];
                    buf.put_pixel(col, row, image::Rgb(p.map(|c| (c.clamp(0.0, 1.0) * 255.0) as u8)));
                }
            }
            let out = format!("{prefix}{k}.png");
            buf.save(&out).unwrap();
            println!("wrote {out} {}x{} in {:.1}s", w_, h_, t1.elapsed().as_secs_f32());
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
        let shared = gen::bundle::Shared::open(std::path::Path::new(bundle).parent().unwrap());
        let f = gen::extract::extract_floors(&env, &shared, &files);
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

    // Scatter markers across every zone, for exercising the UI at volume.
    // Not something a release needs, but reviewing marker rendering, the list
    // and import performance all want more than the 45 real ones.
    if let Some(i) = args.iter().position(|a| a == "--debug-markers") {
        let per: usize = args.get(i + 1).and_then(|s| s.parse().ok()).unwrap_or(25);
        let maps = discover_all();
        if maps.is_empty() {
            eprintln!("no maps found; generate some first");
            return Ok(());
        }
        let stamp = markers::now_stamp();
        let kinds: Vec<&str> = markers::KINDS.iter().map(|k| k.key).collect();
        let mut total = 0usize;
        // A fixed sequence, so a debug run is reproducible and a bug seen once
        // can be seen again.
        let mut seed: u64 = 0x5EED_1234_ABCD_0001;
        let mut next = || {
            seed ^= seed << 13;
            seed ^= seed >> 7;
            seed ^= seed << 17;
            seed
        };
        for p in &maps {
            let [ax, bx, az, bz] = p.extent;
            let path = p.markers_path(&base);
            let mut set = MarkerSet::load(&path);
            for n in 0..per {
                let fx = (next() % 10_000) as f64 / 10_000.0;
                let fz = (next() % 10_000) as f64 / 10_000.0;
                let k = kinds[(next() % kinds.len() as u64) as usize];
                set.items.push(markers::Marker {
                    id: markers::new_id(),
                    x: ((ax + (bx - ax) * fx) * 100.0).round() / 100.0,
                    z: ((az + (bz - az) * fz) * 100.0).round() / 100.0,
                    label: format!("debug {n}"),
                    kind: k.to_string(),
                    note: "generated by --debug-markers; remove with --clean-markers".into(),
                    link: String::new(),
                    reqs: Default::default(),
                    src: "debug".into(),
                    added: stamp.clone(),
                    floor: None,
                });
                total += 1;
            }
            let _ = set.save();
        }
        println!("added {total} debug markers across {} zones", maps.len());
        println!("remove them again with --clean-markers");
        return Ok(());
    }

    // Drop every marker the debug scatter made, leaving real ones alone.
    if args.iter().any(|a| a == "--clean-markers") {
        let dir = base.join("markers");
        let (mut removed, mut kept) = (0usize, 0usize);
        if let Ok(rd) = std::fs::read_dir(&dir) {
            for e in rd.flatten() {
                let p = e.path();
                if p.extension().and_then(|s| s.to_str()) != Some("json") { continue }
                let mut set = MarkerSet::load(&p);
                let before = set.items.len();
                set.items.retain(|m| !m.note.contains("--debug-markers"));
                removed += before - set.items.len();
                kept += set.items.len();
                if before != set.items.len() { let _ = set.save(); }
            }
        }
        println!("removed {removed} debug markers, kept {kept} real ones");
        return Ok(());
    }

    // --- sharing -------------------------------------------------------
    // Export every marker, or one zone's, as a pack others can import.
    if let Some(i) = args.iter().position(|a| a == "--export") {
        let out = args.get(i + 1).map(PathBuf::from)
            .unwrap_or_else(|| PathBuf::from("markers-pack.json"));
        let only = args.iter().position(|a| a == "--zone")
            .and_then(|j| args.get(j + 1)).map(|s| s.to_lowercase());
        let mut pack = share::Pack {
            format: 1,
            name: String::new(),
            author: String::new(),
            note: String::new(),
            created: markers::now_stamp(),
            zones: Default::default(),
        };
        let dir = base.join("markers");
        if let Ok(rd) = std::fs::read_dir(&dir) {
            for e in rd.flatten() {
                let p = e.path();
                if p.extension().and_then(|s| s.to_str()) != Some("json") { continue }
                let slug = p.file_stem().unwrap_or_default().to_string_lossy().to_lowercase();
                if only.as_ref().is_some_and(|o| *o != slug) { continue }
                let set = MarkerSet::load(&p);
                if !set.items.is_empty() {
                    pack.zones.insert(slug.to_string(), set.items);
                }
            }
        }
        match pack.write(&out) {
            Ok(()) => println!("wrote {} -- {} markers across {} zones",
                               out.display(), pack.count(), pack.zones.len()),
            Err(e) => eprintln!("could not write {}: {e}", out.display()),
        }
        return Ok(());
    }

    // Merge someone else's pack in, skipping markers already present.
    if let Some(i) = args.iter().position(|a| a == "--import") {
        let Some(src) = args.get(i + 1).map(PathBuf::from) else {
            eprintln!("usage: --import <pack.json>"); return Ok(());
        };
        let pack = match share::Pack::read(&src) {
            Ok(p) => p,
            Err(e) => { eprintln!("{}: {e}", src.display()); return Ok(()) }
        };
        println!("{} -- {} markers across {} zones{}", src.display(), pack.count(),
                 pack.zones.len(),
                 if pack.name.is_empty() { String::new() } else { format!("  ({})", pack.name) });
        let stamp = markers::now_stamp();
        let (mut added, mut dup) = (0usize, 0usize);
        for (slug, items) in &pack.zones {
            let path = base.join("markers").join(format!("{slug}.json"));
            let mut set = MarkerSet::load(&path);
            let label = if pack.name.is_empty() {
                src.file_stem().unwrap_or_default().to_string_lossy().to_string()
            } else { pack.name.clone() };
            let r = share::merge_from(&mut set.items, items, &stamp, &label);
            if r.added > 0 { let _ = set.save(); }
            println!("  {:<22} +{:<4} {} already present", slug, r.added, r.duplicates);
            added += r.added; dup += r.duplicates;
        }
        println!("{added} added, {dup} skipped as duplicates");
        return Ok(());
    }

    // Print a share code for a marker, by id or by label.
    if let Some(i) = args.iter().position(|a| a == "--share") {
        let want = args.get(i + 1).map(|s| s.to_lowercase()).unwrap_or_default();
        let dir = base.join("markers");
        if let Ok(rd) = std::fs::read_dir(&dir) {
            for e in rd.flatten() {
                let p = e.path();
                if p.extension().and_then(|s| s.to_str()) != Some("json") { continue }
                let slug = p.file_stem().unwrap_or_default().to_string_lossy().to_lowercase();
                for m in MarkerSet::load(&p).items {
                    if want.is_empty()
                        || m.id.to_lowercase() == want
                        || m.label.to_lowercase().contains(&want)
                    {
                        println!("{}", share::encode(&slug, &m));
                    }
                }
            }
        }
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
        let cpu = args.iter().position(|a| a == "--cpu")
            .and_then(|i| args.get(i + 1)).and_then(|v| v.parse().ok())
            .unwrap_or(settings.build_cpu());
        gen::throttle::set_limit(cpu);
        println!("cpu      {}%", gen::throttle::limit());
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
        let many = wanted.len() > 1;
        let (mut tiles_n, mut bytes_n, mut zones_n) = (0usize, 0u64, 0usize);
        // Profiling runs one zone at a time so stage lines do not interleave.
        let lanes = if gen::profiling() && many { 1 } else { gen::job::zone_concurrency() };
        println!("building {lanes} zone(s) at a time");
        let force = args.iter().any(|a| a == "--force");
        let floors_only = args.iter().any(|a| a == "--floors-only");
        let todo: Vec<_> = wanted.into_iter().filter(|l| {
            if floors_only && gen::floors::plan(&l.zone).is_empty() { return false }
            let dest = out.join(format!("{}.mbtiles", gen::zones::slug(&l.zone)));
            if dest.exists() && !force {
                println!("  {:<24} already built (--force to rebuild)", l.zone);
                false
            } else { true }
        }).collect();
        let no_cancel = std::sync::atomic::AtomicBool::new(false);
        let res = gen::job::run_zones(todo, lanes, &install, &out, &no_cancel, |env, shared, l| {
            let t1 = std::time::Instant::now();
            let dest = out.join(format!("{}.mbtiles", gen::zones::slug(&l.zone)));
            let gen::tiles::LoadedZone { tris, sea, props, bbox, .. } =
                gen::tiles::load_zone(env, shared, &l.group);
            if tris.is_empty() {
                println!("  {:<24} skipped (no geometry)", l.zone);
                return (0, 0, false);
            }
            match gen::tiles::build_zone(&dest,
                &gen::tiles::ZoneInput { name: &l.zone, tris: &tris, sea, props: &props,
                                         bbox: Some(bbox), tags: None, lights: None },
                &gen::tiles::Settings {
                    floors: !args.iter().any(|a| a == "--no-floors"),
                    floors_only,
                    skip_floors: settings.floors_off.get(&l.zone).cloned().unwrap_or_default(),
                    ..Default::default()
                }, |_, _| {})
            {
                Ok((n, b)) => {
                    println!("  {:<24} {:>7} tris {:>5} tiles {:>6.1} MB  {:.1}s",
                             l.zone, tris.len(), n, b as f64 / 1e6, t1.elapsed().as_secs_f32());
                    (n, b, true)
                }
                Err(e) => { eprintln!("  !! {}: {e}", l.zone); (0, 0, false) }
            }
        }, |l, msg| { eprintln!("  !! {}: {msg}", l.zone); (0, 0, false) });
        for (n, b, ok) in res {
            tiles_n += n; bytes_n += b; if ok { zones_n += 1 }
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
            .with_title(format!("M&M Cartographer {VERSION}"))
            // Needed at creation for an overlay to be see-through later.
            .with_transparent(true),
        ..Default::default()
    };
    eframe::run_native(
        "M&M Cartographer",
        opts,
        Box::new(move |cc| {
            install_fonts(&cc.egui_ctx);
            Ok(Box::new(App::new(base, log)))
        }),
    )
}
