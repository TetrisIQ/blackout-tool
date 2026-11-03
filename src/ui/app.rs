//! The window: Library, Downloads, Updates and Services, and the Wikipedia wizard, laid out as in
//! the design project "App Starter". Drawing only reads state and collects [`Act`]s; the acts are
//! applied after the frame, so no widget code needs `&mut` access to the model.

use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};
use std::sync::mpsc::{Receiver, Sender, channel};
use std::time::{Duration, Instant};

use eframe::egui::{
    self, Align, Align2, Color32, CornerRadius, FontId, Frame, Layout, Margin, Modal, Pos2, Rect, RichText,
    ScrollArea, Sense, Stroke, StrokeKind, Ui, vec2,
};

use super::backend::{self, ArchiveRequest, Config, Event, JobState, Loaded, Runner, ServiceInfo};
use super::model::{self, Status, Workspace, fmt_bytes, language_name};
use super::theme::{
    self, A100, A700, A800, ACCENT, BG, DIVIDER, N100, N200, N500, N600, N700, N800, SURFACE, TEXT,
};

const SIDEBAR: f32 = 232.0;
/// Width of the language button, the filter box and their gaps in the Library toolbar.
const RIGHT_TOOLS: f32 = 420.0;

#[derive(Clone, Copy, PartialEq, Eq)]
enum Tab {
    Library,
    Downloads,
    Installed,
    Updates,
    Services,
}

enum Msg {
    Loaded {
        data: Result<Box<Loaded>, String>,
        live: bool,
    },
    Online(bool),
    Active(HashMap<String, bool>),
    Health(bool, String),
    Restarted(String),
}

struct Wiz {
    step: usize,
    langs: BTreeSet<String>,
    /// Edition index (see [`model::EDITIONS`]) per language.
    ed: BTreeMap<String, usize>,
}

enum Act {
    Tab(Tab),
    Profile(String),
    ToggleLang(String),
    AllLangs,
    NoLangs,
    ToggleCat(String),
    TogglePkg(String),
    OpenWizard,
    Apply,
    KeepInstalled,
    PauseAll,
    ToggleJob(String),
    CancelJob(String),
    Confirm(Option<String>),
    Remove(String),
    CheckUpdates,
    UpdateAll,
    Update(String),
    InstallService(String),
    Restart(String, String),
    Open(String),
    HealthCheck,
    ChangeLocation,
    ArchiveWebsite,
}

pub struct App {
    cfg: Config,
    ws: Option<Workspace>,
    load_error: Option<String>,
    runner: Runner,
    tab: Tab,
    find: String,
    collapsed: HashSet<String>,
    lang_open: bool,
    lang_find: String,
    /// Package whose removal waits for a second click on the Installed page.
    confirm_remove: Option<String>,
    wiz: Option<Wiz>,
    /// The storage-location dialog: typed path, move flag, last error.
    loc: Option<Location>,
    /// The "Archive a website" dialog: the request being typed and the last error.
    archive: Option<ArchiveDialog>,
    done: Vec<(String, bool, Instant)>,
    failures: Vec<String>,
    online: bool,
    checking_updates: bool,
    checked_at: Option<Instant>,
    services: Vec<ServiceInfo>,
    active: HashMap<String, bool>,
    restarting: HashSet<String>,
    health: Option<(bool, String)>,
    checking_health: bool,
    tx: Sender<Msg>,
    rx: Receiver<Msg>,
    snapshot: Option<Snapshot>,
}

struct ArchiveDialog {
    req: ArchiveRequest,
    error: Option<String>,
    /// The user typed the id, so it no longer follows the address.
    id_touched: bool,
}

struct Location {
    text: String,
    move_data: bool,
    error: Option<String>,
}

/// Developer aid: `KIT_GUI_SHOT=file.ppm` saves the window after a few seconds and quits;
/// `KIT_GUI_TAB=downloads|installed|updates|services|wizard|location|archive` picks the page.
struct Snapshot {
    path: std::path::PathBuf,
    frames: u32,
}

impl App {
    pub fn new(cc: &eframe::CreationContext<'_>, cfg: Config) -> Self {
        theme::install(&cc.egui_ctx);
        let (tx, rx) = channel();
        let mut app = Self {
            runner: Runner::new(cfg.clone()),
            cfg,
            ws: None,
            load_error: None,
            tab: Tab::Library,
            find: String::new(),
            collapsed: ["games", "reference"].iter().map(|s| (*s).to_string()).collect(),
            lang_open: false,
            lang_find: String::new(),
            confirm_remove: None,
            wiz: None,
            loc: None,
            archive: None,
            done: Vec::new(),
            failures: Vec::new(),
            online: true,
            checking_updates: false,
            checked_at: None,
            services: Vec::new(),
            active: HashMap::new(),
            restarting: HashSet::new(),
            health: None,
            checking_health: false,
            tx,
            rx,
            snapshot: std::env::var_os("KIT_GUI_SHOT").map(|p| Snapshot {
                path: p.into(),
                frames: 0,
            }),
        };
        // The cache answers at once; the live catalog follows when the network is there.
        app.tab = match std::env::var("KIT_GUI_TAB").as_deref() {
            Ok("downloads") => Tab::Downloads,
            Ok("installed") => Tab::Installed,
            Ok("updates") => Tab::Updates,
            Ok("services") => Tab::Services,
            _ => Tab::Library,
        };
        app.spawn_load(&cc.egui_ctx, true);
        app.spawn_online_probe(&cc.egui_ctx);
        app
    }

    fn spawn_load(&self, ctx: &egui::Context, offline: bool) {
        let (cfg, tx, ctx) = (self.cfg.clone(), self.tx.clone(), ctx.clone());
        std::thread::spawn(move || {
            let data = backend::load(&cfg, offline)
                .map(Box::new)
                .map_err(|e| format!("{e:#}"));
            let _ = tx.send(Msg::Loaded { data, live: !offline });
            ctx.request_repaint();
        });
    }

    fn spawn_online_probe(&self, ctx: &egui::Context) {
        let (tx, ctx) = (self.tx.clone(), ctx.clone());
        std::thread::spawn(move || {
            loop {
                if tx.send(Msg::Online(backend::online())).is_err() {
                    break;
                }
                ctx.request_repaint();
                std::thread::sleep(Duration::from_secs(20));
            }
        });
    }

    fn spawn_active(&self, ctx: &egui::Context) {
        let units: Vec<String> = self.services.iter().filter_map(|s| s.unit.clone()).collect();
        let (tx, ctx) = (self.tx.clone(), ctx.clone());
        std::thread::spawn(move || {
            let map = units
                .into_iter()
                .map(|u| (u.clone(), backend::unit_active(&u)))
                .collect();
            let _ = tx.send(Msg::Active(map));
            ctx.request_repaint();
        });
    }

    /// Re-reads what is installed after a download finished or something was removed.
    fn refresh_installed(&mut self) {
        let state = backend::read_state(&self.cfg.data_dir);
        let (drive, free) = backend::disk(&self.cfg.data_dir);
        self.services = backend::services_of(&state);
        if let Some(ws) = &mut self.ws {
            ws.installed = backend::installed_of(&state);
            (ws.drive_bytes, ws.free_bytes) = (drive, free);
        }
    }

    fn take_loaded(&mut self, l: Loaded, live: bool) {
        let mut ws = Workspace::new(l.pkgs, l.profiles);
        ws.installed = l.installed;
        ws.drive_bytes = l.drive_bytes;
        ws.free_bytes = l.free_bytes;
        self.services = l.services;
        match self.ws.take() {
            // A refresh keeps what the person has picked.
            Some(old) => {
                ws.sel = old.sel.into_iter().filter(|id| ws.pkg(id).is_some()).collect();
                ws.langs = old.langs;
                ws.profile = old.profile;
            }
            None => {
                ws.sel = ws.installed.keys().cloned().collect();
                let mut langs: BTreeSet<String> = ws
                    .installed
                    .keys()
                    .filter_map(|id| ws.pkg(id))
                    .flat_map(|p| p.langs.clone())
                    .collect();
                langs.insert("EN".into());
                if let Some(l) = std::env::var("LANG")
                    .ok()
                    .and_then(|v| v.get(..2).map(str::to_ascii_uppercase))
                {
                    langs.insert(l);
                }
                let known = ws.all_langs();
                ws.langs = langs.into_iter().filter(|l| known.contains(l)).collect();
            }
        }
        if live {
            self.checking_updates = false;
            self.checked_at = Some(Instant::now());
        }
        self.load_error = None;
        self.ws = Some(ws);
    }

    fn drain(&mut self, ctx: &egui::Context) {
        while let Ok(m) = self.rx.try_recv() {
            match m {
                Msg::Loaded { data: Ok(l), live } => {
                    self.take_loaded(*l, live);
                    if !live && self.online {
                        self.spawn_load(ctx, false);
                    }
                    self.spawn_active(ctx);
                }
                Msg::Loaded { data: Err(e), live } => {
                    self.checking_updates = false;
                    if !live || self.ws.is_none() {
                        self.load_error = Some(e);
                    }
                }
                Msg::Online(o) => {
                    let came_back = o && !self.online;
                    self.online = o;
                    if came_back && self.ws.is_some() {
                        self.spawn_load(ctx, false);
                    }
                }
                Msg::Active(a) => self.active = a,
                Msg::Health(ok, text) => {
                    self.checking_health = false;
                    self.health = Some((ok, text));
                }
                Msg::Restarted(unit) => {
                    self.restarting.remove(&unit);
                    self.spawn_active(ctx);
                }
            }
        }
        for ev in self.runner.poll(self.online) {
            match ev {
                Event::Done { name, update, .. } => {
                    self.done.insert(0, (name, update, Instant::now()));
                    self.refresh_installed();
                }
                Event::Failed { id, message } => {
                    let name = self
                        .ws
                        .as_ref()
                        .and_then(|w| w.pkg(&id))
                        .map(|p| p.name.clone())
                        .or_else(|| {
                            self.runner
                                .jobs
                                .iter()
                                .find(|j| j.id == id)
                                .map(|j| j.name.clone())
                        })
                        .unwrap_or(id);
                    self.failures.push(format!("{name}: {message}"));
                }
                Event::Removed(_) => self.refresh_installed(),
                Event::Relocated(new) => {
                    self.cfg.data_dir = new;
                    self.loc = None;
                    self.refresh_installed();
                }
                Event::RelocateFailed(m) => {
                    if let Some(l) = &mut self.loc {
                        l.error = Some(m);
                    }
                    self.refresh_installed();
                }
                Event::RemoveFailed(m) => {
                    self.refresh_installed();
                    self.failures.push(format!("Removing failed: {m}"));
                }
            }
        }
    }

    fn act(&mut self, ctx: &egui::Context, a: Act) {
        let queued: HashSet<String> = self.runner.jobs.iter().map(|j| j.id.clone()).collect();
        match a {
            Act::Tab(t) => self.tab = t,
            Act::ToggleCat(c) => {
                if !self.collapsed.remove(&c) {
                    self.collapsed.insert(c);
                }
            }
            Act::OpenWizard => self.open_wizard(),
            Act::PauseAll => {
                if self.runner.all_paused() {
                    self.runner.resume_all();
                } else {
                    self.runner.pause_all();
                }
            }
            Act::ToggleJob(id) => self.runner.toggle(&id),
            Act::CancelJob(id) => self.runner.cancel(&id),
            Act::Confirm(id) => self.confirm_remove = id,
            Act::ChangeLocation => {
                self.loc = Some(Location {
                    text: String::new(),
                    move_data: self.ws.as_ref().is_some_and(|w| !w.installed.is_empty()),
                    error: None,
                });
            }
            Act::ArchiveWebsite => {
                self.archive = Some(ArchiveDialog {
                    req: ArchiveRequest {
                        license: "personal copy".into(),
                        ..Default::default()
                    },
                    error: None,
                    id_touched: false,
                });
            }
            Act::Remove(id) => {
                self.confirm_remove = None;
                if let Some(ws) = &mut self.ws {
                    ws.sel.remove(&id);
                }
                self.runner.remove(vec![id]);
            }
            Act::CheckUpdates => {
                self.checking_updates = true;
                self.spawn_load(ctx, false);
            }
            Act::Open(url) => backend::open_url(&url),
            Act::Restart(key, unit) => {
                self.restarting.insert(unit.clone());
                let (tx, ctx) = (self.tx.clone(), ctx.clone());
                let _ = key;
                std::thread::spawn(move || {
                    backend::restart_unit(&unit);
                    let _ = tx.send(Msg::Restarted(unit));
                    ctx.request_repaint();
                });
            }
            Act::HealthCheck => {
                self.checking_health = true;
                let (cfg, tx, ctx) = (self.cfg.clone(), self.tx.clone(), ctx.clone());
                std::thread::spawn(move || {
                    let (ok, text) = backend::health_check(&cfg);
                    let _ = tx.send(Msg::Health(ok, text));
                    ctx.request_repaint();
                });
            }
            other => {
                let Some(ws) = &mut self.ws else { return };
                match other {
                    Act::Profile(id) => ws.apply_profile(&id),
                    Act::ToggleLang(l) => {
                        let mut langs = ws.langs.clone();
                        if !langs.remove(&l) {
                            langs.insert(l);
                        }
                        ws.set_langs(langs);
                    }
                    Act::AllLangs => {
                        let all = ws.all_langs().into_iter().collect();
                        ws.set_langs(all);
                    }
                    Act::NoLangs => ws.set_langs(BTreeSet::new()),
                    Act::TogglePkg(id) => ws.toggle(&id),
                    Act::KeepInstalled => {
                        ws.sel = ws.installed.keys().cloned().collect();
                        ws.profile = "custom".into();
                    }
                    Act::Apply => {
                        let diff = ws.diff(&|id| queued.contains(id));
                        let add: Vec<_> = diff.add.iter().filter_map(|id| ws.pkg(id).cloned()).collect();
                        // Dependencies first: an app before the films that play in it.
                        let mut add = add;
                        add.sort_by_key(|p| !p.needs.is_empty());
                        for p in &add {
                            self.runner.enqueue(p, false);
                        }
                        self.runner.remove(diff.remove);
                        if !add.is_empty() {
                            self.tab = Tab::Downloads;
                        }
                    }
                    Act::UpdateAll => {
                        let ups: Vec<_> = ws.updates().into_iter().cloned().collect();
                        for p in &ups {
                            self.runner.enqueue(p, true);
                        }
                        self.tab = Tab::Downloads;
                    }
                    Act::Update(id) => {
                        if let Some(p) = ws.pkg(&id).cloned() {
                            self.runner.enqueue(&p, true);
                            self.tab = Tab::Downloads;
                        }
                    }
                    Act::InstallService(id) => {
                        ws.sel.insert(id.clone());
                        if let Some(p) = ws.pkg(&id).cloned() {
                            self.runner.enqueue(&p, false);
                            self.tab = Tab::Downloads;
                        }
                    }
                    _ => {}
                }
            }
        }
    }

    fn take_snapshot(&mut self, ctx: &egui::Context) {
        let Some(s) = &mut self.snapshot else { return };
        s.frames += 1;
        let frames = s.frames;
        if frames == 100 && std::env::var("KIT_GUI_TAB").as_deref() == Ok("location") {
            self.loc = Some(Location {
                text: "/run/media/you/disk/kit".into(),
                move_data: true,
                error: Some("example error".into()),
            });
        }
        if frames == 100 && std::env::var("KIT_GUI_TAB").as_deref() == Ok("archive") {
            self.archive = Some(ArchiveDialog {
                req: ArchiveRequest {
                    url: "https://projekt-gutenberg.org/bibliothek/".into(),
                    id: "gutenberg-de".into(),
                    name: "Projekt Gutenberg-DE".into(),
                    license: "personal copy".into(),
                    page_limit: "50".into(),
                },
                error: Some("example error".into()),
                id_touched: true,
            });
        }
        if frames == 100 && std::env::var("KIT_GUI_TAB").as_deref() == Ok("wizard") {
            self.open_wizard();
        }
        let Some(s) = &mut self.snapshot else { return };
        if frames != 240 {
            return;
        }
        let (path, c) = (s.path.clone(), ctx.clone());
        ctx.send_viewport_cmd(egui::ViewportCommand::Screenshot(egui::ScreenshotCallback::new(
            move |img| {
                let mut out = format!("P6\n{} {}\n255\n", img.width(), img.height()).into_bytes();
                out.extend(img.pixels.iter().flat_map(|c| [c.r(), c.g(), c.b()]));
                let _ = std::fs::write(path, out);
                c.send_viewport_cmd(egui::ViewportCommand::Close);
                c.request_repaint();
            },
        )));
    }

    fn open_wizard(&mut self) {
        let Some(ws) = &self.ws else { return };
        let wiki = ws.wikipedia();
        let mut langs = BTreeSet::new();
        let mut ed = BTreeMap::new();
        for (l, ids) in &wiki {
            let chosen = ids
                .iter()
                .position(|id| id.as_ref().is_some_and(|id| ws.sel.contains(id)));
            if let Some(i) = chosen {
                langs.insert(l.clone());
                ed.insert(l.clone(), i);
            }
        }
        if langs.is_empty() {
            langs = wiki
                .iter()
                .map(|(l, _)| l.clone())
                .filter(|l| ws.langs.contains(l))
                .collect();
        }
        for (l, ids) in &wiki {
            ed.entry(l.clone()).or_insert_with(|| {
                ids.iter()
                    .position(Option::is_some)
                    .map_or(1, |_| usize::from(ids[1].is_some()))
            });
        }
        self.wiz = Some(Wiz { step: 0, langs, ed });
        self.lang_open = false;
    }

    fn finish_wizard(&mut self) {
        let (Some(w), Some(ws)) = (self.wiz.take(), &mut self.ws) else {
            return;
        };
        ws.sel.retain(|id| !id.starts_with("wikipedia_"));
        for (l, ids) in ws.wikipedia() {
            if w.langs.contains(&l) {
                if let Some(Some(id)) = w.ed.get(&l).and_then(|&e| ids.get(e)) {
                    let id = id.clone();
                    ws.sel.insert(id);
                }
                ws.langs.insert(l);
            }
        }
        ws.profile = "custom".into();
    }
}

impl eframe::App for App {
    fn ui(&mut self, ui: &mut egui::Ui, _frame: &mut eframe::Frame) {
        let ctx = ui.ctx().clone();
        self.drain(&ctx);
        ctx.request_repaint_after(Duration::from_millis(if self.runner.jobs.is_empty() {
            1500
        } else {
            400
        }));
        if ctx.input(|i| i.key_pressed(egui::Key::Escape)) {
            self.wiz = None;
            self.lang_open = false;
            if !self.runner.is_relocating() {
                self.loc = None;
            }
        }
        let mut acts: Vec<Act> = Vec::new();

        self.header(ui, &mut acts);
        self.sidebar(ui, &mut acts);
        if self.tab == Tab::Library && self.ws.is_some() {
            self.footer(ui, &mut acts);
        }
        egui::CentralPanel::default()
            .frame(Frame::new().fill(BG))
            .show(ui, |ui| {
                if self.ws.is_none() {
                    ui.centered_and_justified(|ui| {
                        let msg = self
                            .load_error
                            .clone()
                            .unwrap_or_else(|| "Loading the catalog\u{2026}".into());
                        ui.label(theme::small(msg, N700));
                    });
                    return;
                }
                match self.tab {
                    Tab::Library => self.library(ui, &mut acts),
                    Tab::Downloads => self.downloads(ui, &mut acts),
                    Tab::Installed => self.installed_tab(ui, &mut acts),
                    Tab::Updates => self.updates(ui, &mut acts),
                    Tab::Services => self.services_tab(ui, &mut acts),
                }
            });
        self.wizard(&ctx);
        self.location(&ctx);
        self.archive_dialog(&ctx);
        self.take_snapshot(&ctx);
        for a in acts {
            self.act(&ctx, a);
        }
    }
}

// ── pieces ─────────────────────────────────────────────────────────────────────────────────

#[derive(Clone, Copy)]
enum Icon {
    Library,
    Download,
    Refresh,
    Server,
    Play,
    Pause,
    Check,
    Close,
    Right,
    Down,
}

fn paint_icon(ui: &Ui, icon: Icon, center: Pos2, color: Color32) {
    let p = ui.painter();
    let s = Stroke::new(1.6, color);
    let at = |x: f32, y: f32| center + vec2(x, y);
    match icon {
        Icon::Library => {
            for x in [-6.0, -2.0, 2.0] {
                p.line_segment([at(x, -6.0), at(x, 6.0)], s);
            }
            p.line_segment([at(5.0, -5.0), at(7.5, 6.0)], s);
        }
        Icon::Download => {
            p.line_segment([at(0.0, -6.0), at(0.0, 2.5)], s);
            p.add(egui::Shape::line(
                vec![at(-4.0, -1.5), at(0.0, 2.5), at(4.0, -1.5)],
                s,
            ));
            p.add(egui::Shape::line(
                vec![at(-7.0, 2.0), at(-7.0, 6.0), at(7.0, 6.0), at(7.0, 2.0)],
                s,
            ));
        }
        Icon::Refresh => {
            let pts: Vec<Pos2> = (0..=22)
                .map(|i| {
                    let a = (30.0 + 290.0 * i as f32 / 22.0).to_radians();
                    at(6.0 * a.cos(), -6.0 * a.sin())
                })
                .collect();
            let end = *pts.last().unwrap_or(&center);
            p.add(egui::Shape::line(pts, s));
            p.add(egui::Shape::line(
                vec![end + vec2(-3.0, -1.0), end, end + vec2(1.0, -3.5)],
                s,
            ));
        }
        Icon::Server => {
            for y in [-6.0, 1.0] {
                p.rect_stroke(
                    Rect::from_min_size(at(-7.0, y), vec2(14.0, 5.5)),
                    1.5,
                    s,
                    StrokeKind::Inside,
                );
                p.circle_filled(at(-4.0, y + 2.75), 0.9, color);
            }
        }
        Icon::Play => {
            p.add(egui::Shape::convex_polygon(
                vec![at(-3.5, -5.0), at(5.0, 0.0), at(-3.5, 5.0)],
                color,
                Stroke::NONE,
            ));
        }
        Icon::Pause => {
            for x in [-4.5, 1.0] {
                p.rect_filled(Rect::from_min_size(at(x, -5.0), vec2(3.5, 10.0)), 1.0, color);
            }
        }
        Icon::Check => {
            p.add(egui::Shape::line(
                vec![at(-4.5, 0.0), at(-1.5, 3.0), at(4.5, -3.5)],
                Stroke::new(2.0, color),
            ));
        }
        Icon::Close => {
            p.line_segment([at(-4.0, -4.0), at(4.0, 4.0)], s);
            p.line_segment([at(-4.0, 4.0), at(4.0, -4.0)], s);
        }
        Icon::Right => {
            p.add(egui::Shape::convex_polygon(
                vec![at(-2.0, -4.0), at(3.0, 0.0), at(-2.0, 4.0)],
                color,
                Stroke::NONE,
            ));
        }
        Icon::Down => {
            p.add(egui::Shape::convex_polygon(
                vec![at(-4.0, -2.0), at(4.0, -2.0), at(0.0, 3.0)],
                color,
                Stroke::NONE,
            ));
        }
    }
}

#[derive(Clone, Copy, PartialEq)]
enum BtnKind {
    Primary,
    Secondary,
    Ghost,
}

fn button(ui: &mut Ui, label: &str, kind: BtnKind, enabled: bool) -> bool {
    let font = FontId::new(14.0, theme::heading_family());
    let color = match kind {
        BtnKind::Secondary => TEXT,
        _ => ACCENT,
    };
    let galley = ui.painter().layout_no_wrap(label.to_string(), font, color);
    let pad = if kind == BtnKind::Ghost { 6.0 } else { 17.0 };
    let size = vec2(galley.size().x + pad * 2.0, 32.0);
    let (rect, resp) = ui.allocate_exact_size(size, if enabled { Sense::click() } else { Sense::hover() });
    let alpha = if enabled { 1.0 } else { 0.45 };
    let fill = match (resp.hovered() && enabled, kind) {
        (false, _) => Color32::TRANSPARENT,
        (true, BtnKind::Secondary) => Color32::from_rgba_unmultiplied(0x20, 0x1f, 0x1d, 18),
        (true, _) => Color32::from_rgba_unmultiplied(0xb6, 0x82, 0x35, 31),
    };
    let border = match kind {
        BtnKind::Primary => ACCENT,
        BtnKind::Secondary => DIVIDER,
        BtnKind::Ghost => Color32::TRANSPARENT,
    };
    ui.painter().rect(
        rect,
        4.0,
        fill,
        Stroke::new(1.0, border.gamma_multiply(alpha)),
        StrokeKind::Inside,
    );
    let pos = rect.center() - galley.size() / 2.0;
    ui.painter().galley(pos, galley, color.gamma_multiply(alpha));
    if enabled {
        resp.on_hover_cursor(egui::CursorIcon::PointingHand).clicked()
    } else {
        false
    }
}

fn card(ui: &mut Ui, add: impl FnOnce(&mut Ui)) {
    Frame::new()
        .fill(N100)
        .stroke(Stroke::new(1.0, DIVIDER))
        .corner_radius(CornerRadius::same(4))
        .show(ui, |ui| {
            ui.spacing_mut().item_spacing.y = 0.0;
            ui.set_width(ui.available_width());
            add(ui);
        });
}

/// One list row of `height`; returns its rectangle and response. `first` suppresses the rule above.
fn row(ui: &mut Ui, height: f32, first: bool, clickable: bool) -> (Rect, egui::Response) {
    let (rect, resp) = ui.allocate_exact_size(
        vec2(ui.available_width(), height),
        if clickable { Sense::click() } else { Sense::hover() },
    );
    if resp.hovered() && clickable {
        ui.painter()
            .rect_filled(rect, 0.0, Color32::from_rgba_unmultiplied(0xb6, 0x82, 0x35, 15));
    }
    if !first {
        ui.painter()
            .hline(rect.x_range(), rect.top(), Stroke::new(1.0, DIVIDER));
    }
    (rect, resp)
}

fn text_clipped(ui: &Ui, pos: Pos2, anchor: Align2, s: &str, size: f32, color: Color32, clip: Rect) {
    ui.painter()
        .with_clip_rect(clip)
        .text(pos, anchor, s, FontId::proportional(size), color);
}

fn checkbox(ui: &Ui, center: Pos2, on: bool) {
    let r = Rect::from_center_size(center, vec2(16.0, 16.0));
    let (fill, border) = if on { (A100, ACCENT) } else { (BG, N500) };
    ui.painter()
        .rect(r, 2.0, fill, Stroke::new(1.0, border), StrokeKind::Inside);
    if on {
        paint_icon(ui, Icon::Check, center, A800);
    }
}

fn section_title(ui: &mut Ui, text: &str) {
    ui.add_space(2.0);
    ui.label(theme::heading(text, 17.0));
    ui.add_space(6.0);
}

fn fmt_size(bytes: u64) -> String {
    if bytes == 0 {
        "\u{2014}".into()
    } else {
        fmt_bytes(bytes)
    }
}

/// The last two folders of a path (`\u{2026}/share/kit`), so it fits the sidebar.
fn short_path(p: &std::path::Path) -> String {
    let parts: Vec<_> = p
        .components()
        .rev()
        .take(2)
        .map(|c| c.as_os_str().to_string_lossy().into_owned())
        .collect();
    format!(
        "\u{2026}/{}",
        parts.into_iter().rev().collect::<Vec<_>>().join("/")
    )
}

fn ago(t: Instant) -> String {
    let s = t.elapsed().as_secs();
    match s {
        0..=59 => "just now".into(),
        60..=3599 => format!("{} min ago", s / 60),
        _ => format!("{} h ago", s / 3600),
    }
}

fn eta(bytes: f64, speed: f64) -> String {
    if speed < 1.0 {
        return String::new();
    }
    let s = (bytes / speed) as u64;
    match s {
        0..=59 => "under a minute".into(),
        60..=3599 => format!("{} min", s / 60),
        _ => format!("{} h {:02} min", s / 3600, s % 3600 / 60),
    }
}

impl App {
    fn header(&self, ui: &mut Ui, _acts: &mut Vec<Act>) {
        let title = match self.tab {
            Tab::Library => "Library",
            Tab::Downloads => "Downloads",
            Tab::Installed => "Installed",
            Tab::Updates => "Updates",
            Tab::Services => "Services",
        };
        egui::Panel::top("header")
            .exact_size(46.0)
            .frame(Frame::new().fill(BG))
            .show(ui, |ui| {
                let full = ui.max_rect();
                ui.painter()
                    .hline(full.x_range(), full.bottom() - 0.5, Stroke::new(1.0, DIVIDER));
                let brand = Rect::from_min_size(full.min, vec2(SIDEBAR, full.height()));
                ui.painter().rect_filled(brand, 0.0, SURFACE);
                ui.painter()
                    .vline(brand.right(), full.y_range(), Stroke::new(1.0, DIVIDER));
                ui.painter().text(
                    brand.left_center() + vec2(14.0, 0.0),
                    Align2::LEFT_CENTER,
                    "Blackout Kit",
                    FontId::new(18.0, theme::heading_family()),
                    TEXT,
                );
                ui.painter().text(
                    full.left_center() + vec2(SIDEBAR + 16.0, 0.0),
                    Align2::LEFT_CENTER,
                    title,
                    FontId::new(17.0, theme::heading_family()),
                    TEXT,
                );
            });
    }

    fn sidebar(&self, ui: &mut Ui, acts: &mut Vec<Act>) {
        let ups = self.ws.as_ref().map_or(0, |w| w.updates().len());
        let downloads = self.runner.jobs.len();
        let installed = self.ws.as_ref().map_or(0, |w| w.installed.len());
        egui::Panel::left("sidebar")
            .exact_size(SIDEBAR)
            .resizable(false)
            .frame(Frame::new().fill(SURFACE).inner_margin(Margin::symmetric(8, 10)))
            .show(ui, |ui| {
                ui.painter().vline(
                    ui.max_rect().right() + 8.0,
                    ui.max_rect().y_range(),
                    Stroke::new(1.0, DIVIDER),
                );
                ui.spacing_mut().item_spacing.y = 2.0;
                let items = [
                    (Tab::Library, "Library", Icon::Library, 0),
                    (Tab::Downloads, "Downloads", Icon::Download, downloads),
                    (Tab::Installed, "Installed", Icon::Check, installed),
                    (Tab::Updates, "Updates", Icon::Refresh, ups),
                    (Tab::Services, "Services", Icon::Server, 0),
                ];
                for (tab, label, icon, badge) in items {
                    let (rect, resp) =
                        ui.allocate_exact_size(vec2(ui.available_width(), 34.0), Sense::click());
                    let on = self.tab == tab;
                    if on || resp.hovered() {
                        ui.painter().rect_filled(rect, 4.0, N200);
                    }
                    let color = if on { A800 } else { TEXT };
                    paint_icon(ui, icon, rect.left_center() + vec2(18.0, 0.0), color);
                    ui.painter().text(
                        rect.left_center() + vec2(36.0, 0.0),
                        Align2::LEFT_CENTER,
                        label,
                        FontId::proportional(13.0),
                        color,
                    );
                    if badge > 0 {
                        let g =
                            ui.painter()
                                .layout_no_wrap(badge.to_string(), FontId::proportional(11.0), A700);
                        let b = Rect::from_center_size(
                            rect.right_center() - vec2(10.0 + (g.size().x + 12.0) / 2.0, 0.0),
                            vec2(g.size().x + 12.0, 18.0),
                        );
                        ui.painter()
                            .rect_stroke(b, 9.0, Stroke::new(1.0, ACCENT), StrokeKind::Inside);
                        ui.painter().galley(b.center() - g.size() / 2.0, g, A700);
                    }
                    if resp.on_hover_cursor(egui::CursorIcon::PointingHand).clicked() {
                        acts.push(Act::Tab(tab));
                    }
                }
                ui.add_space(8.0);
                ui.painter().hline(
                    (ui.cursor().left() + 4.0)..=(ui.cursor().right() - 4.0),
                    ui.cursor().top(),
                    Stroke::new(1.0, DIVIDER),
                );
                ui.add_space(8.0);
                let (rect, resp) = ui.allocate_exact_size(vec2(ui.available_width(), 34.0), Sense::click());
                if resp.hovered() {
                    ui.painter().rect_filled(rect, 4.0, N200);
                }
                ui.painter().text(
                    rect.left_center() + vec2(10.0, 0.0),
                    Align2::LEFT_CENTER,
                    "Open portal \u{2197}",
                    FontId::proportional(13.0),
                    TEXT,
                );
                if resp.on_hover_cursor(egui::CursorIcon::PointingHand).clicked() {
                    acts.push(Act::Open("https://kit.local".into()));
                }
                let (rect, resp) = ui.allocate_exact_size(vec2(ui.available_width(), 34.0), Sense::click());
                if resp.hovered() {
                    ui.painter().rect_filled(rect, 4.0, N200);
                }
                ui.painter().text(
                    rect.left_center() + vec2(10.0, 0.0),
                    Align2::LEFT_CENTER,
                    "Archive a website\u{2026}",
                    FontId::proportional(13.0),
                    TEXT,
                );
                if resp
                    .on_hover_text("Crawl a site that has no ZIM file and add it to the library")
                    .on_hover_cursor(egui::CursorIcon::PointingHand)
                    .clicked()
                {
                    acts.push(Act::ArchiveWebsite);
                }

                ui.with_layout(Layout::bottom_up(Align::LEFT), |ui| {
                    ui.add_space(6.0);
                    let (dot, label) = if self.online {
                        (ACCENT, "Online \u{b7} updates available")
                    } else {
                        (N500, "Offline \u{b7} all content local")
                    };
                    ui.horizontal(|ui| {
                        ui.add_space(10.0);
                        let (r, _) = ui.allocate_exact_size(vec2(7.0, 14.0), Sense::hover());
                        ui.painter().circle_filled(r.center(), 3.5, dot);
                        ui.label(theme::small(label, TEXT));
                    });
                    ui.add_space(6.0);
                    ui.painter().hline(
                        (ui.cursor().left() + 10.0)..=(ui.cursor().right() - 10.0),
                        ui.cursor().top() + 1.0,
                        Stroke::new(1.0, DIVIDER),
                    );
                    if let Some(ws) = &self.ws {
                        let used = ws.drive_bytes.saturating_sub(ws.free_bytes);
                        ui.horizontal(|ui| {
                            ui.add_space(10.0);
                            let r = ui
                                .add(
                                    egui::Label::new(
                                        theme::small(short_path(&self.cfg.data_dir), N700).size(11.0),
                                    )
                                    .sense(Sense::click()),
                                )
                                .on_hover_text(format!(
                                    "{}\nClick to change the storage location",
                                    self.cfg.data_dir.display()
                                ))
                                .on_hover_cursor(egui::CursorIcon::PointingHand);
                            if r.clicked() {
                                acts.push(Act::ChangeLocation);
                            }
                            if ui
                                .add(
                                    egui::Label::new(theme::small("Change", ACCENT).size(11.0))
                                        .sense(Sense::click()),
                                )
                                .on_hover_cursor(egui::CursorIcon::PointingHand)
                                .clicked()
                            {
                                acts.push(Act::ChangeLocation);
                            }
                        });
                        ui.horizontal(|ui| {
                            ui.add_space(10.0);
                            ui.label(theme::small(
                                format!(
                                    "{} used \u{b7} {} free",
                                    fmt_bytes(used),
                                    fmt_bytes(ws.free_bytes)
                                ),
                                N800,
                            ));
                        });
                        ui.horizontal(|ui| {
                            ui.add_space(10.0);
                            let (r, _) = ui
                                .allocate_exact_size(vec2(ui.available_width() - 10.0, 6.0), Sense::hover());
                            ui.painter().rect_filled(r, 3.0, N200);
                            let frac = if ws.drive_bytes == 0 {
                                0.0
                            } else {
                                used as f32 / ws.drive_bytes as f32
                            };
                            ui.painter().rect_filled(
                                Rect::from_min_size(r.min, vec2(r.width() * frac.clamp(0.0, 1.0), 6.0)),
                                3.0,
                                N700,
                            );
                        });
                        ui.horizontal(|ui| {
                            ui.add_space(10.0);
                            ui.label(RichText::new("STORAGE").size(10.0).color(N700));
                        });
                    }
                });
            });
    }

    fn footer(&self, ui: &mut Ui, acts: &mut Vec<Act>) {
        let Some(ws) = &self.ws else { return };
        let diff = ws.diff(&|id| self.runner.is_queued(id));
        let free = ws.free_after(&diff);
        let nothing = diff.add.is_empty() && diff.remove.is_empty();
        egui::Panel::bottom("footer")
            .exact_size(60.0)
            .frame(Frame::new().fill(SURFACE).inner_margin(Margin::symmetric(16, 8)))
            .show(ui, |ui| {
                let r = ui.max_rect();
                ui.painter()
                    .hline(r.expand(8.0).x_range(), r.top() - 8.0, Stroke::new(1.0, DIVIDER));
                ui.horizontal_centered(|ui| {
                    ui.vertical(|ui| {
                        ui.set_width(220.0);
                        ui.add_space(4.0);
                        let (bar, _) = ui.allocate_exact_size(vec2(220.0, 8.0), Sense::hover());
                        ui.painter()
                            .rect(bar, 2.0, BG, Stroke::new(1.0, DIVIDER), StrokeKind::Inside);
                        let w = |b: f64| {
                            (bar.width() as f64 * (b / ws.drive_bytes.max(1) as f64))
                                .clamp(0.0, bar.width() as f64) as f32
                        };
                        let used = (ws.drive_bytes.saturating_sub(ws.free_bytes)) as f64;
                        let keep = (used - diff.remove_bytes as f64).max(0.0);
                        ui.painter()
                            .rect_filled(Rect::from_min_size(bar.min, vec2(w(keep), 8.0)), 0.0, N700);
                        ui.painter().rect_filled(
                            Rect::from_min_size(
                                bar.min + vec2(w(keep), 0.0),
                                vec2(w(diff.add_bytes as f64), 8.0),
                            ),
                            0.0,
                            ACCENT,
                        );
                        let (msg, col) = if free >= 0 {
                            (format!("{} free afterwards", fmt_bytes(free as u64)), N700)
                        } else {
                            (
                                format!(
                                    "{} short \u{2014} does not fit",
                                    fmt_bytes(free.unsigned_abs() as u64)
                                ),
                                A800,
                            )
                        };
                        ui.label(theme::small(msg, col).size(11.0));
                    });
                    ui.add_space(18.0);
                    ui.vertical(|ui| {
                        let top = if nothing {
                            "Installed content matches your selection".to_string()
                        } else if diff.add.is_empty() {
                            "Nothing to download".to_string()
                        } else {
                            format!(
                                "{} to download \u{b7} {}",
                                diff.add.len(),
                                fmt_bytes(diff.add_bytes)
                            )
                        };
                        ui.label(RichText::new(top).size(13.0));
                        let mut sub = if diff.remove.is_empty() {
                            "Nothing to remove".to_string()
                        } else {
                            format!(
                                "{} to remove \u{b7} {}",
                                diff.remove.len(),
                                fmt_bytes(diff.remove_bytes)
                            )
                        };
                        if !self.online && !diff.add.is_empty() {
                            sub.push_str(" \u{b7} starts when online");
                        }
                        ui.label(theme::small(sub, N700).size(11.0));
                    });
                    ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                        let label = if !diff.add.is_empty() {
                            "Download and install"
                        } else if !diff.remove.is_empty() {
                            "Remove"
                        } else {
                            "Apply"
                        };
                        if button(
                            ui,
                            label,
                            BtnKind::Primary,
                            !nothing && free >= 0 && !self.runner.is_removing(),
                        ) {
                            acts.push(Act::Apply);
                        }
                        if button(ui, "Keep installed", BtnKind::Secondary, true) {
                            acts.push(Act::KeepInstalled);
                        }
                    });
                });
            });
    }

    fn toolbar_frame(ui: &mut Ui, add: impl FnOnce(&mut Ui)) {
        Frame::new()
            .inner_margin(Margin::symmetric(16, 10))
            .show(ui, |ui| {
                ui.set_width(ui.available_width());
                ui.horizontal(|ui| add(ui));
            });
        let y = ui.cursor().top();
        ui.painter()
            .hline(ui.max_rect().x_range(), y, Stroke::new(1.0, DIVIDER));
    }

    fn library(&mut self, ui: &mut Ui, acts: &mut Vec<Act>) {
        let Some(ws) = &self.ws else { return };
        let ws_langs: Vec<String> = ws.langs.iter().cloned().collect();
        let all_langs = ws.all_langs();

        Self::toolbar_frame(ui, |ui| {
            // Profile presets
            let mut defs: Vec<(String, String)> = ws
                .profiles
                .iter()
                .map(|p| (p.id.clone(), p.name.clone()))
                .collect();
            defs.push(("custom".into(), "Custom".into()));
            let start = ui.cursor().min;
            let total_w: f32 = defs.len() as f32 * 84.0;
            let group = Rect::from_min_size(start, vec2(total_w, 30.0));
            ui.painter()
                .rect_stroke(group, 4.0, Stroke::new(1.0, DIVIDER), StrokeKind::Inside);
            let mut x = group.left();
            for (id, name) in &defs {
                let r = Rect::from_min_size(Pos2::new(x, group.top()), vec2(84.0, 30.0));
                let resp = ui.interact(r, ui.scope_id().with(("profile", id)), Sense::click());
                let on = ws.profile == *id;
                if on {
                    ui.painter().rect_filled(r.shrink(1.0), 3.0, A100);
                } else if resp.hovered() {
                    ui.painter().rect_filled(r.shrink(1.0), 3.0, N200);
                }
                if x > group.left() {
                    ui.painter().vline(x, group.y_range(), Stroke::new(1.0, DIVIDER));
                }
                ui.painter().text(
                    r.center(),
                    Align2::CENTER_CENTER,
                    name,
                    FontId::proportional(13.0),
                    if on { A800 } else { TEXT },
                );
                if resp.on_hover_cursor(egui::CursorIcon::PointingHand).clicked() {
                    acts.push(Act::Profile(id.clone()));
                }
                x += 84.0;
            }
            ui.allocate_rect(group, Sense::hover());
            let hint = ws
                .profiles
                .iter()
                .find(|p| p.id == ws.profile)
                .map_or("Your own selection".to_string(), |p| p.description.clone());
            ui.add_space(6.0);
            // The filter box and the language button need RIGHT_TOOLS on the right; the hint wraps
            // inside the rest, so it grows taller instead of running under them.
            let room = (ui.available_width() - RIGHT_TOOLS).max(80.0);
            ui.allocate_ui(vec2(room, 30.0), |ui| {
                ui.set_max_width(room);
                ui.add(
                    egui::Label::new(theme::small(hint, N700))
                        .wrap()
                        .selectable(false),
                );
            });

            ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                ui.add(
                    egui::TextEdit::singleline(&mut self.find)
                        .hint_text("Filter packages")
                        .desired_width(190.0)
                        .margin(Margin::symmetric(8, 6)),
                );
                let summary = match ws_langs.len() {
                    0 => "No language".to_string(),
                    n if n == all_langs.len() => "All languages".to_string(),
                    1 | 2 => ws_langs
                        .iter()
                        .map(|l| language_name(l))
                        .collect::<Vec<_>>()
                        .join(", "),
                    n => format!("{n} languages"),
                };
                let lb = egui::Button::new(RichText::new(format!("{summary}  \u{25be}")).size(13.0))
                    .stroke(Stroke::new(1.0, if self.lang_open { ACCENT } else { DIVIDER }))
                    .min_size(vec2(0.0, 30.0));
                let resp = ui.add(lb);
                if resp.clicked() {
                    self.lang_open = !self.lang_open;
                    self.lang_find.clear();
                }
                if self.lang_open {
                    let area = egui::Area::new(ui.scope_id().with("langs"))
                        .order(egui::Order::Foreground)
                        .fixed_pos(resp.rect.right_bottom() + vec2(-240.0, 4.0))
                        .show(ui.ctx(), |ui| {
                            Frame::new()
                                .fill(BG)
                                .stroke(Stroke::new(1.0, N500))
                                .corner_radius(CornerRadius::same(4))
                                .inner_margin(Margin::same(6))
                                .show(ui, |ui| {
                                    ui.set_width(228.0);
                                    ui.add(
                                        egui::TextEdit::singleline(&mut self.lang_find)
                                            .hint_text("Find a language")
                                            .desired_width(f32::INFINITY),
                                    );
                                    ui.add_space(4.0);
                                    let q = self.lang_find.trim().to_lowercase();
                                    for l in all_langs.iter().filter(|l| {
                                        q.is_empty()
                                            || format!("{} {l}", language_name(l)).to_lowercase().contains(&q)
                                    }) {
                                        let (r, resp) = ui.allocate_exact_size(
                                            vec2(ui.available_width(), 30.0),
                                            Sense::click(),
                                        );
                                        if resp.hovered() {
                                            ui.painter().rect_filled(r, 2.0, N200);
                                        }
                                        checkbox(ui, r.left_center() + vec2(10.0, 0.0), ws_langs.contains(l));
                                        ui.painter().text(
                                            r.left_center() + vec2(26.0, 0.0),
                                            Align2::LEFT_CENTER,
                                            language_name(l),
                                            FontId::proportional(13.0),
                                            TEXT,
                                        );
                                        let n = ws.pkgs.iter().filter(|p| p.langs.contains(l)).count();
                                        ui.painter().text(
                                            r.right_center() - vec2(8.0, 0.0),
                                            Align2::RIGHT_CENTER,
                                            format!("{n} {}", if n == 1 { "package" } else { "packages" }),
                                            FontId::proportional(11.0),
                                            N700,
                                        );
                                        if resp.clicked() {
                                            acts.push(Act::ToggleLang(l.clone()));
                                        }
                                    }
                                    ui.horizontal(|ui| {
                                        if button(ui, "Select all", BtnKind::Ghost, true) {
                                            acts.push(Act::AllLangs);
                                        }
                                        ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                                            if button(ui, "Clear", BtnKind::Ghost, true) {
                                                acts.push(Act::NoLangs);
                                            }
                                        });
                                    });
                                });
                        });
                    if ui.input(|i| i.pointer.any_pressed())
                        && !area.response.hovered()
                        && !resp.hovered()
                        && ui
                            .input(|i| i.pointer.interact_pos())
                            .is_some_and(|p| !area.response.rect.contains(p))
                    {
                        self.lang_open = false;
                    }
                }
            });
        });

        let find = self.find.trim().to_lowercase();
        ScrollArea::vertical().auto_shrink([false, false]).show(ui, |ui| {
            Frame::new()
                .inner_margin(Margin {
                    left: 20,
                    right: 20,
                    top: 16,
                    bottom: 24,
                })
                .show(ui, |ui| {
                    ui.set_width(ui.available_width());
                    let mut any = false;
                    for (cat, label) in ws.categories() {
                        let all: Vec<_> = ws.pkgs.iter().filter(|p| p.category == cat).collect();
                        let is_wiki = cat == "wikipedia";
                        let shown: Vec<_> = all.iter().filter(|p| ws.lang_ok(p)).collect();
                        let items: Vec<_> = all
                            .iter()
                            .filter(|p| {
                                let visible = if is_wiki {
                                    ws.sel.contains(&p.id) || ws.installed.contains_key(&p.id)
                                } else {
                                    ws.lang_ok(p)
                                };
                                visible
                                    && (find.is_empty()
                                        || format!("{} {}", p.name, p.desc).to_lowercase().contains(&find))
                            })
                            .collect();
                        if items.is_empty() && !(is_wiki && find.is_empty()) {
                            continue;
                        }
                        any = true;
                        let on_ids: Vec<&String> = all
                            .iter()
                            .filter(|p| (is_wiki || ws.lang_ok(p)) && ws.sel.contains(&p.id))
                            .map(|p| &p.id)
                            .collect();
                        let on_bytes = ws.sum(on_ids.iter());
                        let summary = if is_wiki {
                            if on_ids.is_empty() {
                                "Not in plan".to_string()
                            } else {
                                format!(
                                    "{} {} \u{b7} {}",
                                    on_ids.len(),
                                    if on_ids.len() == 1 {
                                        "language"
                                    } else {
                                        "languages"
                                    },
                                    fmt_bytes(on_bytes)
                                )
                            }
                        } else {
                            format!(
                                "{} of {}{}",
                                on_ids.len(),
                                shown.len(),
                                if on_ids.is_empty() {
                                    String::new()
                                } else {
                                    format!(" \u{b7} {}", fmt_bytes(on_bytes))
                                }
                            )
                        };
                        let open = !find.is_empty() || !self.collapsed.contains(&cat);

                        let (hr, hresp) =
                            ui.allocate_exact_size(vec2(ui.available_width(), 26.0), Sense::click());
                        paint_icon(
                            ui,
                            if open { Icon::Down } else { Icon::Right },
                            hr.left_center() + vec2(10.0, 0.0),
                            N600,
                        );
                        ui.painter().text(
                            hr.left_center() + vec2(24.0, 0.0),
                            Align2::LEFT_CENTER,
                            &label,
                            FontId::new(17.0, theme::heading_family()),
                            TEXT,
                        );
                        ui.painter().text(
                            hr.right_center() - vec2(4.0, 0.0),
                            Align2::RIGHT_CENTER,
                            summary,
                            FontId::proportional(12.0),
                            N700,
                        );
                        if hresp.on_hover_cursor(egui::CursorIcon::PointingHand).clicked() {
                            acts.push(Act::ToggleCat(cat.clone()));
                        }
                        if open {
                            card(ui, |ui| {
                                for (i, p) in items.iter().enumerate() {
                                    let (r, resp) = row(ui, 48.0, i == 0, true);
                                    let on = ws.sel.contains(&p.id);
                                    checkbox(ui, r.left_center() + vec2(22.0, 0.0), on);
                                    let st = ws.status(&p.id, self.runner.is_queued(&p.id));
                                    let right = r.right() - 14.0;
                                    text_clipped(
                                        ui,
                                        Pos2::new(right, r.center().y),
                                        Align2::RIGHT_CENTER,
                                        &fmt_size(p.bytes),
                                        13.0,
                                        TEXT,
                                        r,
                                    );
                                    let status_right = right - 62.0 - 12.0;
                                    if st != Status::None {
                                        let (fg, bg, bd) = match st {
                                            Status::Downloading | Status::Update => {
                                                (A800, A100, Color32::TRANSPARENT)
                                            }
                                            Status::WillRemove => (N800, Color32::TRANSPARENT, N500),
                                            Status::Installed => (N800, N200, Color32::TRANSPARENT),
                                            _ => (A700, Color32::TRANSPARENT, ACCENT),
                                        };
                                        let g = ui.painter().layout_no_wrap(
                                            st.label().to_string(),
                                            FontId::proportional(11.0),
                                            fg,
                                        );
                                        let b = Rect::from_center_size(
                                            Pos2::new(status_right - (g.size().x + 16.0) / 2.0, r.center().y),
                                            vec2(g.size().x + 16.0, 18.0),
                                        );
                                        ui.painter().rect(
                                            b,
                                            2.0,
                                            bg,
                                            Stroke::new(1.0, bd),
                                            StrokeKind::Inside,
                                        );
                                        ui.painter().galley(b.center() - g.size() / 2.0, g, fg);
                                    }
                                    let meta_left = status_right - 110.0 - 190.0;
                                    let meta = [model::license_label(&p.license), &p.langs.join(", ")]
                                        .iter()
                                        .filter(|s| !s.is_empty())
                                        .copied()
                                        .collect::<Vec<_>>()
                                        .join(" \u{b7} ");
                                    let meta_clip = Rect::from_min_max(
                                        Pos2::new(meta_left, r.top()),
                                        Pos2::new(meta_left + 190.0, r.bottom()),
                                    );
                                    text_clipped(
                                        ui,
                                        Pos2::new(meta_left, r.center().y),
                                        Align2::LEFT_CENTER,
                                        &meta,
                                        11.0,
                                        N600,
                                        meta_clip,
                                    );
                                    let main_left = r.left() + 14.0 + 16.0 + 12.0;
                                    let clip = Rect::from_min_max(
                                        Pos2::new(main_left, r.top()),
                                        Pos2::new(meta_left - 12.0, r.bottom()),
                                    );
                                    text_clipped(
                                        ui,
                                        Pos2::new(main_left, r.center().y - 9.0),
                                        Align2::LEFT_CENTER,
                                        &p.name,
                                        13.0,
                                        TEXT,
                                        clip,
                                    );
                                    let name_w = ui
                                        .painter()
                                        .layout_no_wrap(p.name.clone(), FontId::proportional(13.0), TEXT)
                                        .size()
                                        .x;
                                    let note = if let Some(n) = p.needs.first().and_then(|n| ws.pkg(n)) {
                                        format!("Plays in {}", n.name)
                                    } else {
                                        String::new()
                                    };
                                    if !note.is_empty() {
                                        text_clipped(
                                            ui,
                                            Pos2::new(main_left + name_w + 8.0, r.center().y - 9.0),
                                            Align2::LEFT_CENTER,
                                            &note,
                                            11.0,
                                            A700,
                                            clip,
                                        );
                                    }
                                    text_clipped(
                                        ui,
                                        Pos2::new(main_left, r.center().y + 9.0),
                                        Align2::LEFT_CENTER,
                                        &p.desc,
                                        12.0,
                                        N700,
                                        clip,
                                    );
                                    if resp.on_hover_cursor(egui::CursorIcon::PointingHand).clicked() {
                                        acts.push(if is_wiki {
                                            Act::OpenWizard
                                        } else {
                                            Act::TogglePkg(p.id.clone())
                                        });
                                    }
                                }
                                if is_wiki {
                                    let (r, resp) = row(ui, 40.0, items.is_empty(), true);
                                    let label = if on_ids.is_empty() {
                                        "Set up Wikipedia\u{2026}"
                                    } else {
                                        "Change languages and editions\u{2026}"
                                    };
                                    ui.painter().text(
                                        r.left_center() + vec2(14.0, 0.0),
                                        Align2::LEFT_CENTER,
                                        label,
                                        FontId::proportional(13.0),
                                        A700,
                                    );
                                    ui.painter().text(
                                        r.right_center() - vec2(14.0, 0.0),
                                        Align2::RIGHT_CENTER,
                                        "Languages and edition",
                                        FontId::proportional(11.0),
                                        N700,
                                    );
                                    if resp.on_hover_cursor(egui::CursorIcon::PointingHand).clicked() {
                                        acts.push(Act::OpenWizard);
                                    }
                                }
                            });
                        }
                        ui.add_space(18.0);
                    }
                    if !any {
                        ui.add_space(40.0);
                        ui.vertical_centered(|ui| {
                            ui.label(theme::small(
                                format!("No packages match \u{201c}{}\u{201d}.", self.find.trim()),
                                N700,
                            ))
                        });
                    }
                });
        });
    }

    fn downloads(&mut self, ui: &mut Ui, acts: &mut Vec<Act>) {
        let jobs = self.runner.jobs.clone();
        let left = self.runner.remaining_bytes();
        let running = jobs.iter().any(|j| j.state == JobState::Running);
        let speed: f64 = jobs.iter().map(|j| j.speed).sum();
        Self::toolbar_frame(ui, |ui| {
            let title = if jobs.is_empty() {
                "No active downloads".to_string()
            } else {
                let mut t = format!("{} in queue \u{b7} {} left", jobs.len(), fmt_bytes(left));
                if self.online && running && !eta(left as f64, speed).is_empty() {
                    t.push_str(&format!(" \u{b7} about {}", eta(left as f64, speed)));
                }
                t
            };
            ui.label(RichText::new(title).size(13.0));
            if !jobs.is_empty() {
                ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                    if button(
                        ui,
                        if self.runner.all_paused() {
                            "Resume all"
                        } else {
                            "Pause all"
                        },
                        BtnKind::Secondary,
                        true,
                    ) {
                        acts.push(Act::PauseAll);
                    }
                });
            }
        });
        ScrollArea::vertical().auto_shrink([false, false]).show(ui, |ui| {
            Frame::new().inner_margin(Margin { left: 20, right: 20, top: 16, bottom: 24 }).show(ui, |ui| {
                ui.set_width(ui.available_width());
                if !self.online && !jobs.is_empty() {
                    Frame::new().stroke(Stroke::new(1.0, ACCENT)).corner_radius(CornerRadius::same(4)).inner_margin(Margin::symmetric(14, 10)).show(ui, |ui| {
                        ui.set_width(ui.available_width());
                        ui.label(RichText::new("Offline. Downloads resume where they stopped once a connection is back.").size(13.0).color(A800));
                    });
                    ui.add_space(18.0);
                }
                for msg in self.failures.clone() {
                    Frame::new().stroke(Stroke::new(1.0, N500)).corner_radius(CornerRadius::same(4)).inner_margin(Margin::symmetric(14, 8)).show(ui, |ui| {
                        ui.set_width(ui.available_width());
                        ui.label(RichText::new(msg).size(12.0).color(N800));
                    });
                    ui.add_space(6.0);
                }
                if !self.failures.is_empty() {
                    if button(ui, "Dismiss errors", BtnKind::Ghost, true) {
                        self.failures.clear();
                    }
                    ui.add_space(12.0);
                }
                if jobs.is_empty() {
                    ui.add_space(60.0);
                    ui.vertical_centered(|ui| {
                        let (r, _) = ui.allocate_exact_size(vec2(32.0, 32.0), Sense::hover());
                        paint_icon(ui, Icon::Download, r.center(), N500);
                        ui.add_space(10.0);
                        ui.label(RichText::new("Nothing is downloading").size(14.0).color(N700));
                        ui.add_space(8.0);
                        if button(ui, "Open library", BtnKind::Secondary, true) {
                            acts.push(Act::Tab(Tab::Library));
                        }
                    });
                } else {
                    card(ui, |ui| {
                        for (i, j) in jobs.iter().enumerate() {
                            let (r, _) = row(ui, 58.0, i == 0, false);
                            let frac = if j.bytes == 0 { 0.0 } else { (j.got as f64 / j.bytes as f64).clamp(0.0, 1.0) as f32 };
                            let line = if j.archive {
                                match &j.state {
                                    JobState::Failed(m) => format!("Stopped: {m}"),
                                    JobState::Paused => "Website copy \u{b7} Paused (starts over when resumed)".to_string(),
                                    JobState::Queued => "Website copy \u{b7} Queued".to_string(),
                                    _ if !self.online => "Website copy \u{b7} Waiting for connection".to_string(),
                                    JobState::Running => {
                                        let s = self.runner.status_of(&j.id);
                                        if s.is_empty() { "Crawling the site\u{2026}".to_string() } else { s }
                                    }
                                }
                            } else { match &j.state {
                                JobState::Failed(m) => format!("Stopped: {m}"),
                                JobState::Paused => format!("{} of {} \u{b7} Paused", fmt_bytes(j.got), fmt_size(j.bytes)),
                                _ if !self.online => format!("{} of {} \u{b7} Waiting for connection", fmt_bytes(j.got), fmt_size(j.bytes)),
                                JobState::Running => {
                                    let rate = if j.speed > 0.0 { format!(" \u{b7} {:.1} MB/s", j.speed / 1e6) } else { String::new() };
                                    let left = eta(j.bytes.saturating_sub(j.got) as f64, j.speed);
                                    format!("{} of {}{rate}{}", fmt_bytes(j.got), fmt_size(j.bytes), if left.is_empty() { String::new() } else { format!(" \u{b7} {left} left") })
                                }
                                JobState::Queued => format!("{} of {} \u{b7} Queued", fmt_bytes(j.got), fmt_size(j.bytes)),
                            } };
                            let btn = Rect::from_center_size(Pos2::new(r.right() - 14.0 - 15.0, r.center().y), vec2(30.0, 30.0));
                            let cancel = btn.translate(vec2(-34.0, 0.0));
                            let text_right = cancel.left() - 14.0;
                            let name = format!("{}{}", if j.update { "Update: " } else { "" }, j.name);
                            let clip = Rect::from_min_max(r.min, Pos2::new(text_right, r.bottom()));
                            let line_w = ui.painter().layout_no_wrap(line.clone(), FontId::proportional(11.0), N700).size().x;
                            text_clipped(ui, Pos2::new(r.left() + 14.0, r.top() + 18.0), Align2::LEFT_CENTER, &name, 13.0, TEXT, Rect::from_min_max(r.min, Pos2::new(text_right - line_w - 12.0, r.bottom())));
                            text_clipped(ui, Pos2::new(text_right, r.top() + 18.0), Align2::RIGHT_CENTER, &line, 11.0, N700, clip);
                            let bar = Rect::from_min_max(Pos2::new(r.left() + 14.0, r.top() + 35.0), Pos2::new(text_right, r.top() + 39.0));
                            ui.painter().rect_filled(bar, 2.0, N200);
                            let col = if matches!(j.state, JobState::Running | JobState::Queued) { ACCENT } else { N500 };
                            ui.painter().rect_filled(Rect::from_min_size(bar.min, vec2(bar.width() * frac, 4.0)), 2.0, col);
                            let resp = ui.interact(btn, ui.scope_id().with(("job", &j.id)), Sense::click());
                            if resp.hovered() {
                                ui.painter().rect_filled(btn, 4.0, N200);
                            }
                            let paused = matches!(j.state, JobState::Paused | JobState::Failed(_));
                            paint_icon(ui, if paused { Icon::Play } else { Icon::Pause }, btn.center(), TEXT);
                            if resp.on_hover_text(if paused { "Resume" } else { "Pause" }).on_hover_cursor(egui::CursorIcon::PointingHand).clicked() {
                                acts.push(Act::ToggleJob(j.id.clone()));
                            }
                            let cresp = ui.interact(cancel, ui.scope_id().with(("cancel", &j.id)), Sense::click());
                            if cresp.hovered() {
                                ui.painter().rect_filled(cancel, 4.0, N200);
                            }
                            paint_icon(ui, Icon::Close, cancel.center(), TEXT);
                            if cresp.on_hover_text("Remove from the list").on_hover_cursor(egui::CursorIcon::PointingHand).clicked() {
                                acts.push(Act::CancelJob(j.id.clone()));
                            }
                        }
                    });
                }
                if !self.done.is_empty() {
                    ui.add_space(18.0);
                    section_title(ui, "Finished");
                    card(ui, |ui| {
                        for (i, (name, update, at)) in self.done.iter().enumerate() {
                            let (r, _) = row(ui, 38.0, i == 0, false);
                            paint_icon(ui, Icon::Check, r.left_center() + vec2(21.0, 0.0), A700);
                            ui.painter().text(r.left_center() + vec2(40.0, 0.0), Align2::LEFT_CENTER, name, FontId::proportional(13.0), TEXT);
                            ui.painter().text(
                                r.right_center() - vec2(14.0, 0.0),
                                Align2::RIGHT_CENTER,
                                format!("{} and verified \u{b7} {}", if *update { "Updated" } else { "Installed" }, ago(*at)),
                                FontId::proportional(11.0),
                                N700,
                            );
                        }
                    });
                }
            });
        });
    }

    fn installed_tab(&mut self, ui: &mut Ui, acts: &mut Vec<Act>) {
        let Some(ws) = &self.ws else { return };
        let mut items: Vec<(String, String, String, u64)> = ws
            .installed
            .iter()
            .map(|(id, i)| {
                let p = ws.pkg(id);
                let name = p.map_or_else(|| id.clone(), |p| p.name.clone());
                let cat = p.map_or_else(String::new, |p| model::category_label(&p.category));
                let bytes = if i.bytes > 0 {
                    i.bytes
                } else {
                    p.map_or(0, |p| p.bytes)
                };
                let ver = i.version.clone().unwrap_or_default();
                let meta = [cat, ver]
                    .iter()
                    .filter(|s| !s.is_empty())
                    .cloned()
                    .collect::<Vec<_>>()
                    .join(" \u{b7} ");
                (id.clone(), name, meta, bytes)
            })
            .collect();
        items.sort_by_key(|a| a.1.to_lowercase());
        let total = ws.installed_bytes();
        let busy = self.runner.is_removing();
        Self::toolbar_frame(ui, |ui| {
            let t = if items.is_empty() {
                "Nothing installed yet".to_string()
            } else {
                format!("{} installed \u{b7} {} on disk", items.len(), fmt_bytes(total))
            };
            ui.label(RichText::new(t).size(13.0));
        });
        ScrollArea::vertical().auto_shrink([false, false]).show(ui, |ui| {
            Frame::new().inner_margin(Margin { left: 20, right: 20, top: 16, bottom: 24 }).show(ui, |ui| {
                ui.set_width(ui.available_width());
                if busy {
                    ui.label(theme::small("Removing\u{2026}", N700));
                    ui.add_space(8.0);
                }
                if items.is_empty() {
                    ui.add_space(48.0);
                    ui.vertical_centered(|ui| {
                        let (r, _) = ui.allocate_exact_size(vec2(32.0, 32.0), Sense::hover());
                        paint_icon(ui, Icon::Library, r.center(), N500);
                        ui.add_space(10.0);
                        ui.label(RichText::new("Nothing is installed").size(14.0).color(N700));
                        ui.add_space(8.0);
                        if button(ui, "Open library", BtnKind::Secondary, true) {
                            acts.push(Act::Tab(Tab::Library));
                        }
                    });
                    return;
                }
                card(ui, |ui| {
                    for (i, (id, name, meta, bytes)) in items.iter().enumerate() {
                        let (r, _) = row(ui, 50.0, i == 0, false);
                        let confirming = self.confirm_remove.as_deref() == Some(id.as_str());
                        let btn_w = if confirming { 70.0 } else { 90.0 };
                        let remove = Rect::from_center_size(Pos2::new(r.right() - 14.0 - btn_w / 2.0, r.center().y), vec2(btn_w, 32.0));
                        let size_right = remove.left() - if confirming { 80.0 } else { 14.0 };
                        let clip = Rect::from_min_max(r.min, Pos2::new(size_right - 70.0, r.bottom()));
                        text_clipped(ui, r.left_center() + vec2(14.0, -9.0), Align2::LEFT_CENTER, name, 13.0, TEXT, clip);
                        text_clipped(ui, r.left_center() + vec2(14.0, 9.0), Align2::LEFT_CENTER, meta, 12.0, N700, clip);
                        ui.painter().text(Pos2::new(size_right, r.center().y), Align2::RIGHT_CENTER, fmt_size(*bytes), FontId::proportional(13.0), TEXT);
                        if confirming {
                            let keep = Rect::from_center_size(Pos2::new(remove.left() - 4.0 - 35.0, r.center().y), vec2(70.0, 32.0));
                            let mut c = ui.new_child(egui::UiBuilder::new().max_rect(keep).layout(Layout::centered_and_justified(egui::Direction::LeftToRight)));
                            if button(&mut c, "Keep", BtnKind::Ghost, true) {
                                acts.push(Act::Confirm(None));
                            }
                        }
                        let mut c = ui.new_child(egui::UiBuilder::new().max_rect(remove).layout(Layout::centered_and_justified(egui::Direction::LeftToRight)));
                        let label = if confirming { "Remove" } else { "Remove\u{2026}" };
                        let kind = if confirming { BtnKind::Primary } else { BtnKind::Secondary };
                        if button(&mut c, label, kind, !busy) {
                            acts.push(if confirming { Act::Remove(id.clone()) } else { Act::Confirm(Some(id.clone())) });
                        }
                    }
                });
                ui.add_space(14.0);
                ui.label(theme::small("Removing turns the content off and frees its place in the library. Downloaded files stay in the cache, so installing it again is quick.", N700));
            });
        });
    }

    fn updates(&mut self, ui: &mut Ui, acts: &mut Vec<Act>) {
        let Some(ws) = &self.ws else { return };
        let ups = ws.updates();
        let total = ups.iter().map(|p| p.bytes).sum::<u64>();
        Self::toolbar_frame(ui, |ui| {
            let t = match (self.online, self.checked_at) {
                (true, Some(t)) => format!("Last checked against the online catalog {}", ago(t)),
                (true, None) => "Checking against the online catalog\u{2026}".into(),
                (false, Some(t)) => format!("Offline \u{b7} last checked {}", ago(t)),
                (false, None) => "Offline \u{b7} not checked yet".into(),
            };
            ui.label(theme::small(t, N700));
            ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                if !ups.is_empty()
                    && button(
                        ui,
                        &format!("Update all \u{b7} {}", fmt_bytes(total)),
                        BtnKind::Primary,
                        self.online,
                    )
                {
                    acts.push(Act::UpdateAll);
                }
                if button(
                    ui,
                    if self.checking_updates {
                        "Checking\u{2026}"
                    } else {
                        "Check now"
                    },
                    BtnKind::Secondary,
                    self.online && !self.checking_updates,
                ) {
                    acts.push(Act::CheckUpdates);
                }
            });
        });
        ScrollArea::vertical().auto_shrink([false, false]).show(ui, |ui| {
            Frame::new().inner_margin(Margin { left: 20, right: 20, top: 16, bottom: 24 }).show(ui, |ui| {
                ui.set_width(ui.available_width());
                if !self.online {
                    Frame::new().stroke(Stroke::new(1.0, ACCENT)).corner_radius(CornerRadius::same(4)).inner_margin(Margin::symmetric(14, 10)).show(ui, |ui| {
                        ui.set_width(ui.available_width());
                        ui.label(RichText::new("Connect to the internet to check for updates. Installed content keeps working.").size(13.0).color(A800));
                    });
                    ui.add_space(14.0);
                }
                if ups.is_empty() {
                    ui.add_space(48.0);
                    ui.vertical_centered(|ui| {
                        let (r, _) = ui.allocate_exact_size(vec2(32.0, 32.0), Sense::hover());
                        paint_icon(ui, Icon::Check, r.center(), ACCENT);
                        ui.add_space(10.0);
                        ui.label(RichText::new("Your data is up to date").size(14.0));
                    });
                } else {
                    card(ui, |ui| {
                        for (i, p) in ups.iter().enumerate() {
                            let (r, _) = row(ui, 50.0, i == 0, false);
                            ui.painter().text(r.left_center() + vec2(14.0, -9.0), Align2::LEFT_CENTER, &p.name, FontId::proportional(13.0), TEXT);
                            let from = ws.installed.get(&p.id).and_then(|i| i.version.clone()).unwrap_or_default();
                            let to = ws.update_available(&p.id).unwrap_or_default();
                            ui.painter().text(r.left_center() + vec2(14.0, 9.0), Align2::LEFT_CENTER, format!("{from} \u{2192} {to} \u{b7} {}", p.license), FontId::proportional(12.0), N700);
                            let queued = self.runner.is_queued(&p.id);
                            let btn = Rect::from_center_size(Pos2::new(r.right() - 14.0 - 50.0, r.center().y), vec2(100.0, 32.0));
                            ui.painter().text(Pos2::new(btn.left() - 14.0, r.center().y), Align2::RIGHT_CENTER, fmt_size(p.bytes), FontId::proportional(13.0), TEXT);
                            let mut child = ui.new_child(egui::UiBuilder::new().max_rect(btn).layout(Layout::centered_and_justified(egui::Direction::LeftToRight)));
                            if button(&mut child, if queued { "Queued" } else { "Update" }, BtnKind::Secondary, self.online && !queued) {
                                acts.push(Act::Update(p.id.clone()));
                            }
                        }
                    });
                }
                ui.add_space(14.0);
                ui.label(theme::small("The previous version stays in place until the new one has downloaded and its checksum matches.", N700));
            });
        });
    }

    fn services_tab(&mut self, ui: &mut Ui, acts: &mut Vec<Act>) {
        let Some(ws) = &self.ws else { return };
        Self::toolbar_frame(ui, |ui| {
            let (text, col) = match &self.health {
                Some((ok, t)) => (t.clone(), if *ok { N700 } else { A800 }),
                None => (
                    "Checks services, certificates, name resolution, disk and library files.".to_string(),
                    N700,
                ),
            };
            ui.label(theme::small(text, col));
            ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                if button(
                    ui,
                    if self.checking_health {
                        "Checking\u{2026}"
                    } else {
                        "Run health check"
                    },
                    BtnKind::Primary,
                    !self.checking_health,
                ) {
                    acts.push(Act::HealthCheck);
                }
            });
        });
        let missing: Vec<_> = ws
            .pkgs
            .iter()
            .filter(|p| {
                p.category == "services" && !ws.installed.contains_key(&p.id) && !self.runner.is_queued(&p.id)
            })
            .collect();
        ScrollArea::vertical().auto_shrink([false, false]).show(ui, |ui| {
            Frame::new()
                .inner_margin(Margin {
                    left: 20,
                    right: 20,
                    top: 16,
                    bottom: 24,
                })
                .show(ui, |ui| {
                    ui.set_width(ui.available_width());
                    if self.services.is_empty() {
                        ui.label(theme::small("No services are installed yet.", N700));
                    } else {
                        card(ui, |ui| {
                            for (i, s) in self.services.iter().enumerate() {
                                let (r, _) = row(ui, 50.0, i == 0, false);
                                let unit = s.unit.clone().unwrap_or_default();
                                let busy = self.restarting.contains(&unit);
                                let up = self.active.get(&unit).copied().unwrap_or(false);
                                ui.painter().circle_filled(
                                    r.left_center() + vec2(18.0, 0.0),
                                    4.0,
                                    if up && !busy { ACCENT } else { N500 },
                                );
                                ui.painter().text(
                                    r.left_center() + vec2(36.0, -9.0),
                                    Align2::LEFT_CENTER,
                                    &s.name,
                                    FontId::proportional(13.0),
                                    TEXT,
                                );
                                ui.painter().text(
                                    r.left_center() + vec2(36.0, 9.0),
                                    Align2::LEFT_CENTER,
                                    &s.host,
                                    FontId::proportional(12.0),
                                    N700,
                                );
                                let open = Rect::from_center_size(
                                    Pos2::new(r.right() - 14.0 - 32.0, r.center().y),
                                    vec2(64.0, 32.0),
                                );
                                let restart = Rect::from_center_size(
                                    Pos2::new(open.left() - 8.0 - 36.0, r.center().y),
                                    vec2(72.0, 32.0),
                                );
                                let status = if busy {
                                    "Restarting\u{2026}"
                                } else if up {
                                    "Running"
                                } else {
                                    "Stopped"
                                };
                                ui.painter().text(
                                    Pos2::new(restart.left() - 14.0, r.center().y),
                                    Align2::RIGHT_CENTER,
                                    status,
                                    FontId::proportional(12.0),
                                    N800,
                                );
                                let mut c =
                                    ui.new_child(egui::UiBuilder::new().max_rect(restart).layout(
                                        Layout::centered_and_justified(egui::Direction::LeftToRight),
                                    ));
                                if button(&mut c, "Restart", BtnKind::Ghost, !busy && !unit.is_empty()) {
                                    acts.push(Act::Restart(s.name.clone(), unit.clone()));
                                }
                                let mut c =
                                    ui.new_child(egui::UiBuilder::new().max_rect(open).layout(
                                        Layout::centered_and_justified(egui::Direction::LeftToRight),
                                    ));
                                if button(&mut c, "Open", BtnKind::Secondary, true) {
                                    acts.push(Act::Open(s.url.clone()));
                                }
                            }
                        });
                    }
                    ui.add_space(18.0);
                    section_title(ui, "Not installed");
                    card(ui, |ui| {
                        if missing.is_empty() {
                            let (r, _) = row(ui, 40.0, true, false);
                            ui.painter().text(
                                r.left_center() + vec2(14.0, 0.0),
                                Align2::LEFT_CENTER,
                                "Every available app is installed.",
                                FontId::proportional(13.0),
                                N700,
                            );
                        }
                        for (i, p) in missing.iter().enumerate() {
                            let (r, _) = row(ui, 50.0, i == 0, false);
                            ui.painter().text(
                                r.left_center() + vec2(14.0, -9.0),
                                Align2::LEFT_CENTER,
                                &p.name,
                                FontId::proportional(13.0),
                                TEXT,
                            );
                            let btn = Rect::from_center_size(
                                Pos2::new(r.right() - 14.0 - 50.0, r.center().y),
                                vec2(100.0, 32.0),
                            );
                            let clip = Rect::from_min_max(r.min, Pos2::new(btn.left() - 90.0, r.bottom()));
                            text_clipped(
                                ui,
                                r.left_center() + vec2(14.0, 9.0),
                                Align2::LEFT_CENTER,
                                &p.desc,
                                12.0,
                                N700,
                                clip,
                            );
                            ui.painter().text(
                                Pos2::new(btn.left() - 14.0, r.center().y),
                                Align2::RIGHT_CENTER,
                                fmt_size(p.bytes),
                                FontId::proportional(13.0),
                                TEXT,
                            );
                            let mut c = ui.new_child(
                                egui::UiBuilder::new()
                                    .max_rect(btn)
                                    .layout(Layout::centered_and_justified(egui::Direction::LeftToRight)),
                            );
                            if button(&mut c, "Install", BtnKind::Secondary, true) {
                                acts.push(Act::InstallService(p.id.clone()));
                            }
                        }
                    });
                });
        });
    }

    /// Where kit keeps its data: type a folder (an external disk, say) and optionally move
    /// what is installed. The work is `kit data-dir set`, so the window stays responsive.
    fn archive_dialog(&mut self, ctx: &egui::Context) {
        let Some(ArchiveDialog {
            req,
            error,
            id_touched,
        }) = &mut self.archive
        else {
            return;
        };
        let (mut close, mut start) = (false, false);
        #[allow(deprecated)]
        let modal = Modal::new(egui::Id::new("archive"))
            .backdrop_color(Color32::from_rgba_unmultiplied(0x2d, 0x2b, 0x2b, 82))
            .frame(
                Frame::new()
                    .fill(BG)
                    .stroke(Stroke::new(1.0, N500))
                    .corner_radius(CornerRadius::same(12)),
            );
        modal.show(ctx, |ui| {
            ui.set_width(520.0);
            ui.spacing_mut().item_spacing = vec2(8.0, 8.0);
            ui.add_space(12.0);
            ui.horizontal(|ui| {
                ui.add_space(12.0);
                ui.label(theme::heading("Archive a website", 16.0));
            });
            ui.horizontal(|ui| {
                ui.add_space(12.0);
                ui.vertical(|ui| {
                    ui.label(theme::small(
                        "Crawls the site and adds it to the library as a ZIM, for sites that have no \
                         public ZIM file. Only pages below the address you enter are fetched. \
                         Needs Podman and a connection; big sites take hours. Try a page limit of 50 first.",
                        N700,
                    ));
                    ui.label(theme::small("Address", N800));
                    let url = ui.add(
                        egui::TextEdit::singleline(&mut req.url)
                            .hint_text("https://projekt-gutenberg.org/bibliothek/")
                            .desired_width(480.0),
                    );
                    if url.changed() && !*id_touched {
                        req.id = ArchiveRequest::suggested_id(&req.url);
                    }
                    ui.label(theme::small("Title in the library", N800));
                    ui.add(
                        egui::TextEdit::singleline(&mut req.name)
                            .hint_text("Projekt Gutenberg-DE")
                            .desired_width(480.0),
                    );
                    ui.horizontal(|ui| {
                        ui.vertical(|ui| {
                            ui.label(theme::small("Id (a-z, 0-9, - and _)", N800));
                            if ui
                                .add(egui::TextEdit::singleline(&mut req.id).desired_width(230.0))
                                .changed()
                            {
                                *id_touched = true;
                            }
                        });
                        ui.vertical(|ui| {
                            ui.label(theme::small("Page limit (empty: none)", N800));
                            ui.add(egui::TextEdit::singleline(&mut req.page_limit).desired_width(230.0));
                        });
                    });
                    ui.label(theme::small("License of the content", N800));
                    ui.add(
                        egui::TextEdit::singleline(&mut req.license)
                            .hint_text("CC-BY-4.0, or: personal copy")
                            .desired_width(480.0),
                    );
                    ui.label(theme::small(
                        "Check the site's terms first. A \"personal copy\" is for you only; do not share the file.",
                        N700,
                    ));
                    if let Some(e) = error.as_ref() {
                        ui.label(theme::small(e.clone(), Color32::from_rgb(0xa8, 0x32, 0x2d)));
                    }
                });
            });
            ui.horizontal(|ui| {
                ui.add_space(12.0);
                ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                    ui.add_space(12.0);
                    if button(ui, "Start", BtnKind::Primary, true) {
                        start = true;
                    }
                    if button(ui, "Cancel", BtnKind::Ghost, true) {
                        close = true;
                    }
                });
            });
            ui.add_space(8.0);
        });
        if start {
            let taken = self.runner.is_queued(req.id.trim())
                || self
                    .ws
                    .as_ref()
                    .is_some_and(|w| w.installed.contains_key(req.id.trim()));
            match req.check() {
                _ if taken => {
                    *error = Some(format!(
                        "`{}` is already in the library or the queue; choose another id or remove it first.",
                        req.id.trim()
                    ));
                }
                Err(e) => *error = Some(e),
                Ok(()) => {
                    self.runner.enqueue_archive(req);
                    self.tab = Tab::Downloads;
                    close = true;
                }
            }
        }
        if close {
            self.archive = None;
        }
    }

    fn location(&mut self, ctx: &egui::Context) {
        let Some(loc) = &mut self.loc else {
            return;
        };
        let busy = self.runner.is_relocating();
        let can = self.runner.can_relocate();
        let used = self
            .ws
            .as_ref()
            .map_or(0, |w| w.drive_bytes.saturating_sub(w.free_bytes));
        let current = self.cfg.data_dir.clone();
        let (mut close, mut apply) = (false, false);
        #[allow(deprecated)]
        let modal = Modal::new(egui::Id::new("location"))
            .backdrop_color(Color32::from_rgba_unmultiplied(0x2d, 0x2b, 0x2b, 82))
            .frame(
                Frame::new()
                    .fill(BG)
                    .stroke(Stroke::new(1.0, N500))
                    .corner_radius(CornerRadius::same(12)),
            );
        modal.show(ctx, |ui| {
            ui.set_width(520.0);
            ui.spacing_mut().item_spacing = vec2(8.0, 8.0);
            ui.add_space(12.0);
            ui.horizontal(|ui| {
                ui.add_space(12.0);
                ui.label(theme::heading("Storage location", 16.0));
            });
            ui.horizontal(|ui| {
                ui.add_space(12.0);
                ui.vertical(|ui| {
                    ui.label(theme::small(format!("Now: {}", current.display()), N700));
                    ui.label(theme::small(
                        "Choose an empty or new folder, e.g. on an external disk. \
                         Paths with spaces or special characters do not work; \
                         make a link with a plain name to the disk instead.",
                        N700,
                    ));
                    ui.add_enabled(
                        !busy,
                        egui::TextEdit::singleline(&mut loc.text)
                            .hint_text("/run/media/you/disk/kit")
                            .desired_width(480.0),
                    );
                    ui.add_enabled(
                        !busy,
                        egui::Checkbox::new(
                            &mut loc.move_data,
                            format!("Move what is installed ({} used) to the new place", fmt_bytes(used)),
                        ),
                    );
                    if loc.move_data {
                        ui.label(theme::small(
                            "The content server and services stop while the files move; \
                             a copy to another disk can take a long time. Downloads must be paused.",
                            N700,
                        ));
                    } else {
                        ui.label(theme::small(
                            "Nothing is moved: what is installed stays where it is, new downloads go to the new place.",
                            N700,
                        ));
                    }
                    if busy {
                        let s = self.runner.relocate_status();
                        ui.label(theme::small(if s.is_empty() { "Working\u{2026}".into() } else { s }, ACCENT));
                    } else if !can {
                        ui.label(theme::small("Pause or finish the running download first.", A700));
                    }
                    if let Some(e) = &loc.error {
                        ui.label(theme::small(e.clone(), Color32::from_rgb(0xa8, 0x32, 0x2d)));
                    }
                });
            });
            ui.horizontal(|ui| {
                ui.add_space(12.0);
                ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                    ui.add_space(12.0);
                    if button(ui, if loc.move_data { "Move" } else { "Use this place" }, BtnKind::Primary, can && !busy) {
                        apply = true;
                    }
                    if button(ui, "Cancel", BtnKind::Ghost, !busy) {
                        close = true;
                    }
                });
            });
            ui.add_space(8.0);
        });
        if apply {
            match backend::check_location(&current, &loc.text) {
                Ok(path) => {
                    loc.error = None;
                    let move_data = loc.move_data;
                    self.runner.relocate(path, move_data);
                }
                Err(e) => loc.error = Some(e),
            }
        }
        if close {
            self.loc = None;
        }
    }

    fn wizard(&mut self, ctx: &egui::Context) {
        let (Some(ws), Some(w)) = (&self.ws, &mut self.wiz) else {
            return;
        };
        let wiki = ws.wikipedia();
        let pick = |l: &str, w: &Wiz| -> Option<&model::Pkg> {
            let ids = &wiki.iter().find(|(x, _)| x == l)?.1;
            ids.get(*w.ed.get(l)?)?.as_ref().and_then(|id| ws.pkg(id))
        };
        let picks: Vec<&model::Pkg> = w.langs.iter().filter_map(|l| pick(l, w)).collect();
        let to_download: u64 = picks
            .iter()
            .filter(|p| !ws.installed.contains_key(&p.id))
            .map(|p| p.bytes)
            .sum();
        let others: u64 = ws.sum(ws.sel.iter().filter(|id| !id.starts_with("wikipedia_")));
        let reclaim = ws.sum(
            ws.installed
                .keys()
                .filter(|id| id.starts_with("wikipedia_") && !picks.iter().any(|p| &p.id == *id)),
        );
        let free_after = i128::from(ws.free_bytes) + i128::from(reclaim) - i128::from(to_download);
        let _ = others;
        let steps = ["Languages", "Edition", "Summary"];
        let (mut close, mut finish) = (false, false);

        #[allow(deprecated)]
        let modal = Modal::new(egui::Id::new("wizard"))
            .backdrop_color(Color32::from_rgba_unmultiplied(0x2d, 0x2b, 0x2b, 82))
            .frame(
                Frame::new()
                    .fill(BG)
                    .stroke(Stroke::new(1.0, N500))
                    .corner_radius(CornerRadius::same(12)),
            );
        modal.show(ctx, |ui| {
            ui.set_width(560.0);
            ui.spacing_mut().item_spacing = vec2(8.0, 6.0);
            ui.horizontal(|ui| {
                ui.add_space(12.0);
                ui.label(theme::heading("Set up Wikipedia", 16.0));
                ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                    ui.add_space(8.0);
                    if button(ui, "Close", BtnKind::Ghost, true) {
                        close = true;
                    }
                });
            });
            let top = ui.cursor().top();
            ui.painter().hline(ui.max_rect().x_range(), top, Stroke::new(1.0, DIVIDER));
            ui.add_space(12.0);
            ui.horizontal(|ui| {
                ui.add_space(16.0);
                let cw = (ui.available_width() - 16.0 - 16.0) / 3.0;
                for (i, s) in steps.iter().enumerate() {
                    ui.vertical(|ui| {
                        ui.set_width(cw);
                        let (r, _) = ui.allocate_exact_size(vec2(cw, 2.0), Sense::hover());
                        ui.painter().rect_filled(r, 0.0, if i <= w.step { ACCENT } else { DIVIDER });
                        let c = if i == w.step { A800 } else if i < w.step { TEXT } else { N600 };
                        ui.label(theme::small(format!("{}. {s}", i + 1), c).size(11.0));
                    });
                }
            });
            Frame::new().inner_margin(Margin::symmetric(20, 14)).show(ui, |ui| {
                ui.set_width(ui.available_width());
                let (title, intro) = match w.step {
                    0 => ("Which languages do you need?", "Pick every language someone in your household reads. Each one is a separate download."),
                    1 => ("How much of Wikipedia?", "Choose one edition for every language, or set it per language below."),
                    _ => ("Ready to add to your plan", "These packages replace the current Wikipedia selection in your plan. Nothing downloads until you press Download and install."),
                };
                ui.label(theme::heading(title, 24.0));
                ui.label(theme::small(intro, N800).size(13.0));
                ui.add_space(8.0);
                match w.step {
                    0 => {
                        ui.horizontal_wrapped(|ui| {
                            for (l, ids) in &wiki {
                                let on = w.langs.contains(l);
                                let sizes: Vec<u64> = ids.iter().flatten().filter_map(|id| ws.pkg(id)).map(|p| p.bytes).collect();
                                let range = match (sizes.iter().min(), sizes.iter().max()) {
                                    (Some(a), Some(b)) if a != b => format!("{} to {}", fmt_bytes(*a), fmt_bytes(*b)),
                                    (Some(a), _) => fmt_bytes(*a),
                                    _ => String::new(),
                                };
                                let (r, resp) = ui.allocate_exact_size(vec2((ui.available_width() - 8.0).max(200.0) / 2.0, 40.0), Sense::click());
                                ui.painter().rect(r, 4.0, if resp.hovered() { Color32::from_rgba_unmultiplied(0xb6, 0x82, 0x35, 15) } else { Color32::TRANSPARENT }, Stroke::new(1.0, if on { ACCENT } else { N500 }), StrokeKind::Inside);
                                checkbox(ui, r.left_center() + vec2(22.0, 0.0), on);
                                ui.painter().text(r.left_center() + vec2(40.0, 0.0), Align2::LEFT_CENTER, language_name(l), FontId::proportional(13.0), TEXT);
                                ui.painter().text(r.right_center() - vec2(12.0, 0.0), Align2::RIGHT_CENTER, range, FontId::proportional(11.0), N700);
                                if resp.on_hover_cursor(egui::CursorIcon::PointingHand).clicked() && !w.langs.remove(l) {
                                    w.langs.insert(l.clone());
                                }
                            }
                        });
                    }
                    1 => {
                        let langs: Vec<String> = w.langs.iter().cloned().collect();
                        let all_ed = langs.first().and_then(|l| w.ed.get(l)).copied().filter(|e| langs.iter().all(|l| w.ed.get(l) == Some(e)));
                        for (i, (_, name, _, desc)) in model::EDITIONS.iter().enumerate() {
                            let size: u64 = langs
                                .iter()
                                .filter_map(|l| wiki.iter().find(|(x, _)| x == l))
                                .filter_map(|(_, ids)| ids[i].as_ref().and_then(|id| ws.pkg(id)))
                                .map(|p| p.bytes)
                                .sum();
                            let available = langs.iter().all(|l| wiki.iter().find(|(x, _)| x == l).is_some_and(|(_, ids)| ids[i].is_some()));
                            let on = all_ed == Some(i);
                            let (r, resp) = ui.allocate_exact_size(vec2(ui.available_width(), 64.0), Sense::click());
                            ui.painter().rect(r, 4.0, if on { A100 } else { Color32::TRANSPARENT }, Stroke::new(1.0, if on { ACCENT } else { DIVIDER }), StrokeKind::Inside);
                            let c = r.left_top() + vec2(26.0, 22.0);
                            ui.painter().circle_stroke(c, 8.0, Stroke::new(1.0, if on { ACCENT } else { N500 }));
                            if on {
                                ui.painter().circle_filled(c, 4.0, ACCENT);
                            }
                            let a = if available { 1.0 } else { 0.4 };
                            ui.painter().text(r.left_top() + vec2(44.0, 14.0), Align2::LEFT_CENTER, *name, FontId::proportional(14.0), TEXT.gamma_multiply(a));
                            let clip = Rect::from_min_max(r.min, r.max - vec2(80.0, 0.0));
                            ui.painter().with_clip_rect(clip).text(r.left_top() + vec2(44.0, 36.0), Align2::LEFT_CENTER, *desc, FontId::proportional(11.0), N800.gamma_multiply(a));
                            ui.painter().text(r.right_top() + vec2(-14.0, 14.0), Align2::RIGHT_CENTER, fmt_bytes(size), FontId::proportional(13.0), TEXT.gamma_multiply(a));
                            if available && resp.on_hover_cursor(egui::CursorIcon::PointingHand).clicked() {
                                for l in &langs {
                                    w.ed.insert(l.clone(), i);
                                }
                            }
                        }
                        if langs.len() > 1 {
                            ui.add_space(6.0);
                            ui.label(RichText::new("PER LANGUAGE").size(10.0).color(N700));
                            card(ui, |ui| {
                                for (i, l) in langs.iter().enumerate() {
                                    let (r, _) = row(ui, 42.0, i == 0, false);
                                    ui.painter().text(r.left_center() + vec2(12.0, 0.0), Align2::LEFT_CENTER, language_name(l), FontId::proportional(13.0), TEXT);
                                    if let Some(p) = pick(l, w) {
                                        ui.painter().text(r.right_center() - vec2(12.0, 0.0), Align2::RIGHT_CENTER, fmt_bytes(p.bytes), FontId::proportional(12.0), TEXT);
                                    }
                                    let ids = wiki.iter().find(|(x, _)| x == l).map(|(_, ids)| ids.clone()).unwrap_or_default();
                                    for (j, (_, _, short, _)) in model::EDITIONS.iter().enumerate() {
                                        let b = Rect::from_min_size(Pos2::new(r.right() - 64.0 - 3.0 * 62.0 + j as f32 * 62.0, r.center().y - 13.0), vec2(62.0, 26.0));
                                        let has = ids[j].is_some();
                                        let sel = w.ed.get(l) == Some(&j);
                                        let resp = ui.interact(b, ui.scope_id().with(("ed", l, j)), if has { Sense::click() } else { Sense::hover() });
                                        ui.painter().rect_filled(b.shrink(0.5), 0.0, if sel { A100 } else if resp.hovered() && has { N200 } else { Color32::TRANSPARENT });
                                        ui.painter().rect_stroke(b, 0.0, Stroke::new(1.0, DIVIDER), StrokeKind::Inside);
                                        ui.painter().text(b.center(), Align2::CENTER_CENTER, *short, FontId::proportional(12.0), if sel { A800 } else if has { TEXT } else { N500 });
                                        if has && resp.clicked() {
                                            w.ed.insert(l.clone(), j);
                                        }
                                    }
                                }
                            });
                        }
                    }
                    _ => {
                        card(ui, |ui| {
                            for (i, p) in picks.iter().enumerate() {
                                let (r, _) = row(ui, 46.0, i == 0, false);
                                ui.painter().text(r.left_center() + vec2(14.0, -9.0), Align2::LEFT_CENTER, &p.name, FontId::proportional(13.0), TEXT);
                                let ed = model::EDITIONS.iter().find(|(e, ..)| p.id.ends_with(&format!("_{e}"))).map_or("", |e| e.1);
                                ui.painter().text(r.left_center() + vec2(14.0, 9.0), Align2::LEFT_CENTER, ed, FontId::proportional(12.0), N700);
                                ui.painter().text(r.right_center() - vec2(14.0, 0.0), Align2::RIGHT_CENTER, fmt_bytes(p.bytes), FontId::proportional(13.0), TEXT);
                                let st = if ws.installed.contains_key(&p.id) { "Installed" } else { "Will download" };
                                ui.painter().text(r.right_center() - vec2(14.0 + 70.0, 0.0), Align2::RIGHT_CENTER, st, FontId::proportional(11.0), N700);
                            }
                        });
                        ui.add_space(6.0);
                        ui.label(RichText::new(format!("{} to download", fmt_bytes(to_download))).size(14.0));
                        let (msg, col) = if free_after >= 0 {
                            (format!("{} free on the drive afterwards", fmt_bytes(free_after as u64)), N700)
                        } else {
                            (format!("Does not fit: {} short. Choose a smaller edition.", fmt_bytes(free_after.unsigned_abs() as u64)), A800)
                        };
                        ui.label(theme::small(msg, col));
                    }
                }
            });
            let r = ui.cursor();
            ui.painter().rect_filled(Rect::from_min_size(r.min, vec2(ui.available_width(), 54.0)), CornerRadius { nw: 0, ne: 0, sw: 12, se: 12 }, SURFACE);
            ui.painter().hline(ui.max_rect().x_range(), r.top(), Stroke::new(1.0, DIVIDER));
            ui.horizontal(|ui| {
                ui.set_height(54.0);
                ui.add_space(16.0);
                let foot = if w.step == 0 {
                    format!("{} {}", w.langs.len(), if w.langs.len() == 1 { "language" } else { "languages" })
                } else {
                    format!("{} selected", fmt_bytes(picks.iter().map(|p| p.bytes).sum()))
                };
                ui.label(theme::small(foot, N700));
                ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                    ui.add_space(10.0);
                    let last = w.step == 2;
                    let can = (w.step != 0 || !w.langs.is_empty()) && !(last && free_after < 0) && !picks.is_empty();
                    if button(ui, if last { "Add to plan" } else { "Continue" }, BtnKind::Primary, can) {
                        if last {
                            finish = true;
                        } else {
                            w.step += 1;
                        }
                    }
                    if w.step > 0 {
                        if button(ui, "Back", BtnKind::Secondary, true) {
                            w.step -= 1;
                        }
                    } else if button(ui, "Cancel", BtnKind::Secondary, true) {
                        close = true;
                    }
                });
            });
        });
        if finish {
            self.finish_wizard();
        } else if close {
            self.wiz = None;
        }
    }
}
