//! Background thread that owns the camera connection. The UI talks to it with
//! [`Cmd`] messages and receives [`Msg`] updates; live-view frames go through a
//! shared slot so the UI always shows the latest one.

use std::collections::VecDeque;
use std::fs::{self, File};
use std::io::{BufWriter, Write};
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::mpsc::{Receiver, RecvTimeoutError, Sender};
use std::sync::{Arc, Mutex};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use eframe::egui::{self, ColorImage};
use openfuji::camera::{Camera, ConnectOptions, EventChannel, LiveView, Mode};
use openfuji::dataset::{DeviceInfo, ObjectInfo, PropDesc, PropValue};
use openfuji::ptp::{ev, prop};

/// GetPartialObject chunk size for downloads.
const DOWNLOAD_CHUNK: u32 = 1024 * 1024;

/// Properties shown on the remote-control panel, in display order.
pub const REMOTE_PROPS: &[u16] = &[
    prop::EXPOSURE_PROGRAM_MODE,
    prop::F_NUMBER,
    0x500D, // exposure time
    prop::EXPOSURE_INDEX,
    prop::EXPOSURE_BIAS,
    prop::WHITE_BALANCE,
    prop::FOCUS_MODE,
    prop::FLASH_MODE,
    prop::STILL_CAPTURE_MODE,
    prop::BATTERY_LEVEL,
];

pub enum Cmd {
    Connect {
        opts: ConnectOptions,
        mode: Mode,
        ble: bool,
    },
    Pair {
        name: String,
    },
    Disconnect,
    Shoot {
        af: bool,
    },
    SetProp {
        code: u16,
        value: PropValue,
    },
    RefreshGallery,
    Download {
        handles: Vec<u32>,
        dir: PathBuf,
    },
}

pub enum Msg {
    Connecting,
    Status(String),
    Paired(String),
    Connected {
        camera: String,
        model: String,
        firmware: String,
        mode: Mode,
    },
    Disconnected {
        error: Option<String>,
    },
    Props(Vec<PropDesc>),
    Prop(PropDesc),
    Objects(Vec<(u32, ObjectInfo)>),
    Thumb(u32, ColorImage),
    Progress {
        label: String,
        fraction: f32,
    },
    ProgressDone(String),
    Log(String),
    Error(String),
}

#[derive(Default)]
pub struct Shared {
    pub frame: Mutex<Option<ColorImage>>,
    pub frames: AtomicU64,
}

pub fn decode_jpeg(data: &[u8]) -> Option<ColorImage> {
    let img = image::load_from_memory_with_format(data, image::ImageFormat::Jpeg)
        .ok()?
        .to_rgba8();
    let size = [img.width() as usize, img.height() as usize];
    Some(ColorImage::from_rgba_unmultiplied(size, img.as_raw()))
}

struct Live {
    tid: u32,
    stop: Arc<AtomicBool>,
    lv: LiveView,
    ev: EventChannel,
    threads: Vec<JoinHandle<()>>,
}

struct Worker {
    rx: Receiver<Cmd>,
    tx: Sender<Msg>,
    ctx: egui::Context,
    shared: Arc<Shared>,
    cam: Option<Camera>,
    mode: Mode,
    live: Option<Live>,
    thumbs: VecDeque<u32>,
    /// Set by the event thread when the camera reports something new.
    dirty: Arc<AtomicBool>,
    /// Bluetooth link + joined camera Wi-Fi, when connecting the Bluetooth way.
    link: Option<openfuji::link::BleWifi>,
}

pub fn spawn(rx: Receiver<Cmd>, tx: Sender<Msg>, ctx: egui::Context, shared: Arc<Shared>) {
    thread::Builder::new()
        .name("camera".into())
        .spawn(move || {
            Worker {
                rx,
                tx,
                ctx,
                shared,
                cam: None,
                mode: Mode::Remote,
                live: None,
                thumbs: VecDeque::new(),
                dirty: Arc::new(AtomicBool::new(false)),
                link: None,
            }
            .run()
        })
        .expect("spawn worker");
}

impl Worker {
    fn send(&self, m: Msg) {
        let _ = self.tx.send(m);
        self.ctx.request_repaint();
    }

    fn log(&self, s: impl Into<String>) {
        let s = s.into();
        log::info!("{s}");
        self.send(Msg::Log(s));
    }

    fn run(mut self) {
        loop {
            // While thumbnails are pending, only peek at commands so they stay responsive.
            let timeout = if self.thumbs.is_empty() {
                Duration::from_millis(1000)
            } else {
                Duration::ZERO
            };
            match self.rx.recv_timeout(timeout) {
                Ok(cmd) => {
                    if let Err(e) = self.handle(cmd) {
                        self.fail(e);
                    }
                }
                Err(RecvTimeoutError::Timeout) => {
                    if let Err(e) = self.idle() {
                        self.fail(e);
                    }
                }
                Err(RecvTimeoutError::Disconnected) => {
                    self.disconnect();
                    return;
                }
            }
        }
    }

    /// Errors on the command channel usually mean the link is gone.
    fn fail(&mut self, e: anyhow::Error) {
        let msg = format!("{e:#}");
        log::warn!("{msg}");
        let fatal = e
            .chain()
            .any(|c| c.downcast_ref::<std::io::Error>().is_some());
        if fatal || self.cam.is_none() {
            self.disconnect();
            self.send(Msg::Disconnected { error: Some(msg) });
        } else {
            self.send(Msg::Error(msg));
        }
    }

    fn handle(&mut self, cmd: Cmd) -> Result<()> {
        match cmd {
            Cmd::Connect {
                mut opts,
                mode,
                ble,
            } => {
                if self.cam.is_some() || self.link.is_some() {
                    self.disconnect();
                    thread::sleep(Duration::from_millis(500));
                }
                self.send(Msg::Connecting);
                if ble {
                    self.send(Msg::Status(
                        "Waking the camera's Wi-Fi over Bluetooth…".into(),
                    ));
                    let link = openfuji::link::start(mode, &opts.name, true)?;
                    self.log(format!("Joined camera Wi-Fi {:?}", link.info.ssid));
                    self.send(Msg::Status(format!(
                        "Joined {}; connecting…",
                        link.info.ssid
                    )));
                    self.link = Some(link);
                    opts.ip = openfuji::ptp::DEFAULT_CAMERA_IP.parse().unwrap();
                    // Give the camera's servers a moment after the network comes up.
                    thread::sleep(Duration::from_secs(1));
                }
                self.connect(&opts, mode)?;
                self.send(Msg::Status(String::new()));
            }
            Cmd::Pair { name } => {
                self.disconnect();
                self.send(Msg::Status(
                    "Pairing: on the camera open Bluetooth settings → Pairing registration…".into(),
                ));
                let (p, link) = openfuji::ble::BleLink::pair(&name, Duration::from_secs(90))?;
                link.disconnect();
                self.log(format!("Paired with {} ({})", p.name, p.address));
                self.send(Msg::Paired(p.name));
            }
            Cmd::Disconnect => {
                self.disconnect();
                self.send(Msg::Disconnected { error: None });
            }
            Cmd::Shoot { af } => {
                let cam = self.cam.as_mut().context("not connected")?;
                if af {
                    cam.s1_lock().context("half-press")?;
                    thread::sleep(Duration::from_millis(600));
                }
                let r = cam.capture();
                if af {
                    cam.s1_unlock().ok();
                }
                r.context("shutter release")?;
                self.log("Shutter released");
            }
            Cmd::SetProp { code, value } => {
                let cam = self.cam.as_mut().context("not connected")?;
                cam.set_prop_typed(code, &value)?;
                let d = cam.prop_desc(code)?;
                self.send(Msg::Prop(d));
            }
            Cmd::RefreshGallery => self.load_gallery()?,
            Cmd::Download { handles, dir } => self.download(handles, dir)?,
        }
        Ok(())
    }

    fn connect(&mut self, opts: &ConnectOptions, mode: Mode) -> Result<()> {
        let mut cam = Camera::connect(opts)?;
        // The X-E3 (when its Wi-Fi was started over Bluetooth) answers
        // GetDeviceInfo with an empty dataset until the function mode is set.
        let info = match cam.device_info() {
            Ok(info) => {
                cam.set_mode(mode)?;
                info
            }
            Err(e) => {
                log::info!("device info not available before setting the mode ({e:#}); retrying after");
                cam.set_mode(mode)?;
                match cam.device_info() {
                    Ok(info) => info,
                    Err(e) => {
                        // Seen on the X-E3 after a Bluetooth-started connection.
                        // Nothing below needs it except the model name and the
                        // property list, so carry on without it.
                        log::warn!("camera sent no device info ({e:#}); continuing without it");
                        DeviceInfo {
                            model: cam.ack.camera_name.clone(),
                            properties: REMOTE_PROPS.to_vec(),
                            ..Default::default()
                        }
                    }
                }
            }
        };
        self.mode = mode;
        let camera = cam.ack.camera_name.clone();
        self.cam = Some(cam);
        self.send(Msg::Connected {
            camera,
            model: info.model.clone(),
            firmware: info.device_version.clone(),
            mode,
        });
        self.log(format!(
            "Connected to {} (firmware {})",
            info.model, info.device_version
        ));

        if mode == Mode::Remote {
            let props: Vec<u16> = REMOTE_PROPS
                .iter()
                .copied()
                .filter(|p| info.properties.contains(p))
                .collect();
            let cam = self.cam.as_mut().unwrap();
            let descs: Vec<PropDesc> = props
                .iter()
                .filter_map(|&p| cam.prop_desc(p).ok())
                .collect();
            self.send(Msg::Props(descs));
            self.start_live()?;
        } else {
            self.load_gallery()?;
        }
        Ok(())
    }

    fn start_live(&mut self) -> Result<()> {
        let cam = self.cam.as_mut().context("not connected")?;
        let (tid, lv, ev) = cam.start_liveview()?;
        ev.set_blocking_forever()?;
        let stop = Arc::new(AtomicBool::new(false));
        let mut threads = vec![];

        let (mut lv2, stop2, shared, ctx) = (
            lv.try_clone()?,
            stop.clone(),
            self.shared.clone(),
            self.ctx.clone(),
        );
        threads.push(thread::spawn(move || {
            while !stop2.load(Ordering::Relaxed) {
                match lv2.next_frame() {
                    Ok(f) => {
                        if let Some(img) = decode_jpeg(&f.jpeg) {
                            *shared.frame.lock().unwrap() = Some(img);
                            shared.frames.fetch_add(1, Ordering::Relaxed);
                            ctx.request_repaint();
                        }
                    }
                    Err(e) => {
                        if !stop2.load(Ordering::Relaxed) {
                            log::warn!("live view: {e:#}");
                        }
                        break;
                    }
                }
            }
        }));

        let (mut ev2, stop3, tx, dirty, ctx) = (
            ev.try_clone()?,
            stop.clone(),
            self.tx.clone(),
            self.dirty.clone(),
            self.ctx.clone(),
        );
        threads.push(thread::spawn(move || {
            while let Ok(e) = ev2.next_event() {
                if stop3.load(Ordering::Relaxed) {
                    break;
                }
                let _ = tx.send(Msg::Log(format!(
                    "Camera event {} (0x{:04x}) {:x?}",
                    ev::name(e.code),
                    e.code,
                    e.params
                )));
                dirty.store(true, Ordering::Relaxed);
                ctx.request_repaint();
            }
        }));

        self.live = Some(Live {
            tid,
            stop,
            lv,
            ev,
            threads,
        });
        Ok(())
    }

    fn stop_live(&mut self) {
        if let Some(live) = self.live.take() {
            // Terminate while the reader threads still drain the live-view and
            // event sockets (a stalled live-view socket stalls the camera).
            if let Some(cam) = self.cam.as_mut()
                && let Err(e) = cam.stop_liveview(live.tid)
            {
                log::warn!("stopping live view: {e:#}");
            }
            live.stop.store(true, Ordering::Relaxed);
            live.lv.shutdown();
            live.ev.shutdown();
            for t in live.threads {
                let _ = t.join();
            }
            *self.shared.frame.lock().unwrap() = None;
        }
    }

    fn disconnect(&mut self) {
        self.stop_live();
        self.thumbs.clear();
        if let Some(cam) = self.cam.take() {
            cam.close().ok();
        }
        if let Some(link) = self.link.take() {
            self.send(Msg::Status("Leaving camera Wi-Fi…".into()));
            link.stop();
            self.send(Msg::Status(String::new()));
        }
    }

    /// Called about once a second (or immediately when thumbnails are queued).
    fn idle(&mut self) -> Result<()> {
        if let Some(h) = self.thumbs.pop_front() {
            let cam = self.cam.as_mut().context("not connected")?;
            match cam.thumbnail(h) {
                Ok(data) => {
                    if let Some(img) = decode_jpeg(&data) {
                        self.send(Msg::Thumb(h, img));
                    }
                }
                Err(e) => log::debug!("thumbnail {h}: {e}"),
            }
            return Ok(());
        }
        let Some(cam) = self.cam.as_mut() else {
            return Ok(());
        };
        // The app polls the Fuji change list; it also keeps the session alive.
        let changes = cam.poll_events()?;
        let dirty = self.dirty.swap(false, Ordering::Relaxed);
        if self.mode == Mode::Remote {
            let mut updated = vec![];
            for (code, _) in &changes {
                if REMOTE_PROPS.contains(code) && !updated.contains(code) {
                    updated.push(*code);
                }
            }
            for code in updated {
                if let Ok(d) = cam.prop_desc(code) {
                    let _ = self.tx.send(Msg::Prop(d));
                    self.ctx.request_repaint();
                }
            }
        } else if dirty || changes.iter().any(|(c, _)| *c == prop::FUJI_OBJECT_COUNT) {
            self.load_gallery()?;
        }
        Ok(())
    }

    fn load_gallery(&mut self) -> Result<()> {
        let cam = self.cam.as_mut().context("not connected")?;
        let handles = cam.object_handles()?;
        let mut objects = Vec::with_capacity(handles.len());
        let n = handles.len();
        for (i, h) in handles.into_iter().enumerate() {
            match cam.object_info(h) {
                Ok(o) if o.format != 0x3001 => objects.push((h, o)),
                Ok(_) => {}
                Err(e) => log::debug!("object {h}: {e}"),
            }
            if i % 10 == 0 {
                let _ = self.tx.send(Msg::Progress {
                    label: format!("Reading picture list {}/{n}", i + 1),
                    fraction: (i + 1) as f32 / n.max(1) as f32,
                });
                self.ctx.request_repaint();
            }
        }
        // Newest first, which is what people usually want to see.
        objects.reverse();
        self.thumbs = objects.iter().map(|(h, _)| *h).collect();
        self.send(Msg::ProgressDone(format!(
            "{} pictures on the camera",
            objects.len()
        )));
        self.send(Msg::Objects(objects));
        Ok(())
    }

    fn download(&mut self, handles: Vec<u32>, dir: PathBuf) -> Result<()> {
        fs::create_dir_all(&dir).with_context(|| format!("creating {}", dir.display()))?;
        let total = handles.len();
        let mut saved = 0;
        for (i, h) in handles.into_iter().enumerate() {
            let cam = self.cam.as_mut().context("not connected")?;
            let info = cam.object_info(h)?;
            let name = if info.filename.is_empty() {
                format!("object_{h}")
            } else {
                info.filename.clone()
            };
            let path = dir.join(&name);
            if fs::metadata(&path).is_ok_and(|m| m.len() == info.compressed_size as u64) {
                continue;
            }
            let tmp = dir.join(format!(".{name}.part"));
            let mut f = BufWriter::new(File::create(&tmp)?);
            let size = info.compressed_size.max(1) as f32;
            let (tx, ctx) = (self.tx.clone(), self.ctx.clone());
            let mut last = Instant::now();
            let mut progress = |done: u64| {
                if last.elapsed() > Duration::from_millis(100) {
                    last = Instant::now();
                    let _ = tx.send(Msg::Progress {
                        label: format!("Downloading {name} ({}/{total})", i + 1),
                        fraction: (i as f32 + done as f32 / size) / total as f32,
                    });
                    ctx.request_repaint();
                }
            };
            // GetPartialObject in chunks: over Wi-Fi the X-E3 stalls a plain
            // GetObject after its first 100 KiB.
            if info.compressed_size > 0 {
                cam.download_partial(
                    h,
                    info.compressed_size as u64,
                    DOWNLOAD_CHUNK,
                    &mut f,
                    &mut progress,
                )?;
            } else {
                cam.download(h, &mut f, &mut progress)?;
            }
            f.flush()?;
            drop(f);
            fs::rename(&tmp, &path)?;
            saved += 1;
        }
        self.send(Msg::ProgressDone(format!(
            "Saved {saved} file(s) to {}",
            dir.display()
        )));
        Ok(())
    }
}
