use std::collections::{BTreeSet, HashMap};
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::Ordering;
use std::sync::mpsc::{Receiver, Sender, channel};
use std::time::{Duration, Instant};

use eframe::egui::{self, Color32, RichText, TextureHandle, TextureOptions, Vec2};
use openfuji::camera::{ConnectOptions, Mode, default_client_name};
use openfuji::dataset::{ObjectInfo, PropDesc, PropForm, PropValue};
use openfuji::ptp::{self, prop};

use crate::worker::{self, Cmd, Msg, Shared};

#[derive(PartialEq, Clone, Copy)]
enum Tab {
    Remote,
    Gallery,
}

enum Conn {
    Disconnected,
    Connecting,
    Connected {
        camera: String,
        model: String,
        firmware: String,
        mode: Mode,
    },
}

pub struct App {
    tx: Sender<Cmd>,
    rx: Receiver<Msg>,
    shared: Arc<Shared>,

    ip: String,
    name: String,
    tab: Tab,
    conn: Conn,
    error: Option<String>,
    log: Vec<String>,
    show_log: bool,
    use_ble: bool,
    paired: Option<String>,
    busy_pairing: bool,
    confirm_repair: bool,
    /// Whether the paired camera was seen advertising over Bluetooth.
    presence: Presence,
    probe: Option<Receiver<Presence>>,
    probe_cancel: Arc<std::sync::atomic::AtomicBool>,
    next_probe: Instant,

    // remote
    live: Option<TextureHandle>,
    fps: (Instant, u64, f32),
    props: Vec<PropDesc>,
    af: bool,

    // gallery
    objects: Vec<(u32, ObjectInfo)>,
    thumbs: HashMap<u32, TextureHandle>,
    selected: BTreeSet<u32>,
    download_dir: String,
    progress: Option<(String, f32)>,
    status: String,
}

#[derive(Clone, Copy, PartialEq)]
enum Presence {
    Unknown,
    Nearby,
    NotFound,
}

/// How long one Bluetooth presence scan lasts, and the pause between scans.
const PROBE_SCAN: Duration = Duration::from_secs(6);
const PROBE_EVERY: Duration = Duration::from_secs(15);

fn default_download_dir() -> String {
    let home = std::env::var("HOME").unwrap_or_else(|_| ".".into());
    let pics = PathBuf::from(&home).join("Pictures");
    let base = if pics.is_dir() {
        pics
    } else {
        PathBuf::from(home)
    };
    base.join("openFuji").display().to_string()
}

impl App {
    pub fn new(cc: &eframe::CreationContext<'_>, ip: Option<String>, gallery: bool) -> Self {
        let auto_connect = ip.is_some();
        let (tx, worker_rx) = channel();
        let (worker_tx, rx) = channel();
        let shared = Arc::new(Shared::default());
        worker::spawn(worker_rx, worker_tx, cc.egui_ctx.clone(), shared.clone());
        let mut app = App {
            tx,
            rx,
            shared,
            ip: ip.unwrap_or_else(|| ptp::DEFAULT_CAMERA_IP.into()),
            name: default_client_name(),
            tab: Tab::Remote,
            conn: Conn::Disconnected,
            error: None,
            log: vec![],
            show_log: false,
            use_ble: openfuji::link::last_pairing().is_some(),
            paired: openfuji::link::last_pairing()
                .map(|p| if p.name.is_empty() { p.address } else { p.name }),
            busy_pairing: false,
            confirm_repair: false,
            presence: Presence::Unknown,
            probe: None,
            probe_cancel: Arc::default(),
            next_probe: Instant::now(),
            live: None,
            fps: (Instant::now(), 0, 0.0),
            props: vec![],
            af: true,
            objects: vec![],
            thumbs: HashMap::new(),
            selected: BTreeSet::new(),
            download_dir: default_download_dir(),
            progress: None,
            status: String::new(),
        };
        if gallery {
            app.tab = Tab::Gallery;
        }
        if auto_connect {
            app.connect();
        }
        app
    }

    fn mode_for(tab: Tab) -> Mode {
        match tab {
            Tab::Remote => Mode::Remote,
            Tab::Gallery => Mode::PhotoView,
        }
    }

    fn connect(&mut self) {
        let ip = match self.ip.trim().parse() {
            Ok(ip) => ip,
            Err(_) => {
                self.error = Some(format!("“{}” is not a valid IP address", self.ip));
                return;
            }
        };
        let opts = ConnectOptions {
            ip,
            name: self.name.clone(),
            ..Default::default()
        };
        self.error = None;
        self.props.clear();
        self.objects.clear();
        self.thumbs.clear();
        self.selected.clear();
        self.live = None;
        self.cancel_probe();
        let _ = self.tx.send(Cmd::Connect {
            opts,
            mode: Self::mode_for(self.tab),
            ble: self.use_ble,
        });
    }

    fn push_log(&mut self, s: String) {
        self.log.push(s);
        if self.log.len() > 500 {
            self.log.drain(..100);
        }
    }

    fn drain(&mut self, ctx: &egui::Context) {
        while let Ok(m) = self.rx.try_recv() {
            match m {
                Msg::Connecting => self.conn = Conn::Connecting,
                Msg::Status(s) => self.status = s,
                Msg::Paired(name) => {
                    self.busy_pairing = false;
                    self.status = format!("Paired with {name}. Press Connect.");
                    self.paired = Some(name);
                    self.use_ble = true;
                    self.presence = Presence::Nearby;
                    self.next_probe = Instant::now() + PROBE_EVERY;
                }
                Msg::Connected {
                    camera,
                    model,
                    firmware,
                    mode,
                } => {
                    if self.link_is_ble() {
                        self.presence = Presence::Nearby;
                    }
                    self.conn = Conn::Connected {
                        camera,
                        model,
                        firmware,
                        mode,
                    };
                }
                Msg::Disconnected { error } => {
                    self.conn = Conn::Disconnected;
                    self.next_probe = Instant::now() + Duration::from_secs(2);
                    self.busy_pairing = false;
                    self.live = None;
                    self.progress = None;
                    if let Some(e) = error {
                        self.push_log(format!("Error: {e}"));
                        self.error = Some(e);
                    }
                }
                Msg::Props(p) => self.props = p,
                Msg::Prop(d) => {
                    if let Some(p) = self.props.iter_mut().find(|p| p.code == d.code) {
                        *p = d;
                    }
                }
                Msg::Objects(o) => {
                    self.selected.retain(|h| o.iter().any(|(x, _)| x == h));
                    self.objects = o;
                }
                Msg::Thumb(h, img) => {
                    let t = ctx.load_texture(format!("thumb{h}"), img, TextureOptions::LINEAR);
                    self.thumbs.insert(h, t);
                }
                Msg::Progress { label, fraction } => self.progress = Some((label, fraction)),
                Msg::ProgressDone(s) => {
                    self.progress = None;
                    self.push_log(s.clone());
                    self.status = s;
                }
                Msg::Log(s) => self.push_log(s),
                Msg::Error(e) => {
                    self.busy_pairing = false;
                    self.push_log(format!("Error: {e}"));
                    self.error = Some(e);
                }
            }
        }
        if let Some(img) = self.shared.frame.lock().unwrap().take() {
            match &mut self.live {
                Some(t) => t.set(img, TextureOptions::LINEAR),
                None => self.live = Some(ctx.load_texture("liveview", img, TextureOptions::LINEAR)),
            }
        }
        let (t0, n0, _) = self.fps;
        if t0.elapsed().as_secs_f32() >= 1.0 {
            let n = self.shared.frames.load(Ordering::Relaxed);
            self.fps = (
                Instant::now(),
                n,
                (n - n0) as f32 / t0.elapsed().as_secs_f32(),
            );
        }
    }

    fn link_is_ble(&self) -> bool {
        self.use_ble && self.paired.is_some()
    }

    /// While idle, keep checking in the background whether the paired camera is
    /// actually on: the saved pairing alone says nothing about that.
    fn poll_presence(&mut self, ctx: &egui::Context) {
        if let Some(rx) = &self.probe {
            match rx.try_recv() {
                Ok(p) => {
                    self.presence = p;
                    self.probe = None;
                    self.next_probe = Instant::now() + PROBE_EVERY;
                }
                Err(std::sync::mpsc::TryRecvError::Disconnected) => {
                    self.probe = None;
                    self.next_probe = Instant::now() + PROBE_EVERY;
                }
                Err(std::sync::mpsc::TryRecvError::Empty) => {}
            }
            return;
        }
        let idle = matches!(self.conn, Conn::Disconnected) && !self.busy_pairing;
        if !idle || !self.use_ble {
            return;
        }
        let Some(pairing) = openfuji::link::last_pairing() else {
            return;
        };
        let now = Instant::now();
        if now < self.next_probe {
            ctx.request_repaint_after(self.next_probe - now);
            return;
        }
        let (tx, rx) = channel();
        let ctx = ctx.clone();
        let cancel = Arc::new(std::sync::atomic::AtomicBool::new(false));
        self.probe_cancel = cancel.clone();
        let spawned = std::thread::Builder::new()
            .name("ble-probe".into())
            .spawn(move || {
                let p = match openfuji::ble::probe(&pairing, PROBE_SCAN, &cancel) {
                    Ok(true) => Presence::Nearby,
                    Ok(false) => Presence::NotFound,
                    Err(e) => {
                        log::debug!("presence probe: {e:#}");
                        Presence::Unknown
                    }
                };
                let _ = tx.send(p);
                ctx.request_repaint();
            });
        if spawned.is_ok() {
            self.probe = Some(rx);
        } else {
            self.next_probe = now + PROBE_EVERY;
        }
    }

    /// Stop a running presence scan before talking to the camera over Bluetooth.
    fn cancel_probe(&mut self) {
        self.probe_cancel.store(true, Ordering::Relaxed);
        self.probe = None;
        self.next_probe = Instant::now() + PROBE_EVERY;
    }

    fn connected_mode(&self) -> Option<Mode> {
        match &self.conn {
            Conn::Connected { mode, .. } => Some(*mode),
            _ => None,
        }
    }

    // ---- top bar -------------------------------------------------------------

    fn top_bar(&mut self, ui: &mut egui::Ui) {
        ui.horizontal(|ui| {
            ui.heading(RichText::new("openFuji").strong());
            ui.separator();
            for (tab, label) in [
                (Tab::Remote, "📷 Remote shooting"),
                (Tab::Gallery, "🖼 Pictures on camera"),
            ] {
                if ui.selectable_label(self.tab == tab, label).clicked() && self.tab != tab {
                    self.tab = tab;
                    // The camera needs a fresh session for a different function.
                    if let Some(m) = self.connected_mode()
                        && m != Self::mode_for(tab)
                    {
                        self.connect();
                    }
                }
            }
            ui.with_layout(
                egui::Layout::right_to_left(egui::Align::Center),
                |ui| match &self.conn {
                    Conn::Connected {
                        camera,
                        model,
                        firmware,
                        ..
                    } => {
                        if ui.button("Disconnect").clicked() {
                            let _ = self.tx.send(Cmd::Disconnect);
                        }
                        ui.label(
                            RichText::new(format!("● {model} “{camera}” fw {firmware}"))
                                .color(Color32::from_rgb(80, 190, 90)),
                        );
                    }
                    Conn::Connecting => {
                        ui.add_enabled(false, egui::Button::new("Connecting…"));
                        ui.spinner();
                    }
                    Conn::Disconnected => {
                        if ui.button(RichText::new("Connect").strong()).clicked() {
                            self.connect();
                        }
                        ui.add(egui::TextEdit::singleline(&mut self.name).desired_width(110.0))
                            .on_hover_text("Name shown on the camera's screen");
                        ui.label("Name");
                        ui.add_enabled(
                            !self.use_ble,
                            egui::TextEdit::singleline(&mut self.ip).desired_width(110.0),
                        );
                        ui.label("Camera IP");
                        ui.separator();
                        let label = match (self.busy_pairing, &self.paired, self.confirm_repair) {
                            (true, _, _) => "Pairing…".to_string(),
                            (_, Some(_), false) => "Re-pair…".to_string(),
                            (_, Some(_), true) => "Really re-pair? Click again".to_string(),
                            (_, None, _) => "Pair camera…".to_string(),
                        };
                        if ui
                            .add_enabled(!self.busy_pairing, egui::Button::new(label))
                            .on_hover_text(
                                "Only needed once. On the camera: Bluetooth settings → Pairing registration, \
                                 then click here",
                            )
                            .clicked()
                        {
                            if self.paired.is_some() && !self.confirm_repair {
                                self.confirm_repair = true;
                            } else {
                                self.confirm_repair = false;
                                self.busy_pairing = true;
                                self.error = None;
                                self.cancel_probe();
                                let _ = self.tx.send(Cmd::Pair { name: self.name.clone() });
                            }
                        }
                        if let Some(p) = &self.paired {
                            let checking = self.probe.is_some();
                            let (text, color, tip) = match (self.presence, self.use_ble) {
                                (_, false) => (
                                    format!("{p} paired"),
                                    Color32::GRAY,
                                    "Bluetooth is off for this connection".to_string(),
                                ),
                                (Presence::Nearby, _) => (
                                    format!("● {p} paired · in range"),
                                    Color32::from_rgb(80, 190, 90),
                                    "The camera is on and advertising over Bluetooth".to_string(),
                                ),
                                (Presence::NotFound, _) => (
                                    format!("○ {p} paired · not found"),
                                    Color32::from_rgb(220, 160, 60),
                                    "Paired, but the camera isn't advertising: is it switched on \
                                     with Bluetooth enabled?"
                                        .to_string(),
                                ),
                                (Presence::Unknown, _) => (
                                    format!("{p} paired · checking…"),
                                    Color32::GRAY,
                                    "Looking for the camera over Bluetooth".to_string(),
                                ),
                            };
                            if checking && self.presence == Presence::Unknown {
                                ui.spinner();
                            }
                            ui.label(RichText::new(text).color(color)).on_hover_text(tip);
                        }
                        let tip = match &self.paired {
                            Some(p) => format!(
                                "Paired camera: {p}. Starts its Wi-Fi over Bluetooth and joins it \
                                 (your PC leaves its current Wi-Fi while connected)."
                            ),
                            None => "Needed for X-E3 and newer bodies. Pair the camera first.".into(),
                        };
                        ui.checkbox(&mut self.use_ble, "Wake Wi-Fi via Bluetooth").on_hover_text(tip);
                    }
                },
            );
        });
    }

    fn bottom_bar(&mut self, ui: &mut egui::Ui) {
        ui.horizontal(|ui| {
            if let Some((label, f)) = &self.progress {
                ui.add(
                    egui::ProgressBar::new(*f)
                        .text(label.clone())
                        .desired_width(ui.available_width() - 90.0),
                );
            } else if let Some(e) = &self.error {
                ui.colored_label(Color32::from_rgb(230, 90, 80), format!("⚠ {e}"));
            } else {
                ui.label(&self.status);
            }
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                ui.toggle_value(&mut self.show_log, "Log");
            });
        });
        if self.show_log {
            egui::ScrollArea::vertical()
                .max_height(140.0)
                .stick_to_bottom(true)
                .show(ui, |ui| {
                    for l in &self.log {
                        ui.monospace(l);
                    }
                });
        }
    }

    // ---- remote --------------------------------------------------------------

    fn remote_controls(&mut self, ui: &mut egui::Ui) {
        let connected = self.connected_mode() == Some(Mode::Remote);
        ui.add_space(6.0);
        let shoot = ui.add_enabled(
            connected,
            egui::Button::new(RichText::new("◉  Shutter").size(22.0).strong())
                .min_size(Vec2::new(ui.available_width(), 56.0)),
        );
        let space = connected && ui.input(|i| i.key_pressed(egui::Key::Space));
        if shoot.clicked() || space {
            let _ = self.tx.send(Cmd::Shoot { af: self.af });
        }
        ui.checkbox(&mut self.af, "Autofocus first (half-press)");
        ui.label(RichText::new("Shortcut: Space").weak().small());
        ui.separator();

        if self.props.is_empty() {
            ui.label(
                RichText::new(if connected {
                    "No adjustable settings reported"
                } else {
                    "Not connected"
                })
                .weak(),
            );
            return;
        }
        let mut change: Option<(u16, PropValue)> = None;
        egui::Grid::new("props")
            .num_columns(2)
            .spacing([10.0, 8.0])
            .show(ui, |ui| {
                for p in &self.props {
                    ui.label(prop_label(p.code));
                    let current = fmt_value(p.code, &p.current);
                    match (&p.form, p.writable) {
                        (PropForm::Enum(values), true) if !values.is_empty() => {
                            egui::ComboBox::from_id_salt(p.code)
                                .selected_text(current)
                                .width(130.0)
                                .show_ui(ui, |ui| {
                                    for v in values {
                                        if ui
                                            .selectable_label(*v == p.current, fmt_value(p.code, v))
                                            .clicked()
                                            && *v != p.current
                                        {
                                            change = Some((p.code, v.clone()));
                                        }
                                    }
                                });
                        }
                        _ => {
                            ui.label(RichText::new(current).strong());
                        }
                    }
                    ui.end_row();
                }
            });
        if let Some((code, value)) = change {
            let _ = self.tx.send(Cmd::SetProp { code, value });
        }
    }

    fn remote_view(&mut self, ui: &mut egui::Ui) {
        match (&self.live, &self.conn) {
            (Some(tex), _) => {
                let avail = ui.available_size();
                let size = tex.size_vec2();
                let scale = (avail.x / size.x).min(avail.y / size.y);
                ui.centered_and_justified(|ui| {
                    ui.add(egui::Image::new(tex).fit_to_exact_size(size * scale));
                });
                let r = ui.max_rect();
                ui.painter().text(
                    r.right_top() + Vec2::new(-8.0, 8.0),
                    egui::Align2::RIGHT_TOP,
                    format!("{:.0} fps", self.fps.2),
                    egui::FontId::monospace(12.0),
                    Color32::from_white_alpha(180),
                );
            }
            (None, Conn::Connected { .. }) => {
                ui.centered_and_justified(|ui| ui.spinner());
            }
            (None, Conn::Connecting) => {
                ui.centered_and_justified(|ui| {
                    ui.label("Connecting… if the camera shows a prompt, press OK on the camera.")
                });
            }
            (None, Conn::Disconnected) => self.help(ui),
        }
    }

    fn help(&mut self, ui: &mut egui::Ui) {
        ui.vertical_centered(|ui| {
            ui.add_space(ui.available_height() * 0.15);
            ui.heading("Connect your Fujifilm camera");
            ui.add_space(8.0);
            ui.label(RichText::new("Bluetooth cameras (X-E3, X-T3 and newer)").strong());
            ui.label("1. Once: on the camera choose Bluetooth settings → Pairing registration, then click “Pair camera…”.");
            ui.label("2. Tick “Wake Wi-Fi via Bluetooth” and click Connect. openFuji starts the camera's Wi-Fi and joins it;");
            ui.label("your PC leaves its current Wi-Fi while connected and switches back when you disconnect.");
            if let Some(p) = &self.paired {
                let (state, color) = match self.presence {
                    Presence::Nearby => ("in range", Color32::from_rgb(80, 190, 90)),
                    Presence::NotFound => ("not found: switch it on", Color32::from_rgb(220, 160, 60)),
                    Presence::Unknown => ("checking…", Color32::GRAY),
                };
                ui.label(RichText::new(format!("Paired camera: {p} ({state})")).color(color));
            }
            ui.add_space(8.0);
            ui.label(RichText::new("Wi-Fi-only cameras (X-T2, X-Pro2, …)").strong());
            ui.label("Start the wireless connection on the camera, join its FUJIFILM-… Wi-Fi,");
            ui.label("untick Bluetooth and click Connect.");
            ui.add_space(12.0);
            ui.label(RichText::new("Tip: run `openfuji-mock` and connect to 127.0.0.1 to try the app without a camera.").weak());
            if ui.button("Connect").clicked() {
                self.connect();
            }
        });
    }

    // ---- gallery -------------------------------------------------------------

    fn gallery(&mut self, ui: &mut egui::Ui) {
        if self.connected_mode() != Some(Mode::PhotoView) {
            if matches!(self.conn, Conn::Disconnected) {
                self.help(ui);
            } else {
                ui.centered_and_justified(|ui| ui.spinner());
            }
            return;
        }
        ui.horizontal(|ui| {
            ui.label("Save to");
            ui.add(egui::TextEdit::singleline(&mut self.download_dir).desired_width(260.0));
            ui.separator();
            if ui.button("Select all").clicked() {
                self.selected = self.objects.iter().map(|(h, _)| *h).collect();
            }
            if ui.button("Select none").clicked() {
                self.selected.clear();
            }
            let busy = self.progress.is_some();
            let n = self.selected.len();
            if ui
                .add_enabled(
                    n > 0 && !busy,
                    egui::Button::new(format!("⬇ Download selected ({n})")),
                )
                .clicked()
            {
                let _ = self.tx.send(Cmd::Download {
                    handles: self.selected.iter().copied().collect(),
                    dir: PathBuf::from(&self.download_dir),
                });
            }
            if ui
                .add_enabled(!busy, egui::Button::new("⟳ Refresh"))
                .clicked()
            {
                let _ = self.tx.send(Cmd::RefreshGallery);
            }
        });
        ui.separator();

        if self.objects.is_empty() {
            ui.centered_and_justified(|ui| ui.label("No pictures (yet)."));
            return;
        }
        let cell = Vec2::new(176.0, 160.0);
        egui::ScrollArea::vertical()
            .auto_shrink([false, false])
            .show(ui, |ui| {
                ui.spacing_mut().item_spacing = Vec2::splat(8.0);
                let cols = ((ui.available_width() + 8.0) / (cell.x + 8.0))
                    .floor()
                    .max(1.0) as usize;
                for row in self.objects.chunks(cols) {
                    ui.horizontal(|ui| {
                        for (h, info) in row {
                            let selected = self.selected.contains(h);
                            let stroke = if selected {
                                egui::Stroke::new(2.0, ui.visuals().selection.stroke.color)
                            } else {
                                egui::Stroke::new(
                                    1.0,
                                    ui.visuals().widgets.noninteractive.bg_stroke.color,
                                )
                            };
                            let resp = egui::Frame::group(ui.style())
                                .stroke(stroke)
                                .fill(if selected {
                                    ui.visuals().selection.bg_fill.gamma_multiply(0.35)
                                } else {
                                    Color32::TRANSPARENT
                                })
                                .show(ui, |ui| {
                                    ui.set_min_size(cell - Vec2::splat(12.0));
                                    ui.set_max_size(cell - Vec2::splat(12.0));
                                    ui.vertical_centered(|ui| {
                                        let img_size = Vec2::new(160.0, 107.0);
                                        match self.thumbs.get(h) {
                                            Some(t) => {
                                                let s = t.size_vec2();
                                                let k = (img_size.x / s.x).min(img_size.y / s.y);
                                                ui.add_sized(
                                                    img_size,
                                                    egui::Image::new(t).fit_to_exact_size(s * k),
                                                );
                                            }
                                            None => {
                                                ui.add_sized(img_size, egui::Spinner::new());
                                            }
                                        }
                                        ui.label(RichText::new(&info.filename).small().strong());
                                        ui.label(
                                            RichText::new(format!(
                                                "{} · {}",
                                                ptp::format_name(info.format),
                                                human(info.compressed_size as u64)
                                            ))
                                            .small()
                                            .weak(),
                                        );
                                    });
                                })
                                .response
                                .interact(egui::Sense::click());
                            if resp.clicked() {
                                if selected {
                                    self.selected.remove(h);
                                } else {
                                    self.selected.insert(*h);
                                }
                            }
                            resp.on_hover_text(format!(
                                "{}\n{}×{}\n{}",
                                info.filename,
                                info.image_width,
                                info.image_height,
                                info.capture_date
                            ));
                        }
                    });
                }
            });
    }
}

impl eframe::App for App {
    fn ui(&mut self, ui: &mut egui::Ui, _frame: &mut eframe::Frame) {
        let ctx = ui.ctx().clone();
        self.drain(&ctx);
        self.poll_presence(&ctx);

        egui::Panel::top("top").show(ui, |ui| {
            ui.add_space(4.0);
            self.top_bar(ui);
            ui.add_space(2.0);
        });
        egui::Panel::bottom("bottom").show(ui, |ui| self.bottom_bar(ui));
        if self.tab == Tab::Remote {
            egui::Panel::right("controls")
                .resizable(false)
                .exact_size(270.0)
                .show(ui, |ui| {
                    self.remote_controls(ui);
                });
        }
        egui::CentralPanel::default().show(ui, |ui| match self.tab {
            Tab::Remote => self.remote_view(ui),
            Tab::Gallery => self.gallery(ui),
        });
    }
}

impl Drop for App {
    fn drop(&mut self) {
        let _ = self.tx.send(Cmd::Disconnect);
    }
}

fn human(n: u64) -> String {
    let n = n as f64;
    if n >= 1e6 {
        format!("{:.1} MB", n / 1e6)
    } else {
        format!("{:.0} kB", n / 1e3)
    }
}

fn prop_label(code: u16) -> &'static str {
    match code {
        prop::EXPOSURE_PROGRAM_MODE => "Exposure mode",
        prop::F_NUMBER => "Aperture",
        0x500D => "Shutter speed",
        prop::EXPOSURE_INDEX => "ISO",
        prop::EXPOSURE_BIAS => "Exposure comp.",
        prop::WHITE_BALANCE => "White balance",
        prop::FOCUS_MODE => "Focus mode",
        prop::FLASH_MODE => "Flash",
        prop::STILL_CAPTURE_MODE => "Drive mode",
        prop::BATTERY_LEVEL => "Battery",
        _ => prop::name(code),
    }
}

/// Human-readable values for standard PTP properties. Fuji-specific values
/// that are not in the PTP spec are shown as hex so they remain identifiable.
fn fmt_value(code: u16, v: &PropValue) -> String {
    let Some(n) = v.as_u64() else {
        return v.to_string();
    };
    match code {
        prop::F_NUMBER => format!("f/{}", trim(n as f64 / 100.0)),
        prop::EXPOSURE_INDEX => {
            let iso = n & 0x00FF_FFFF;
            if n & 0x8000_0000 != 0 {
                format!("Auto ({iso})")
            } else {
                format!("ISO {iso}")
            }
        }
        prop::EXPOSURE_BIAS => {
            let ev = n as i64 as i16 as f64 / 1000.0;
            if ev == 0.0 {
                "±0 EV".into()
            } else {
                format!("{ev:+.1} EV")
            }
        }
        0x500D => {
            // PTP: units of 0.0001 s
            let s = n as f64 / 10000.0;
            if s >= 0.3 {
                format!("{}\"", trim(s))
            } else if n > 0 {
                format!("1/{:.0}", 1.0 / s)
            } else {
                "0".into()
            }
        }
        prop::EXPOSURE_PROGRAM_MODE => match n {
            1 => "Manual".into(),
            2 => "Program".into(),
            3 => "Aperture priority".into(),
            4 => "Shutter priority".into(),
            _ => format!("0x{n:x}"),
        },
        prop::WHITE_BALANCE => match n {
            1 => "Manual".into(),
            2 => "Auto".into(),
            3 => "One-push auto".into(),
            4 => "Daylight".into(),
            5 => "Fluorescent".into(),
            6 => "Incandescent".into(),
            7 => "Flash".into(),
            _ => format!("0x{n:x}"),
        },
        prop::FOCUS_MODE => match n {
            1 => "Manual".into(),
            2 => "Auto".into(),
            3 => "Auto macro".into(),
            _ => format!("0x{n:x}"),
        },
        prop::BATTERY_LEVEL => format!("{n}"),
        _ => format!("0x{n:x}"),
    }
}

fn trim(x: f64) -> String {
    let s = format!("{x:.1}");
    s.strip_suffix(".0").map(str::to_string).unwrap_or(s)
}
