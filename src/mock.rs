//! A fake Fujifilm camera speaking the same protocol, for testing without hardware.
//!
//! It listens on the real ports (55740-55742) of a local address, accepts the
//! init handshake, answers the PTP operations openFuji uses, serves generated
//! JPEG "pictures" and streams an animated live view.

use anyhow::{Context, Result, bail};
use log::{debug, info, warn};
use std::collections::HashMap;
use std::io::{Read, Write};
use std::net::{IpAddr, SocketAddr, TcpListener, TcpStream};
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

use crate::ptp::{self, ContainerType, HEADER_LEN, Header, dtc, op, prop, rc};

struct Prop {
    datatype: u16,
    value: i64,
    values: Vec<i64>,
    writable: bool,
}

struct State {
    pictures: Vec<Vec<u8>>,
    props: HashMap<u16, Prop>,
    changes: Vec<(u16, u32)>,
    events: Option<TcpStream>,
}

pub struct MockCamera {
    state: Arc<Mutex<State>>,
    liveview_on: Arc<AtomicBool>,
    frame_no: Arc<AtomicU32>,
}

fn jpeg(w: u32, h: u32, f: impl Fn(u32, u32) -> [u8; 3]) -> Vec<u8> {
    let mut rgb = Vec::with_capacity((w * h * 3) as usize);
    for y in 0..h {
        for x in 0..w {
            rgb.extend_from_slice(&f(x, y));
        }
    }
    let mut out = Vec::new();
    image::codecs::jpeg::JpegEncoder::new_with_quality(&mut out, 80)
        .encode(&rgb, w, h, image::ExtendedColorType::Rgb8)
        .expect("jpeg encode");
    out
}

fn picture(seed: u32) -> Vec<u8> {
    let (r0, g0, b0) = ((seed * 67) % 255, (seed * 131) % 255, (seed * 29) % 255);
    jpeg(640, 426, |x, y| {
        [
            ((r0 + x / 3) % 256) as u8,
            ((g0 + y / 2) % 256) as u8,
            ((b0 + (x + y) / 5) % 256) as u8,
        ]
    })
}

fn live_frame(n: u32) -> Vec<u8> {
    let bar = (n * 8) % 640;
    jpeg(640, 426, |x, y| {
        if x.abs_diff(bar) < 12 {
            [255, 210, 40]
        } else {
            [(x / 3) as u8, (y / 2) as u8, 90]
        }
    })
}

fn ptp_str(s: &str) -> Vec<u8> {
    if s.is_empty() {
        return vec![0];
    }
    let u: Vec<u16> = s.encode_utf16().chain([0]).collect();
    let mut v = vec![u.len() as u8];
    v.extend(u.iter().flat_map(|x| x.to_le_bytes()));
    v
}

fn arr16(v: &[u16]) -> Vec<u8> {
    let mut o = (v.len() as u32).to_le_bytes().to_vec();
    o.extend(v.iter().flat_map(|x| x.to_le_bytes()));
    o
}

fn enc(dt: u16, v: i64) -> Vec<u8> {
    match dt {
        dtc::INT8 | dtc::UINT8 => vec![v as u8],
        dtc::INT16 | dtc::UINT16 => (v as u16).to_le_bytes().to_vec(),
        _ => (v as u32).to_le_bytes().to_vec(),
    }
}

fn dec(dt: u16, b: &[u8]) -> i64 {
    match dt {
        dtc::INT8 => b.first().map(|&x| x as i8 as i64).unwrap_or(0),
        dtc::UINT8 => b.first().map(|&x| x as i64).unwrap_or(0),
        dtc::INT16 if b.len() >= 2 => i16::from_le_bytes([b[0], b[1]]) as i64,
        dtc::UINT16 if b.len() >= 2 => u16::from_le_bytes([b[0], b[1]]) as i64,
        _ if b.len() >= 4 => u32::from_le_bytes([b[0], b[1], b[2], b[3]]) as i64,
        _ => 0,
    }
}

impl MockCamera {
    pub fn new(pictures: usize) -> Self {
        let mut props = HashMap::new();
        let mut p = |code, datatype, value, values: &[i64], writable| {
            props.insert(
                code,
                Prop {
                    datatype,
                    value,
                    values: values.to_vec(),
                    writable,
                },
            );
        };
        p(prop::BATTERY_LEVEL, dtc::UINT16, 3, &[1, 2, 3], false);
        p(
            prop::EXPOSURE_PROGRAM_MODE,
            dtc::UINT16,
            1,
            &[1, 2, 3, 4],
            true,
        );
        p(
            prop::F_NUMBER,
            dtc::UINT16,
            560,
            &[140, 200, 280, 400, 560, 800, 1100, 1600],
            true,
        );
        p(
            0x500D,
            dtc::UINT32,
            80,
            &[10000, 5000, 2000, 1000, 500, 250, 125, 80, 40, 20, 10, 5],
            true,
        );
        p(
            prop::EXPOSURE_INDEX,
            dtc::UINT32,
            400,
            &[160, 200, 400, 800, 1600, 3200, 6400, 12800],
            true,
        );
        p(
            prop::EXPOSURE_BIAS,
            dtc::INT16,
            0,
            &[
                -2000, -1667, -1333, -1000, -667, -333, 0, 333, 667, 1000, 1333, 1667, 2000,
            ],
            true,
        );
        p(prop::WHITE_BALANCE, dtc::UINT16, 2, &[2, 4, 5, 6, 7], true);
        p(prop::FOCUS_MODE, dtc::UINT16, 2, &[1, 2], true);
        p(prop::FUJI_CAMERA_STATE, dtc::UINT16, 0, &[], true);
        p(prop::FUJI_FUNCTION_MODE, dtc::UINT16, 0, &[], true);
        p(prop::FUJI_VERSION_PHOTO_VIEW, dtc::UINT32, 5, &[], true);
        p(prop::FUJI_VERSION_REMOTE, dtc::UINT32, 0x2000C, &[], true);
        let pictures = (0..pictures as u32).map(picture).collect();
        MockCamera {
            state: Arc::new(Mutex::new(State {
                pictures,
                props,
                changes: vec![],
                events: None,
            })),
            liveview_on: Arc::new(AtomicBool::new(false)),
            frame_no: Arc::new(AtomicU32::new(0)),
        }
    }

    /// Serve forever on `ip` (use 127.0.0.1 and `--ip 127.0.0.1` in the client).
    pub fn serve(self, ip: IpAddr) -> Result<()> {
        let cmd = TcpListener::bind(SocketAddr::new(ip, ptp::CMD_PORT)).context("bind 55740")?;
        let evl = TcpListener::bind(SocketAddr::new(ip, ptp::EVENT_PORT)).context("bind 55741")?;
        let lvl =
            TcpListener::bind(SocketAddr::new(ip, ptp::LIVEVIEW_PORT)).context("bind 55742")?;
        info!("mock camera listening on {ip}:55740-55742");

        let state = self.state.clone();
        thread::spawn(move || {
            for s in evl.incoming().flatten() {
                debug!("event channel connected");
                state.lock().unwrap().events = Some(s);
            }
        });
        let (on, frame_no) = (self.liveview_on.clone(), self.frame_no.clone());
        thread::spawn(move || {
            for mut s in lvl.incoming().flatten() {
                debug!("live view connected");
                let (on, frame_no) = (on.clone(), frame_no.clone());
                thread::spawn(move || {
                    let start = Instant::now();
                    while on.load(Ordering::Relaxed) {
                        let n = frame_no.fetch_add(1, Ordering::Relaxed);
                        let jpg = live_frame(n);
                        // u32 length, 12-byte container header, 2 vendor bytes, JPEG
                        let mut pkt = Vec::new();
                        let total = 4 + HEADER_LEN + 2 + jpg.len();
                        pkt.extend((total as u32).to_le_bytes());
                        pkt.extend(((total - 4) as u32).to_le_bytes());
                        pkt.extend(2u16.to_le_bytes());
                        pkt.extend(0x9022u16.to_le_bytes());
                        pkt.extend(n.to_le_bytes());
                        pkt.extend([0, 0]);
                        pkt.extend(jpg);
                        if s.write_all(&pkt).is_err() {
                            break;
                        }
                        thread::sleep(Duration::from_millis(66));
                    }
                    debug!("live view stopped after {:?}", start.elapsed());
                });
            }
        });

        for s in cmd.incoming() {
            let s = match s {
                Ok(s) => s,
                Err(e) => {
                    warn!("accept: {e}");
                    continue;
                }
            };
            info!("client connected from {:?}", s.peer_addr().ok());
            if let Err(e) = self.session(s) {
                info!("session ended: {e:#}");
            }
            self.liveview_on.store(false, Ordering::Relaxed);
        }
        Ok(())
    }

    fn session(&self, mut s: TcpStream) -> Result<()> {
        let mut init = [0u8; ptp::INIT_REQUEST_LEN];
        s.read_exact(&mut init)?;
        if init[0..4] != [0x52, 0, 0, 0] || init[8..12] != [0xf2, 0xe4, 0x53, 0x8f] {
            bail!("bad init request");
        }
        let units: Vec<u16> = init[28..]
            .as_chunks::<2>()
            .0
            .iter()
            .map(|c| u16::from_le_bytes([c[0], c[1]]))
            .take_while(|&u| u != 0)
            .collect();
        info!("client name {:?}", String::from_utf16_lossy(&units));
        let mut ack = vec![0u8; ptp::INIT_ACK_LEN];
        ack[0..4].copy_from_slice(&(ptp::INIT_ACK_LEN as u32).to_le_bytes());
        ack[4..8].copy_from_slice(&ptp::INIT_COMMAND_ACK.to_le_bytes());
        ack[8..12].copy_from_slice(&1u32.to_le_bytes());
        ack[12..28].copy_from_slice(&ptp::EXPECTED_CAMERA_GUID);
        for (i, u) in "X-MOCK".encode_utf16().enumerate() {
            ack[28 + i * 2..30 + i * 2].copy_from_slice(&u.to_le_bytes());
        }
        s.write_all(&ack)?;

        loop {
            let mut hb = [0u8; HEADER_LEN];
            s.read_exact(&mut hb)?;
            let h = Header::parse(&hb);
            let mut payload = vec![0u8; h.payload_len()];
            s.read_exact(&mut payload)?;
            if h.kind != ContainerType::Command as u16 {
                bail!("expected command, got type {}", h.kind);
            }
            let params = ptp::decode_params(&payload);
            let p0 = params.first().copied().unwrap_or(0);
            debug!("mock <- {} {:x?}", op::name(h.code), params);

            let reply: Result<Option<Vec<u8>>, u16> = match h.code {
                op::OPEN_SESSION | op::CLOSE_SESSION => Ok(None),
                op::GET_DEVICE_INFO => Ok(Some(self.device_info())),
                op::GET_STORAGE_IDS => {
                    let mut v = 1u32.to_le_bytes().to_vec();
                    v.extend(0x0001_0001u32.to_le_bytes());
                    Ok(Some(v))
                }
                op::GET_STORAGE_INFO => {
                    let mut v = vec![];
                    v.extend(4u16.to_le_bytes());
                    v.extend(2u16.to_le_bytes());
                    v.extend(0u16.to_le_bytes());
                    v.extend(64_000_000_000u64.to_le_bytes());
                    v.extend(40_000_000_000u64.to_le_bytes());
                    v.extend(1234u32.to_le_bytes());
                    v.extend(ptp_str("SD1"));
                    v.extend(ptp_str(""));
                    Ok(Some(v))
                }
                // Real Fuji cameras may not support this over Wi-Fi; exercise the fallback.
                op::GET_OBJECT_HANDLES => Err(rc::OPERATION_NOT_SUPPORTED),
                op::GET_OBJECT_INFO => self
                    .object_info(p0)
                    .ok_or(rc::INVALID_OBJECT_HANDLE)
                    .map(Some),
                op::GET_OBJECT => self.picture(p0).ok_or(rc::INVALID_OBJECT_HANDLE).map(Some),
                op::GET_PARTIAL_OBJECT => self
                    .picture(p0)
                    .map(|p| {
                        let off = (params.get(1).copied().unwrap_or(0) as usize).min(p.len());
                        let n = (params.get(2).copied().unwrap_or(0) as usize).min(p.len() - off);
                        p[off..off + n].to_vec()
                    })
                    .ok_or(rc::INVALID_OBJECT_HANDLE)
                    .map(Some),
                op::GET_THUMB => self
                    .picture(p0)
                    .map(|_| jpeg(160, 107, |x, y| [(x + p0 * 40) as u8, (y * 2) as u8, 120]))
                    .ok_or(rc::INVALID_OBJECT_HANDLE)
                    .map(Some),
                op::GET_DEVICE_PROP_VALUE => self.prop_value(p0 as u16).map(Some),
                op::GET_DEVICE_PROP_DESC => self.prop_desc(p0 as u16).map(Some),
                op::SET_DEVICE_PROP_VALUE => {
                    let mut hb = [0u8; HEADER_LEN];
                    s.read_exact(&mut hb)?;
                    let dh = Header::parse(&hb);
                    let mut data = vec![0u8; dh.payload_len()];
                    s.read_exact(&mut data)?;
                    self.set_prop(p0 as u16, &data).map(|_| None)
                }
                op::INITIATE_OPEN_CAPTURE => {
                    self.liveview_on.store(true, Ordering::Relaxed);
                    Ok(None)
                }
                op::TERMINATE_OPEN_CAPTURE => {
                    self.liveview_on.store(false, Ordering::Relaxed);
                    Ok(None)
                }
                op::INITIATE_CAPTURE => {
                    self.capture();
                    Ok(None)
                }
                op::FUJI_S1_LOCK | op::FUJI_S1_UNLOCK => Ok(None),
                _ => Err(rc::OPERATION_NOT_SUPPORTED),
            };
            match reply {
                Ok(data) => {
                    if let Some(d) = data {
                        s.write_all(&ptp::encode(ContainerType::Data, h.code, h.tid, &d))?;
                    }
                    s.write_all(&ptp::encode(ContainerType::Response, rc::OK, h.tid, &[]))?;
                    if h.code == op::CLOSE_SESSION {
                        return Ok(());
                    }
                }
                Err(code) => {
                    debug!("mock -> {}", rc::name(code));
                    s.write_all(&ptp::encode(ContainerType::Response, code, h.tid, &[]))?;
                }
            }
        }
    }

    fn device_info(&self) -> Vec<u8> {
        let st = self.state.lock().unwrap();
        let mut props: Vec<u16> = st.props.keys().copied().collect();
        props.sort();
        let mut v = vec![];
        v.extend(100u16.to_le_bytes());
        v.extend(6u32.to_le_bytes());
        v.extend(100u16.to_le_bytes());
        v.extend(ptp_str("fujifilm.co.jp: 1.0;"));
        v.extend(0u16.to_le_bytes());
        v.extend(arr16(&[
            op::GET_DEVICE_INFO,
            op::OPEN_SESSION,
            op::CLOSE_SESSION,
            op::GET_STORAGE_IDS,
            op::GET_STORAGE_INFO,
            op::GET_OBJECT_INFO,
            op::GET_OBJECT,
            op::GET_THUMB,
            op::INITIATE_CAPTURE,
            op::GET_DEVICE_PROP_DESC,
            op::GET_DEVICE_PROP_VALUE,
            op::SET_DEVICE_PROP_VALUE,
            op::TERMINATE_OPEN_CAPTURE,
            op::GET_PARTIAL_OBJECT,
            op::INITIATE_OPEN_CAPTURE,
            op::FUJI_S1_LOCK,
            op::FUJI_S1_UNLOCK,
        ]));
        v.extend(arr16(&[0x4002, 0x4006]));
        v.extend(arr16(&props));
        v.extend(arr16(&[0x3801]));
        v.extend(arr16(&[0x3801]));
        v.extend(ptp_str("FUJIFILM"));
        v.extend(ptp_str("X-MOCK"));
        v.extend(ptp_str("1.00"));
        v.extend(ptp_str("0123456789"));
        v
    }

    fn picture(&self, h: u32) -> Option<Vec<u8>> {
        let st = self.state.lock().unwrap();
        st.pictures.get((h as usize).checked_sub(1)?).cloned()
    }

    fn object_info(&self, h: u32) -> Option<Vec<u8>> {
        let size = self.picture(h)?.len() as u32;
        let mut v = vec![];
        v.extend(0x0001_0001u32.to_le_bytes());
        v.extend(0x3801u16.to_le_bytes());
        v.extend(0u16.to_le_bytes());
        v.extend(size.to_le_bytes());
        v.extend(0x3801u16.to_le_bytes());
        v.extend(4000u32.to_le_bytes());
        v.extend(160u32.to_le_bytes());
        v.extend(107u32.to_le_bytes());
        v.extend(640u32.to_le_bytes());
        v.extend(426u32.to_le_bytes());
        v.extend(24u32.to_le_bytes());
        v.extend(0u32.to_le_bytes());
        v.extend(0u16.to_le_bytes());
        v.extend(0u32.to_le_bytes());
        v.extend(h.to_le_bytes());
        v.extend(ptp_str(&format!("DSCF{h:04}.JPG")));
        v.extend(ptp_str(&format!("20261103T12{:02}00", h % 60)));
        v.extend(ptp_str(""));
        v.extend(ptp_str(""));
        Some(v)
    }

    fn prop_value(&self, code: u16) -> Result<Vec<u8>, u16> {
        let mut st = self.state.lock().unwrap();
        match code {
            prop::FUJI_OBJECT_COUNT => Ok((st.pictures.len() as u32).to_le_bytes().to_vec()),
            prop::FUJI_EVENTS_LIST => {
                let ch = std::mem::take(&mut st.changes);
                let mut v = (ch.len() as u16).to_le_bytes().to_vec();
                for (c, val) in ch {
                    v.extend(c.to_le_bytes());
                    v.extend(val.to_le_bytes());
                }
                Ok(v)
            }
            _ => st
                .props
                .get(&code)
                .map(|p| enc(p.datatype, p.value))
                .ok_or(rc::DEVICE_PROP_NOT_SUPPORTED),
        }
    }

    fn prop_desc(&self, code: u16) -> Result<Vec<u8>, u16> {
        let st = self.state.lock().unwrap();
        let p = st.props.get(&code).ok_or(rc::DEVICE_PROP_NOT_SUPPORTED)?;
        let mut v = vec![];
        v.extend(code.to_le_bytes());
        v.extend(p.datatype.to_le_bytes());
        v.push(p.writable as u8);
        v.extend(enc(p.datatype, p.value));
        v.extend(enc(p.datatype, p.value));
        if p.values.is_empty() {
            v.push(0);
        } else {
            v.push(2);
            v.extend((p.values.len() as u16).to_le_bytes());
            for &x in &p.values {
                v.extend(enc(p.datatype, x));
            }
        }
        Ok(v)
    }

    fn set_prop(&self, code: u16, data: &[u8]) -> Result<(), u16> {
        let mut st = self.state.lock().unwrap();
        let p = st
            .props
            .get_mut(&code)
            .ok_or(rc::DEVICE_PROP_NOT_SUPPORTED)?;
        if !p.writable {
            return Err(rc::ACCESS_DENIED);
        }
        let v = dec(p.datatype, data);
        if !p.values.is_empty() && !p.values.contains(&v) {
            return Err(rc::INVALID_DEVICE_PROP_VALUE);
        }
        p.value = v;
        info!("mock: {} = {v}", prop::name(code));
        st.changes.push((code, v as u32));
        Ok(())
    }

    fn capture(&self) {
        let mut st = self.state.lock().unwrap();
        let n = st.pictures.len() as u32 + 1;
        st.pictures.push(picture(n * 7));
        st.changes.push((prop::FUJI_OBJECT_COUNT, n));
        if let Some(e) = st.events.as_mut() {
            let _ = e.write_all(&ptp::encode(
                ContainerType::Event,
                0x4002,
                0,
                &n.to_le_bytes(),
            ));
        }
        info!("mock: captured picture {n}");
    }
}
