// RasterView - classic native photo viewer (eframe/egui)
#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

use eframe::egui::{
    self, Color32, CursorIcon, Key, Rect, Rounding, Sense, TextureHandle, TextureOptions, Vec2,
    ViewportCommand,
};
use image::{DynamicImage, ImageDecoder, RgbaImage};
use std::cmp::Ordering;
use std::iter::Peekable;
use std::path::{Path, PathBuf};
use std::str::Chars;
use std::sync::mpsc::{channel, Receiver, Sender};

const EXTENSIONS: &[&str] = &[
    "png", "jpg", "jpeg", "jpe", "gif", "bmp", "webp", "tif", "tiff", "ico", "tga",
];
const MAX_TEX_SIDE: u32 = 8192; // larger images are downscaled for display
const MIN_ZOOM: f32 = 0.01;
const MAX_ZOOM: f32 = 64.0;

struct Decoded {
    rgba: RgbaImage,
    orig_dims: (u32, u32),
}

struct Msg {
    id: u64,
    result: Result<Decoded, String>,
}

struct Loaded {
    rgba: RgbaImage,
    orig_dims: (u32, u32),
    tex: TextureHandle,
}

enum Action {
    Open,
    Next,
    Prev,
    First,
    Last,
    Fit,
    Actual,
    ZoomIn,
    ZoomOut,
    RotCw,
    RotCcw,
    Fullscreen,
    About,
    Quit,
}

struct RasterView {
    files: Vec<PathBuf>,
    index: usize,
    loaded: Option<Loaded>,
    error: Option<String>,
    loading: bool,
    file_size: u64,
    req_id: u64,
    tx: Sender<Msg>,
    rx: Receiver<Msg>,
    fit: bool,
    zoom: f32,
    offset: Vec2,
    fullscreen: bool,
    show_about: bool,
    title: String,
}

fn main() -> eframe::Result<()> {
    let initial = std::env::args_os().nth(1).map(PathBuf::from);
    let options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_title("RasterView")
            .with_inner_size([1100.0, 750.0])
            .with_min_inner_size([480.0, 320.0])
            .with_drag_and_drop(true),
        ..Default::default()
    };
    eframe::run_native(
        "RasterView",
        options,
        Box::new(move |cc| Ok(Box::new(RasterView::new(cc, initial)))),
    )
}

impl RasterView {
    fn new(cc: &eframe::CreationContext<'_>, initial: Option<PathBuf>) -> Self {
        apply_classic_style(&cc.egui_ctx);
        let (tx, rx) = channel();
        let mut app = Self {
            files: Vec::new(),
            index: 0,
            loaded: None,
            error: None,
            loading: false,
            file_size: 0,
            req_id: 0,
            tx,
            rx,
            fit: true,
            zoom: 1.0,
            offset: Vec2::ZERO,
            fullscreen: false,
            show_about: false,
            title: String::new(),
        };
        if let Some(p) = initial {
            app.open_path(&cc.egui_ctx, &p);
        }
        app
    }

    fn current_path(&self) -> Option<&PathBuf> {
        self.files.get(self.index)
    }

    /// Open a file (or the first image of a directory) and scan its folder.
    fn open_path(&mut self, ctx: &egui::Context, path: &Path) {
        let (dir, target): (PathBuf, Option<PathBuf>) = if path.is_dir() {
            (path.to_path_buf(), None)
        } else {
            let d = match path.parent() {
                Some(p) if !p.as_os_str().is_empty() => p.to_path_buf(),
                _ => PathBuf::from("."),
            };
            (d, Some(path.to_path_buf()))
        };
        let mut files: Vec<PathBuf> = std::fs::read_dir(&dir)
            .map(|rd| {
                rd.filter_map(|e| e.ok())
                    .map(|e| e.path())
                    .filter(|p| p.is_file() && is_image(p))
                    .collect()
            })
            .unwrap_or_default();
        if let Some(t) = &target {
            if !files.iter().any(|f| same_file(f, t)) {
                files.push(t.clone()); // unsupported extension: still try, show error
            }
        }
        files.sort_by(|a, b| natural_cmp(&name_lower(a), &name_lower(b)));
        if files.is_empty() {
            self.files.clear();
            self.loaded = None;
            self.error = Some("No supported images found in this folder.".into());
            return;
        }
        let idx = target
            .as_ref()
            .and_then(|t| files.iter().position(|f| same_file(f, t)))
            .unwrap_or(0);
        self.files = files;
        self.goto(ctx, idx);
    }

    fn goto(&mut self, ctx: &egui::Context, idx: usize) {
        if self.files.is_empty() {
            return;
        }
        self.index = idx.min(self.files.len() - 1);
        let path = self.files[self.index].clone();
        self.file_size = std::fs::metadata(&path).map(|m| m.len()).unwrap_or(0);
        self.req_id += 1;
        self.loading = true;
        self.error = None;
        let (id, tx, ctx) = (self.req_id, self.tx.clone(), ctx.clone());
        std::thread::spawn(move || {
            let result = std::panic::catch_unwind(|| decode(&path))
                .unwrap_or_else(|_| Err("Decoder crashed on this file.".into()));
            let _ = tx.send(Msg { id, result });
            ctx.request_repaint();
        });
    }

    fn step(&mut self, ctx: &egui::Context, delta: isize) {
        let n = self.files.len() as isize;
        if n > 1 {
            let i = (self.index as isize + delta).rem_euclid(n) as usize;
            self.goto(ctx, i);
        }
    }

    fn poll_loader(&mut self, ctx: &egui::Context) {
        while let Ok(msg) = self.rx.try_recv() {
            if msg.id != self.req_id {
                continue; // stale result from a previous navigation
            }
            self.loading = false;
            match msg.result {
                Ok(d) => {
                    let tex = ctx.load_texture("image", to_color_image(&d.rgba), TextureOptions::LINEAR);
                    self.loaded = Some(Loaded { rgba: d.rgba, orig_dims: d.orig_dims, tex });
                    self.fit = true;
                    self.offset = Vec2::ZERO;
                }
                Err(e) => {
                    self.loaded = None;
                    self.error = Some(e);
                }
            }
        }
    }

    fn rotate(&mut self, cw: bool) {
        if let Some(l) = &mut self.loaded {
            l.rgba = if cw {
                image::imageops::rotate90(&l.rgba)
            } else {
                image::imageops::rotate270(&l.rgba)
            };
            l.orig_dims = (l.orig_dims.1, l.orig_dims.0);
            l.tex.set(to_color_image(&l.rgba), TextureOptions::LINEAR);
            self.offset = Vec2::ZERO;
        }
    }

    fn zoom_about(&mut self, anchor: Vec2, new_zoom: f32) {
        let nz = new_zoom.clamp(MIN_ZOOM, MAX_ZOOM);
        // anchor = cursor position relative to viewport centre
        self.offset = anchor - (anchor - self.offset) * (nz / self.zoom);
        self.zoom = nz;
        self.fit = false;
    }

    fn actual_zoom(ctx: &egui::Context) -> f32 {
        1.0 / ctx.pixels_per_point()
    }

    fn open_dialog(&mut self, ctx: &egui::Context) {
        let exts: Vec<&str> = EXTENSIONS.to_vec();
        if let Some(p) = rfd::FileDialog::new()
            .add_filter("Images", &exts)
            .add_filter("All files", &["*"])
            .pick_file()
        {
            self.open_path(ctx, &p);
        }
    }

    fn run(&mut self, ctx: &egui::Context, a: Action) {
        match a {
            Action::Open => self.open_dialog(ctx),
            Action::Next => self.step(ctx, 1),
            Action::Prev => self.step(ctx, -1),
            Action::First => self.goto(ctx, 0),
            Action::Last => self.goto(ctx, self.files.len().saturating_sub(1)),
            Action::Fit => {
                self.fit = true;
                self.offset = Vec2::ZERO;
            }
            Action::Actual => {
                self.fit = false;
                self.zoom = Self::actual_zoom(ctx);
                self.offset = Vec2::ZERO;
            }
            Action::ZoomIn => {
                let z = self.zoom * 1.25;
                self.zoom_about(Vec2::ZERO, z);
            }
            Action::ZoomOut => {
                let z = self.zoom / 1.25;
                self.zoom_about(Vec2::ZERO, z);
            }
            Action::RotCw => self.rotate(true),
            Action::RotCcw => self.rotate(false),
            Action::Fullscreen => {
                self.fullscreen = !self.fullscreen;
                ctx.send_viewport_cmd(ViewportCommand::Fullscreen(self.fullscreen));
            }
            Action::About => self.show_about = true,
            Action::Quit => ctx.send_viewport_cmd(ViewportCommand::Close),
        }
    }

    fn draw_viewer(&mut self, ui: &mut egui::Ui) {
        let ctx = ui.ctx().clone();
        let (rect, resp) = ui.allocate_exact_size(ui.available_size(), Sense::click_and_drag());
        let painter = ui.painter_at(rect);
        let info = self
            .loaded
            .as_ref()
            .map(|l| (l.tex.id(), l.rgba.width() as f32, l.rgba.height() as f32));

        let Some((tid, w, h)) = info else {
            let (msg, color) = if let Some(e) = &self.error {
                (format!("Cannot display image\n\n{e}"), Color32::from_rgb(220, 120, 110))
            } else if self.loading {
                ("Loading...".to_string(), Color32::GRAY)
            } else {
                ("Open an image (Ctrl+O) or drop a file here.".to_string(), Color32::GRAY)
            };
            painter.text(rect.center(), egui::Align2::CENTER_CENTER, msg,
                egui::FontId::proportional(14.0), color);
            return;
        };

        if self.fit {
            self.zoom = (rect.width() / w).min(rect.height() / h).clamp(MIN_ZOOM, MAX_ZOOM);
            self.offset = Vec2::ZERO;
        }
        if resp.hovered() {
            let s = ui.input(|i| i.smooth_scroll_delta.y);
            if s != 0.0 {
                if let Some(p) = resp.hover_pos() {
                    let z = self.zoom * (s * 0.0025).exp();
                    self.zoom_about(p - rect.center(), z);
                }
            }
        }
        if resp.dragged() && !self.fit {
            self.offset += resp.drag_delta();
            ctx.set_cursor_icon(CursorIcon::Grabbing);
        }
        if resp.double_clicked() {
            let a = if self.fit { Action::Actual } else { Action::Fit };
            self.run(&ctx, a);
        }
        // keep part of the image reachable
        let size = Vec2::new(w, h) * self.zoom;
        let maxo = ((size + rect.size()) * 0.5 - Vec2::splat(32.0)).max(Vec2::ZERO);
        self.offset.x = self.offset.x.clamp(-maxo.x, maxo.x);
        self.offset.y = self.offset.y.clamp(-maxo.y, maxo.y);

        let min = (rect.center() + self.offset - size * 0.5).round(); // pixel snap
        let img_rect = Rect::from_min_size(min, size);
        painter.image(tid, img_rect,
            Rect::from_min_max(egui::pos2(0.0, 0.0), egui::pos2(1.0, 1.0)), Color32::WHITE);
    }
}

impl eframe::App for RasterView {
    fn update(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        self.poll_loader(ctx);

        // drag & drop
        let dropped = ctx.input(|i| i.raw.dropped_files.iter().find_map(|f| f.path.clone()));
        if let Some(p) = dropped {
            self.open_path(ctx, &p);
        }

        // keyboard
        let mut actions: Vec<Action> = Vec::new();
        ctx.input(|i| {
            let ctrl = i.modifiers.command;
            if ctrl && i.key_pressed(Key::O) { actions.push(Action::Open); }
            if i.key_pressed(Key::ArrowRight) || i.key_pressed(Key::PageDown) { actions.push(Action::Next); }
            if i.key_pressed(Key::ArrowLeft) || i.key_pressed(Key::PageUp) { actions.push(Action::Prev); }
            if i.key_pressed(Key::Home) { actions.push(Action::First); }
            if i.key_pressed(Key::End) { actions.push(Action::Last); }
            if !ctrl && i.key_pressed(Key::F) { actions.push(Action::Fit); }
            if i.key_pressed(Key::Num1) { actions.push(Action::Actual); }
            if i.key_pressed(Key::Plus) || i.key_pressed(Key::Equals) { actions.push(Action::ZoomIn); }
            if i.key_pressed(Key::Minus) { actions.push(Action::ZoomOut); }
            if !ctrl && i.key_pressed(Key::R) { actions.push(Action::RotCw); }
            if !ctrl && i.key_pressed(Key::L) { actions.push(Action::RotCcw); }
            if i.key_pressed(Key::F11) { actions.push(Action::Fullscreen); }
            if i.key_pressed(Key::Escape) && self.fullscreen { actions.push(Action::Fullscreen); }
        });

        // menu bar
        egui::TopBottomPanel::top("menu").show(ctx, |ui| {
            egui::menu::bar(ui, |ui| {
                let mut item = |ui: &mut egui::Ui, label: &str, sc: &str, a: Action| {
                    if ui.add(egui::Button::new(label).shortcut_text(sc)).clicked() {
                        actions.push(a);
                        ui.close_menu();
                    }
                };
                ui.menu_button("File", |ui| {
                    item(ui, "Open...", "Ctrl+O", Action::Open);
                    ui.separator();
                    item(ui, "Previous", "Left", Action::Prev);
                    item(ui, "Next", "Right", Action::Next);
                    item(ui, "First", "Home", Action::First);
                    item(ui, "Last", "End", Action::Last);
                    ui.separator();
                    item(ui, "Exit", "", Action::Quit);
                });
                ui.menu_button("View", |ui| {
                    item(ui, "Fit to window", "F", Action::Fit);
                    item(ui, "Actual size (1:1)", "1", Action::Actual);
                    item(ui, "Zoom in", "+", Action::ZoomIn);
                    item(ui, "Zoom out", "-", Action::ZoomOut);
                    ui.separator();
                    item(ui, "Rotate clockwise", "R", Action::RotCw);
                    item(ui, "Rotate counter-clockwise", "L", Action::RotCcw);
                    ui.separator();
                    item(ui, "Full screen", "F11", Action::Fullscreen);
                });
                ui.menu_button("Help", |ui| {
                    item(ui, "About RasterView", "", Action::About);
                });
            });
        });

        // status bar
        egui::TopBottomPanel::bottom("status").show(ctx, |ui| {
            ui.horizontal(|ui| {
                match self.current_path() {
                    Some(p) => {
                        ui.label(p.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default());
                        ui.separator();
                        if let Some(l) = &self.loaded {
                            ui.label(format!("{}x{}", l.orig_dims.0, l.orig_dims.1));
                            ui.separator();
                        }
                        ui.label(format_size(self.file_size));
                        ui.separator();
                        ui.label(format!("{:.0}%", self.zoom * ctx.pixels_per_point() * 100.0));
                        ui.separator();
                        ui.label(format!("[{} / {}]", self.index + 1, self.files.len()));
                    }
                    None => { ui.label("No image"); }
                }
            });
        });

        egui::CentralPanel::default()
            .frame(egui::Frame::none().fill(Color32::from_gray(0x24)))
            .show(ctx, |ui| self.draw_viewer(ui));

        if self.show_about {
            egui::Window::new("About RasterView")
                .collapsible(false)
                .resizable(false)
                .open(&mut self.show_about)
                .show(ctx, |ui| {
                    ui.label(format!("RasterView {}", env!("CARGO_PKG_VERSION")));
                    ui.label("A small, fast, native image viewer.");
                    ui.add_space(4.0);
                    ui.label("Arrows / PgUp / PgDn: browse    R / L: rotate");
                    ui.label("F: fit    1: actual size    +/-: zoom    F11: fullscreen");
                    ui.label("Wheel: zoom at cursor    Drag: pan    Double-click: fit / 1:1");
                });
        }

        for a in actions {
            self.run(ctx, a);
        }

        // window title
        let title = match self.current_path() {
            Some(p) => format!("{} - RasterView", p.file_name().unwrap_or_default().to_string_lossy()),
            None => "RasterView".to_string(),
        };
        if title != self.title {
            ctx.send_viewport_cmd(ViewportCommand::Title(title.clone()));
            self.title = title;
        }
    }
}

// ---------- helpers ----------

fn decode(path: &Path) -> Result<Decoded, String> {
    let reader = image::ImageReader::open(path)
        .map_err(|e| format!("Cannot open file: {e}"))?
        .with_guessed_format()
        .map_err(|e| format!("Cannot read file: {e}"))?;
    let mut decoder = reader
        .into_decoder()
        .map_err(|e| format!("Unsupported or corrupted image: {e}"))?;
    let orientation = decoder.orientation().ok();
    let mut img = DynamicImage::from_decoder(decoder)
        .map_err(|e| format!("Unsupported or corrupted image: {e}"))?;
    if let Some(o) = orientation {
        img.apply_orientation(o); // honour EXIF rotation
    }
    let orig_dims = (img.width(), img.height());
    if img.width() > MAX_TEX_SIDE || img.height() > MAX_TEX_SIDE {
        img = img.resize(MAX_TEX_SIDE, MAX_TEX_SIDE, image::imageops::FilterType::Triangle);
    }
    Ok(Decoded { rgba: img.to_rgba8(), orig_dims })
}

fn to_color_image(rgba: &RgbaImage) -> egui::ColorImage {
    egui::ColorImage::from_rgba_unmultiplied(
        [rgba.width() as usize, rgba.height() as usize],
        rgba.as_raw(),
    )
}

fn is_image(p: &Path) -> bool {
    p.extension()
        .and_then(|e| e.to_str())
        .map(|e| EXTENSIONS.contains(&e.to_ascii_lowercase().as_str()))
        .unwrap_or(false)
}

fn same_file(a: &Path, b: &Path) -> bool {
    a.file_name().map(|n| n.to_ascii_lowercase_lossy()) == b.file_name().map(|n| n.to_ascii_lowercase_lossy())
}

trait LowerLossy {
    fn to_ascii_lowercase_lossy(&self) -> String;
}
impl LowerLossy for std::ffi::OsStr {
    fn to_ascii_lowercase_lossy(&self) -> String {
        self.to_string_lossy().to_lowercase()
    }
}

fn name_lower(p: &Path) -> String {
    p.file_name().map(|n| n.to_string_lossy().to_lowercase()).unwrap_or_default()
}

fn take_num(it: &mut Peekable<Chars>) -> u128 {
    let mut n: u128 = 0;
    while let Some(d) = it.peek().and_then(|c| c.to_digit(10)) {
        n = n.saturating_mul(10).saturating_add(d as u128);
        it.next();
    }
    n
}

/// "img2" sorts before "img10".
fn natural_cmp(a: &str, b: &str) -> Ordering {
    let (mut a, mut b) = (a.chars().peekable(), b.chars().peekable());
    loop {
        match (a.peek().copied(), b.peek().copied()) {
            (None, None) => return Ordering::Equal,
            (None, _) => return Ordering::Less,
            (_, None) => return Ordering::Greater,
            (Some(x), Some(y)) if x.is_ascii_digit() && y.is_ascii_digit() => {
                let o = take_num(&mut a).cmp(&take_num(&mut b));
                if o != Ordering::Equal { return o; }
            }
            (Some(x), Some(y)) => {
                a.next();
                b.next();
                let o = x.cmp(&y);
                if o != Ordering::Equal { return o; }
            }
        }
    }
}

fn format_size(bytes: u64) -> String {
    const KB: f64 = 1024.0;
    let b = bytes as f64;
    if b >= KB * KB { format!("{:.2} MB", b / (KB * KB)) }
    else if b >= KB { format!("{:.1} KB", b / KB) }
    else { format!("{bytes} B") }
}

fn apply_classic_style(ctx: &egui::Context) {
    let mut style = (*ctx.style()).clone();
    let v = &mut style.visuals;
    v.panel_fill = Color32::from_gray(0x33);
    v.window_fill = Color32::from_gray(0x3a);
    v.extreme_bg_color = Color32::from_gray(0x24);
    v.window_rounding = Rounding::ZERO;
    v.menu_rounding = Rounding::ZERO;
    v.window_shadow = egui::Shadow::NONE;
    v.popup_shadow = egui::Shadow::NONE;
    for w in [
        &mut v.widgets.noninteractive,
        &mut v.widgets.inactive,
        &mut v.widgets.hovered,
        &mut v.widgets.active,
        &mut v.widgets.open,
    ] {
        w.rounding = Rounding::ZERO;
    }
    style.spacing.item_spacing = Vec2::new(6.0, 3.0);
    style.spacing.button_padding = Vec2::new(6.0, 2.0);
    ctx.set_style(style);
}
