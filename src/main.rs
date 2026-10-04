#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

//! M&M Cartographer -- zone maps for Monsters & Memories.

mod connections;
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
        v.push(d.join("mnm-cartographer"));
    }
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

struct Editing {
    id: String,
    label: String,
    kind: usize,
    note: String,
    link: String,
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
    cursor_world: Option<(f64, f64)>,
    title_shown: String,
    confirm_clear: bool,
}

impl App {
    fn new(base: PathBuf) -> Self {
        let maps = discover(&base);
        let graph = connections::Graph::load(&base);
        let watcher = watch::ZoneWatcher::new(None);
        let names: Vec<String> = maps.iter().map(|m| m.name.clone()).collect();
        let cur = watcher
            .zone
            .as_deref()
            .and_then(|z| watch::match_zone(z, &names))
            .unwrap_or(0);
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
        }
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
                    painter.text(vp.center(), egui::Align2::CENTER_CENTER,
                        "No map data found.\n\nDownload mnm-maps.zip from the releases page\n\
                         and unzip it so a `maps` folder sits next to the application.",
                        egui::FontId::proportional(15.0), PAPER);
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
        if save {
            if let Some(m) = self.mset.get_mut(&id) {
                m.label = label; m.kind = KINDS[kindi].key.into(); m.note = note;
                m.link = link;
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
    let args: Vec<String> = std::env::args().skip(1).collect();
    let check = args.iter().any(|a| a == "--check");
    let base = args.iter().find(|a| !a.starts_with("--")).map(PathBuf::from)
        .or_else(find_base)
        .unwrap_or_else(app_dir);

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
        println!("base          {}", base.display());
        println!("searched      {}", data_dirs().iter()
            .map(|p| p.display().to_string()).collect::<Vec<_>>().join("\n              "));
        let maps = discover(&base);
        println!("zones         {}", maps.len());
        let g = connections::Graph::load(&base);
        println!("wiki edges    {}", g.adj.values().map(|v| v.len()).sum::<usize>() / 2);
        let w = watch::ZoneWatcher::new(None);
        println!("Player.log    {}", w.path.as_ref()
            .map(|p| p.display().to_string()).unwrap_or_else(|| "not found".into()));
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
            .with_title("M&M Cartographer"),
        ..Default::default()
    };
    eframe::run_native(
        "M&M Cartographer",
        opts,
        Box::new(move |_cc| Ok(Box::new(App::new(base)))),
    )
}
