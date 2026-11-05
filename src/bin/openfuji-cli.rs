use anyhow::{Context, Result, bail};
use clap::{Parser, Subcommand, ValueEnum};
use std::fs::{self, File};
use std::io::{BufWriter, Write};
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::thread;
use std::time::{Duration, Instant};

use openfuji::camera::{Camera, ConnectOptions, Mode};
use openfuji::dataset::{PropForm, PropValue};
use openfuji::ptp::{self, ev, format_name, op, prop, rc};

#[derive(Parser)]
#[command(
    name = "openfuji",
    version,
    about = "Control Fujifilm cameras over Wi-Fi from Linux"
)]
struct Cli {
    /// Camera IP address (192.168.0.1 when the camera is the access point)
    #[arg(long, global = true, default_value = ptp::DEFAULT_CAMERA_IP)]
    ip: String,

    /// Client name shown on the camera's screen
    #[arg(long, global = true)]
    name: Option<String>,

    /// Override the 16-byte client GUID (32 hex chars)
    #[arg(long, global = true)]
    guid: Option<String>,

    /// How many times to retry the handshake while the camera reports "busy"
    #[arg(long, global = true, default_value_t = 6)]
    busy_attempts: u32,

    /// Seconds to wait for the camera to answer the handshake
    #[arg(long, global = true, default_value_t = 120)]
    handshake_timeout: u64,

    /// Function-mode handshake to perform after connecting (default depends on the command)
    #[arg(long, global = true, value_enum)]
    mode: Option<ModeArg>,

    /// Start the camera's Wi-Fi over Bluetooth first and join it (needs `ble-pair` once)
    #[arg(long, global = true)]
    ble: bool,

    /// Verbose logging (-v debug, -vv packet hex dumps)
    #[arg(short, long, global = true, action = clap::ArgAction::Count)]
    verbose: u8,

    #[command(subcommand)]
    cmd: Cmd,
}

#[derive(Clone, Copy, ValueEnum, PartialEq)]
enum ModeArg {
    /// Skip the function-mode handshake
    None,
    /// Browse/import pictures (FunctionMode 2)
    View,
    /// Browse/import, "Ver1.2" variant used by newer bodies (FunctionMode 9)
    ViewV12,
    /// Remote shooting / live view (FunctionMode 5)
    Remote,
}

#[derive(Subcommand)]
enum Cmd {
    /// Show device info and Fuji connection state
    Info,
    /// List Fujifilm cameras advertising over Bluetooth
    BleScan {
        #[arg(long, default_value_t = 15)]
        seconds: u64,
    },
    /// Pair with a camera in "Pairing registration" mode and remember it
    BlePair {
        #[arg(long, default_value_t = 60)]
        seconds: u64,
    },
    /// Start the paired camera's Wi-Fi over Bluetooth and print its SSID/passphrase
    BleLaunch {
        /// Request the picture viewer instead of remote shooting
        #[arg(long)]
        viewer: bool,
        /// Also join the camera's Wi-Fi (until you press Enter)
        #[arg(long)]
        join: bool,
    },
    /// Dump property descriptors (all advertised ones by default)
    Props {
        /// Property codes in hex (e.g. 5001 d212)
        codes: Vec<String>,
    },
    /// Read one property value
    Get { code: String },
    /// Write one property value (type taken from the property descriptor)
    Set {
        code: String,
        /// Integer (decimal or 0x-hex) or, with --string, text
        value: String,
        #[arg(long)]
        string: bool,
    },
    /// List pictures on the card
    Ls,
    /// Download pictures
    Download {
        /// Object handles (default: all)
        handles: Vec<u32>,
        /// Only the N most recent pictures
        #[arg(long)]
        latest: Option<usize>,
        /// Output directory
        #[arg(short, long, default_value = ".")]
        out: PathBuf,
        /// Use GetPartialObject with this chunk size (bytes) instead of GetObject
        #[arg(long)]
        chunk: Option<u32>,
        /// Re-download files that already exist with the same size
        #[arg(long)]
        overwrite: bool,
    },
    /// Save the thumbnail of a picture
    Thumb {
        handle: u32,
        #[arg(short, long)]
        out: Option<PathBuf>,
    },
    /// Stream live view (as MJPEG on stdout, or JPEG files into a directory)
    Liveview {
        /// Stop after N frames (default: run until interrupted)
        #[arg(long)]
        frames: Option<u64>,
        /// Write numbered JPEG files into this directory
        #[arg(long, conflicts_with = "stdout")]
        dir: Option<PathBuf>,
        /// Write an MJPEG stream to stdout (pipe into `ffplay -f mjpeg -i -`)
        #[arg(long)]
        stdout: bool,
    },
    /// Take a picture
    Shoot {
        /// Half-press (autofocus) before releasing
        #[arg(long)]
        af: bool,
    },
    /// Poll the Fuji change list (property 0xD212) and print changes
    Events {
        #[arg(long, default_value_t = 1000)]
        interval_ms: u64,
        /// Stop after this many seconds
        #[arg(long)]
        seconds: Option<u64>,
    },
    /// Send an arbitrary PTP operation and hex-dump the result
    Raw {
        /// Operation code in hex
        opcode: String,
        /// Parameters (decimal or 0x-hex), up to 5
        params: Vec<String>,
        /// Data phase to send, as hex
        #[arg(long)]
        data: Option<String>,
    },
}

fn parse_hex16(s: &str) -> Result<u16> {
    let t = s.trim_start_matches("0x").trim_start_matches("0X");
    u16::from_str_radix(t, 16).with_context(|| format!("invalid hex code {s:?}"))
}

fn parse_num(s: &str) -> Result<u64> {
    if let Some(h) = s.strip_prefix("0x").or_else(|| s.strip_prefix("0X")) {
        Ok(u64::from_str_radix(h, 16)?)
    } else {
        Ok(s.parse::<u64>()
            .or_else(|_| s.parse::<i64>().map(|v| v as u64))?)
    }
}

fn parse_hex_bytes(s: &str) -> Result<Vec<u8>> {
    let clean: String = s.chars().filter(|c| c.is_ascii_hexdigit()).collect();
    if !clean.len().is_multiple_of(2) {
        bail!("odd number of hex digits");
    }
    (0..clean.len())
        .step_by(2)
        .map(|i| u8::from_str_radix(&clean[i..i + 2], 16).map_err(Into::into))
        .collect()
}

fn hexdump(b: &[u8]) {
    for (i, chunk) in b.chunks(16).enumerate() {
        let hex: Vec<String> = chunk.iter().map(|x| format!("{x:02x}")).collect();
        let asc: String = chunk
            .iter()
            .map(|&c| {
                if (0x20..0x7f).contains(&c) {
                    c as char
                } else {
                    '.'
                }
            })
            .collect();
        println!("{:08x}  {:<48} {asc}", i * 16, hex.join(" "));
    }
}

fn human(n: u64) -> String {
    let n = n as f64;
    if n >= 1e9 {
        format!("{:.2} GB", n / 1e9)
    } else if n >= 1e6 {
        format!("{:.1} MB", n / 1e6)
    } else if n >= 1e3 {
        format!("{:.0} kB", n / 1e3)
    } else {
        format!("{n} B")
    }
}

fn main() {
    let cli = Cli::parse();
    let level = match cli.verbose {
        0 => "info",
        1 => "debug",
        _ => "trace",
    };
    env_logger::Builder::from_env(env_logger::Env::default().default_filter_or(level))
        .format_timestamp_millis()
        .init();
    if let Err(e) = run(cli) {
        eprintln!("error: {e:#}");
        std::process::exit(1);
    }
}

fn run(cli: Cli) -> Result<()> {
    let mut opts = ConnectOptions {
        ip: cli.ip.parse().context("invalid --ip")?,
        busy_attempts: cli.busy_attempts,
        handshake_timeout: Duration::from_secs(cli.handshake_timeout),
        ..Default::default()
    };
    if let Some(n) = cli.name {
        opts.name = n;
    }
    if let Some(g) = &cli.guid {
        let b = parse_hex_bytes(g)?;
        opts.guid = Some(
            b.try_into()
                .map_err(|_| anyhow::anyhow!("--guid must be 16 bytes"))?,
        );
    }

    let default_mode = match cli.cmd {
        Cmd::Ls | Cmd::Download { .. } | Cmd::Thumb { .. } => ModeArg::View,
        Cmd::Liveview { .. } | Cmd::Shoot { .. } => ModeArg::Remote,
        _ => ModeArg::None,
    };
    let mode = cli.mode.unwrap_or(default_mode);

    // Bluetooth-only commands.
    match &cli.cmd {
        Cmd::BleScan { seconds } => {
            let rt = openfuji::ble::runtime()?;
            let found = rt.block_on(openfuji::ble::scan(
                Duration::from_secs(*seconds),
                false,
                None,
            ))?;
            if found.is_empty() {
                println!("no Fujifilm cameras seen (is the camera's Bluetooth on?)");
            }
            for f in found {
                println!(
                    "{}  {:<20} rssi {:>4}  pairing-mode {}  data {}",
                    f.address,
                    f.name,
                    f.rssi.unwrap_or(0),
                    if f.pairing_key.is_some() { "yes" } else { "no" },
                    f.manufacturer_data
                        .iter()
                        .map(|b| format!("{b:02x}"))
                        .collect::<String>()
                );
            }
            return Ok(());
        }
        Cmd::BlePair { seconds } => {
            eprintln!(
                "Looking for a camera in pairing mode (camera: Bluetooth settings → Pairing registration)…"
            );
            let (p, link) =
                openfuji::ble::BleLink::pair(&opts.name, Duration::from_secs(*seconds))?;
            println!(
                "Paired with {} ({}). Saved; use --ble from now on.",
                p.name, p.address
            );
            link.disconnect();
            return Ok(());
        }
        Cmd::BleLaunch { viewer, join } => {
            let m = if *viewer {
                Mode::PhotoView
            } else {
                Mode::Remote
            };
            let link = openfuji::link::start(m, &opts.name, *join)?;
            println!(
                "SSID: {}\npassphrase: {:?}",
                link.info.ssid, link.info.passphrase
            );
            eprintln!("Press Enter to disconnect…");
            let mut s = String::new();
            let _ = std::io::stdin().read_line(&mut s);
            link.stop();
            return Ok(());
        }
        _ => {}
    }

    let _ble_link = if cli.ble {
        let m = if mode == ModeArg::View || mode == ModeArg::ViewV12 {
            Mode::PhotoView
        } else {
            Mode::Remote
        };
        Some(openfuji::link::start(m, &opts.name, true)?)
    } else {
        None
    };

    let mut cam = Camera::connect(&opts)?;
    match mode {
        ModeArg::None => {}
        ModeArg::View => cam.set_mode(Mode::PhotoView)?,
        ModeArg::ViewV12 => cam.set_mode(Mode::PhotoViewV12)?,
        ModeArg::Remote => cam.set_mode(Mode::Remote)?,
    }

    match cli.cmd {
        Cmd::BleScan { .. } | Cmd::BlePair { .. } | Cmd::BleLaunch { .. } => unreachable!(),
        Cmd::Info => cmd_info(&mut cam)?,
        Cmd::Props { codes } => cmd_props(&mut cam, &codes)?,
        Cmd::Get { code } => {
            let code = parse_hex16(&code)?;
            match cam.prop_desc(code) {
                Ok(d) => println!("0x{code:04x} {} = {}", prop::name(code), d.current),
                Err(_) => {
                    let raw = cam.prop_raw(code)?;
                    println!("0x{code:04x} {} (raw):", prop::name(code));
                    hexdump(&raw);
                }
            }
        }
        Cmd::Set {
            code,
            value,
            string,
        } => {
            let code = parse_hex16(&code)?;
            let v = if string {
                PropValue::Str(value)
            } else {
                PropValue::UInt(parse_num(&value)?)
            };
            cam.set_prop_typed(code, &v)?;
            println!("0x{code:04x} {} set", prop::name(code));
        }
        Cmd::Ls => cmd_ls(&mut cam)?,
        Cmd::Download {
            handles,
            latest,
            out,
            chunk,
            overwrite,
        } => cmd_download(&mut cam, handles, latest, &out, chunk, overwrite)?,
        Cmd::Thumb { handle, out } => {
            let data = cam.thumbnail(handle)?;
            let path = out.unwrap_or_else(|| PathBuf::from(format!("thumb_{handle}.jpg")));
            fs::write(&path, &data)?;
            eprintln!("wrote {} ({})", path.display(), human(data.len() as u64));
        }
        Cmd::Liveview {
            frames,
            dir,
            stdout,
        } => cmd_liveview(&mut cam, frames, dir, stdout)?,
        Cmd::Shoot { af } => cmd_shoot(&mut cam, af)?,
        Cmd::Events {
            interval_ms,
            seconds,
        } => {
            let start = Instant::now();
            loop {
                for (code, v) in cam.poll_events()? {
                    println!("0x{code:04x} {:<28} = {v} (0x{v:x})", prop::name(code));
                }
                if seconds.is_some_and(|s| start.elapsed() >= Duration::from_secs(s)) {
                    break;
                }
                thread::sleep(Duration::from_millis(interval_ms));
            }
        }
        Cmd::Raw {
            opcode,
            params,
            data,
        } => {
            let code = parse_hex16(&opcode)?;
            let params: Vec<u32> = params
                .iter()
                .map(|p| parse_num(p).map(|v| v as u32))
                .collect::<Result<_>>()?;
            let data = data.map(|d| parse_hex_bytes(&d)).transpose()?;
            let (resp, out) = cam.raw(code, &params, data.as_deref())?;
            println!(
                "response {} (0x{:04x}) params {:x?}",
                rc::name(resp.code),
                resp.code,
                resp.params
            );
            if !out.is_empty() {
                println!("data ({} bytes):", out.len());
                hexdump(&out);
            }
        }
    }

    cam.close().ok();
    Ok(())
}

fn cmd_info(cam: &mut Camera) -> Result<()> {
    println!("Camera name (handshake): {}", cam.ack.camera_name);
    let d = cam.device_info()?;
    println!("Manufacturer:  {}", d.manufacturer);
    println!("Model:         {}", d.model);
    println!("Firmware:      {}", d.device_version);
    println!("Serial:        {}", d.serial_number);
    println!(
        "Vendor ext:    0x{:x} v{} {:?}",
        d.vendor_extension_id, d.vendor_extension_version, d.vendor_extension_desc
    );
    println!("Operations:");
    for o in &d.operations {
        println!("  0x{o:04x} {}", op::name(*o));
    }
    println!(
        "Events:        {}",
        d.events
            .iter()
            .map(|e| format!("0x{e:04x}({})", ev::name(*e)))
            .collect::<Vec<_>>()
            .join(" ")
    );
    println!(
        "Properties:    {}",
        d.properties
            .iter()
            .map(|p| format!("{p:04x}"))
            .collect::<Vec<_>>()
            .join(" ")
    );
    println!("Fuji state:");
    for code in [
        prop::FUJI_CAMERA_STATE,
        prop::FUJI_FUNCTION_MODE,
        prop::FUJI_OBJECT_COUNT,
        prop::FUJI_VERSION_PHOTO_RECEIVE,
        prop::FUJI_VERSION_PHOTO_VIEW,
        prop::FUJI_VERSION_REMOTE,
        prop::FUJI_VERSION_GPS,
        prop::BATTERY_LEVEL,
    ] {
        match cam.prop_u32(code) {
            Ok(v) => println!("  0x{code:04x} {:<28} {v} (0x{v:x})", prop::name(code)),
            Err(e) => println!("  0x{code:04x} {:<28} <{e}>", prop::name(code)),
        }
    }
    if let Ok(ids) = cam.storage_ids() {
        for id in ids {
            match cam.storage_info(id) {
                Ok(s) => println!(
                    "Storage 0x{id:08x}: {} {:?} free {} of {}, {} images left",
                    s.description,
                    s.label,
                    human(s.free_bytes),
                    human(s.max_capacity),
                    s.free_images
                ),
                Err(e) => println!("Storage 0x{id:08x}: <{e}>"),
            }
        }
    }
    Ok(())
}

fn cmd_props(cam: &mut Camera, codes: &[String]) -> Result<()> {
    let codes: Vec<u16> = if codes.is_empty() {
        cam.device_info()?.properties
    } else {
        codes
            .iter()
            .map(|c| parse_hex16(c))
            .collect::<Result<_>>()?
    };
    for code in codes {
        match cam.prop_desc(code) {
            Ok(d) => {
                let form = match &d.form {
                    PropForm::None => String::new(),
                    PropForm::Range { min, max, step } => {
                        format!(" range {min}..{max} step {step}")
                    }
                    PropForm::Enum(v) => format!(
                        " enum [{}]",
                        v.iter()
                            .map(|x| x
                                .as_u64()
                                .map(|n| format!("{n:#x}"))
                                .unwrap_or_else(|| x.to_string()))
                            .collect::<Vec<_>>()
                            .join(", ")
                    ),
                };
                println!(
                    "0x{code:04x} {:<28} {} type=0x{:04x} = {}{form}",
                    prop::name(code),
                    if d.writable { "rw" } else { "ro" },
                    d.datatype,
                    d.current
                );
            }
            Err(e) => println!("0x{code:04x} {:<28} <{e}>", prop::name(code)),
        }
    }
    Ok(())
}

fn cmd_ls(cam: &mut Camera) -> Result<()> {
    let handles = cam.object_handles()?;
    println!("{} objects", handles.len());
    for h in handles {
        match cam.object_info(h) {
            Ok(o) => println!(
                "{h:>6}  {:<16} {:<5} {:>10}  {}x{}  {}",
                o.filename,
                format_name(o.format),
                human(o.compressed_size as u64),
                o.image_width,
                o.image_height,
                o.capture_date
            ),
            Err(e) => println!("{h:>6}  <{e}>"),
        }
    }
    Ok(())
}

fn cmd_download(
    cam: &mut Camera,
    mut handles: Vec<u32>,
    latest: Option<usize>,
    out: &PathBuf,
    chunk: Option<u32>,
    overwrite: bool,
) -> Result<()> {
    fs::create_dir_all(out)?;
    if handles.is_empty() {
        handles = cam.object_handles()?;
    }
    if let Some(n) = latest {
        let skip = handles.len().saturating_sub(n);
        handles.drain(..skip);
    }
    let total = handles.len();
    for (i, h) in handles.into_iter().enumerate() {
        let info = cam.object_info(h)?;
        if info.format == 0x3001 {
            continue; // folder
        }
        let name = if info.filename.is_empty() {
            format!("object_{h}")
        } else {
            info.filename.clone()
        };
        let path = out.join(&name);
        if !overwrite
            && let Ok(m) = fs::metadata(&path)
            && m.len() == info.compressed_size as u64
        {
            eprintln!("[{}/{total}] {name}: exists, skipping", i + 1);
            continue;
        }
        let tmp = out.join(format!(".{name}.part"));
        let mut f = BufWriter::new(File::create(&tmp)?);
        let size = info.compressed_size as u64;
        let mut last = Instant::now();
        let mut progress = |done: u64| {
            if last.elapsed() > Duration::from_millis(250) || done == size {
                last = Instant::now();
                let pct = (done * 100).checked_div(size).unwrap_or(0);
                eprint!(
                    "\r[{}/{total}] {name}: {} / {} ({pct}%)   ",
                    i + 1,
                    human(done),
                    human(size)
                );
            }
        };
        let got = match chunk {
            Some(c) => cam.download_partial(h, size, c, &mut f, &mut progress)?,
            None => cam.download(h, &mut f, &mut progress)?,
        };
        f.flush()?;
        drop(f);
        fs::rename(&tmp, &path)?;
        eprintln!(
            "\r[{}/{total}] {name}: {} done{}",
            i + 1,
            human(got),
            " ".repeat(20)
        );
    }
    Ok(())
}

fn cmd_liveview(
    cam: &mut Camera,
    frames: Option<u64>,
    dir: Option<PathBuf>,
    stdout: bool,
) -> Result<()> {
    if dir.is_none() && !stdout {
        bail!("choose --stdout (MJPEG stream) or --dir <directory>");
    }
    if let Some(d) = &dir {
        fs::create_dir_all(d)?;
    }
    let (tid, mut lv, mut evc) = cam.start_liveview()?;
    evc.set_blocking_forever()?;
    thread::spawn(move || {
        while let Ok(e) = evc.next_event() {
            log::info!(
                "event {} (0x{:04x}) params {:x?}",
                ev::name(e.code),
                e.code,
                e.params
            );
        }
    });

    let done = Arc::new(AtomicBool::new(false));
    let count = Arc::new(AtomicU64::new(0));
    let (done2, count2) = (done.clone(), count.clone());
    let worker = thread::spawn(move || -> Result<()> {
        let mut out = std::io::stdout().lock();
        let res = (|| -> Result<()> {
            loop {
                let f = lv.next_frame()?;
                let n = count2.fetch_add(1, Ordering::Relaxed) + 1;
                if n == 1 {
                    log::debug!("first frame header: {:02x?}", f.header);
                }
                match &dir {
                    Some(d) => fs::write(d.join(format!("frame_{n:06}.jpg")), &f.jpeg)?,
                    None => {
                        out.write_all(&f.jpeg)?;
                        out.flush()?;
                    }
                }
                if frames.is_some_and(|max| n >= max) {
                    return Ok(());
                }
            }
        })();
        done2.store(true, Ordering::Relaxed);
        res
    });

    // Keep the command channel alive like the app does by polling the change list.
    let start = Instant::now();
    while !done.load(Ordering::Relaxed) {
        thread::sleep(Duration::from_millis(1000));
        match cam.poll_events() {
            Ok(evs) => {
                for (code, v) in evs {
                    log::debug!("change 0x{code:04x} {} = {v}", prop::name(code));
                }
            }
            Err(e) => log::debug!("poll: {e}"),
        }
        let n = count.load(Ordering::Relaxed);
        log::info!(
            "{n} frames, {:.1} fps",
            n as f64 / start.elapsed().as_secs_f64()
        );
    }
    let res = worker.join().expect("live-view thread panicked");
    cam.stop_liveview(tid).ok();
    match res {
        Err(e)
            if e.downcast_ref::<std::io::Error>()
                .is_some_and(|io| io.kind() == std::io::ErrorKind::BrokenPipe) =>
        {
            Ok(())
        }
        r => r,
    }
}

fn cmd_shoot(cam: &mut Camera, af: bool) -> Result<()> {
    let before = cam.prop_u32(prop::FUJI_OBJECT_COUNT).ok();
    let (tid, mut lv, mut evc) = cam.start_liveview()?;
    evc.set_blocking_forever()?;
    thread::spawn(move || while lv.next_frame().is_ok() {});
    thread::spawn(move || {
        while let Ok(e) = evc.next_event() {
            log::info!(
                "event {} (0x{:04x}) params {:x?}",
                ev::name(e.code),
                e.code,
                e.params
            );
        }
    });
    thread::sleep(Duration::from_millis(500));
    if af {
        cam.s1_lock().context("half-press (S1 lock)")?;
        thread::sleep(Duration::from_millis(700));
    }
    let res = cam.capture();
    if af {
        cam.s1_unlock().ok();
    }
    res.context("InitiateCapture")?;
    eprintln!("shutter released");
    for _ in 0..4 {
        thread::sleep(Duration::from_millis(750));
        for (code, v) in cam.poll_events().unwrap_or_default() {
            log::info!("change 0x{code:04x} {} = {v}", prop::name(code));
        }
    }
    if let (Some(b), Ok(a)) = (before, cam.prop_u32(prop::FUJI_OBJECT_COUNT)) {
        eprintln!("object count {b} -> {a}");
    }
    cam.stop_liveview(tid).ok();
    Ok(())
}
