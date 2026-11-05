//! High-level camera API.

use anyhow::{Context, Result, bail};
use log::{debug, info, warn};
use std::io::{Read, Write};
use std::net::{IpAddr, SocketAddr, TcpStream};
use std::thread::sleep;
use std::time::{Duration, Instant};

use crate::dataset::{
    DeviceInfo, ObjectInfo, PropDesc, PropValue, StorageInfo, encode_value, parse_fuji_events,
};
use crate::ptp::{self, app_version, function_mode, op, prop, rc};
use crate::transport::{self, InitAck, PtpError, Response, Transport, hex, ptp_code};

#[derive(Debug, Clone)]
pub struct ConnectOptions {
    pub ip: IpAddr,
    /// Client name shown on the camera (max 26 UTF-16 units).
    pub name: String,
    /// Override the client GUID. Defaults to the app's scheme (fixed prefix + local IPv4).
    pub guid: Option<[u8; 16]>,
    pub connect_timeout: Duration,
    /// Socket read/write timeout. The app uses 30 s for commands.
    pub io_timeout: Duration,
    /// How long to wait for the camera to answer the handshake (user may need to confirm).
    pub handshake_timeout: Duration,
    /// Number of Init attempts while the camera reports DeviceBusy (app: 6).
    pub busy_attempts: u32,
}

impl Default for ConnectOptions {
    fn default() -> Self {
        ConnectOptions {
            ip: ptp::DEFAULT_CAMERA_IP.parse().unwrap(),
            name: default_client_name(),
            guid: None,
            connect_timeout: Duration::from_secs(10),
            io_timeout: Duration::from_secs(30),
            handshake_timeout: Duration::from_secs(120),
            busy_attempts: 6,
        }
    }
}

pub fn default_client_name() -> String {
    let host = std::fs::read_to_string("/proc/sys/kernel/hostname")
        .map(|s| s.trim().to_string())
        .unwrap_or_default();
    if host.is_empty() {
        "openFuji".into()
    } else {
        host.chars().take(ptp::INIT_NAME_MAX_CHARS).collect()
    }
}

/// What the client wants to do; decides the function-mode handshake.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Mode {
    /// Remote shooting + live view (`FunctionMode = 5`, `VersionRemote = 0x2000C`).
    Remote,
    /// Browse/import images on the card (`FunctionMode = 2`, `VersionPhotoView = 5`).
    PhotoView,
    /// "Ver1.2" browse mode used by some newer bodies (`FunctionMode = 9`).
    PhotoViewV12,
}

pub struct Camera {
    t: Transport,
    ip: IpAddr,
    pub ack: InitAck,
    io_timeout: Duration,
}

impl Camera {
    /// Connect to the command port, register, and open a PTP session.
    ///
    /// Like the app (`CameraConnectActivity.openCamera`: 7 tries of 5 s, each
    /// on a new socket), a handshake that gets no answer is retried on a fresh
    /// connection: right after joining its Wi-Fi the camera accepts TCP before
    /// its remote service is ready and never answers that first connection.
    /// Retries continue until `handshake_timeout` (the user may also need to
    /// confirm on the camera's screen).
    pub fn connect(opts: &ConnectOptions) -> Result<Self> {
        let addr = SocketAddr::new(opts.ip, ptp::CMD_PORT);
        let deadline = Instant::now() + opts.handshake_timeout;
        info!(
            "waiting for the camera to accept {:?} (confirm on the camera's screen if it asks)",
            opts.name
        );
        let mut attempt = 0u32;
        let (stream, ack) = loop {
            attempt += 1;
            info!("connecting to {addr} (attempt {attempt})");
            let started = Instant::now();
            match Self::try_register(opts, addr) {
                Ok(ok) => break ok,
                Err(e) if Self::retryable(&e) && Instant::now() < deadline => {
                    warn!("no handshake answer yet ({e:#}); retrying on a new connection");
                    // Don't hammer a camera that refuses or resets immediately.
                    let spent = started.elapsed();
                    if spent < Duration::from_secs(1) {
                        sleep(Duration::from_secs(1) - spent);
                    }
                }
                Err(e) if Self::retryable(&e) => {
                    return Err(e.context(format!(
                        "the camera did not answer the handshake within {}s; check the camera's \
                         screen for a confirmation prompt",
                        opts.handshake_timeout.as_secs()
                    )));
                }
                Err(e) => return Err(e),
            }
        };
        stream.set_read_timeout(Some(opts.io_timeout))?;
        info!("registered with camera {:?}", ack.camera_name);

        let mut cam = Camera {
            t: Transport::new(stream),
            ip: opts.ip,
            ack,
            io_timeout: opts.io_timeout,
        };
        cam.open_session()?;
        Ok(cam)
    }

    /// How long one handshake attempt waits for the ack (app: 5 s).
    const HANDSHAKE_ATTEMPT: Duration = Duration::from_secs(5);

    fn try_register(opts: &ConnectOptions, addr: SocketAddr) -> Result<(TcpStream, InitAck)> {
        let mut stream =
            TcpStream::connect_timeout(&addr, opts.connect_timeout).with_context(|| {
                format!("cannot reach camera at {addr} (is your PC on the camera's Wi-Fi?)")
            })?;
        stream.set_nodelay(true).ok();
        stream.set_write_timeout(Some(opts.io_timeout))?;
        stream.set_read_timeout(Some(Self::HANDSHAKE_ATTEMPT.min(opts.handshake_timeout)))?;
        let guid = match opts.guid {
            Some(g) => g,
            None => match stream.local_addr()?.ip() {
                IpAddr::V4(v4) => ptp::app_guid(v4.octets()),
                IpAddr::V6(_) => ptp::app_guid([0; 4]),
            },
        };
        let ack = transport::register(&mut stream, &guid, &opts.name, opts.busy_attempts)?;
        Ok((stream, ack))
    }

    /// Timeouts, a closed/reset connection or a refused connect: worth another try.
    fn retryable(e: &anyhow::Error) -> bool {
        use std::io::ErrorKind::*;
        e.chain().any(|c| {
            c.downcast_ref::<std::io::Error>().is_some_and(|io| {
                matches!(
                    io.kind(),
                    WouldBlock
                        | TimedOut
                        | UnexpectedEof
                        | ConnectionReset
                        | ConnectionAborted
                        | ConnectionRefused
                        | BrokenPipe
                )
            })
        })
    }

    fn open_session(&mut self) -> Result<()> {
        // Same as CPTPCommand::OpenSession: reset tid, OpenSession(1); if the
        // camera says a session is already open, close it and try again.
        self.t.reset_tid();
        match self.op(op::OPEN_SESSION, &[1]) {
            Ok(_) => Ok(()),
            Err(e) if ptp_code(&e) == Some(rc::SESSION_ALREADY_OPEN) => {
                warn!("session already open, closing and reopening");
                let _ = self.op(op::CLOSE_SESSION, &[]);
                self.t.reset_tid();
                self.op(op::OPEN_SESSION, &[1]).map(|_| ())
            }
            Err(e) => Err(e),
        }
    }

    pub fn ip(&self) -> IpAddr {
        self.ip
    }

    // ---- generic transactions -------------------------------------------

    fn check(code: u16, r: Response) -> Result<Response> {
        if r.code == rc::OK {
            Ok(r)
        } else {
            Err(PtpError {
                operation: code,
                code: r.code,
            }
            .into())
        }
    }

    /// Transaction without data phase.
    pub fn op(&mut self, code: u16, params: &[u32]) -> Result<Response> {
        let r = self.t.transaction(code, params, None, &mut |_| Ok(()))?;
        Self::check(code, r)
    }

    /// Transaction with camera→host data, collected into memory.
    pub fn op_read(&mut self, code: u16, params: &[u32]) -> Result<Vec<u8>> {
        let mut out = Vec::new();
        let r = self.t.transaction(code, params, None, &mut |b| {
            out.extend_from_slice(b);
            Ok(())
        })?;
        Self::check(code, r)?;
        Ok(out)
    }

    /// Transaction with camera→host data streamed to `sink`.
    pub fn op_read_with(
        &mut self,
        code: u16,
        params: &[u32],
        sink: &mut dyn FnMut(&[u8]) -> Result<()>,
    ) -> Result<Response> {
        let r = self.t.transaction(code, params, None, sink)?;
        Self::check(code, r)
    }

    /// Transaction with host→camera data.
    pub fn op_write(&mut self, code: u16, params: &[u32], data: &[u8]) -> Result<Response> {
        let r = self
            .t
            .transaction(code, params, Some(data), &mut |_| Ok(()))?;
        Self::check(code, r)
    }

    /// Raw transaction that does not fail on non-OK responses.
    pub fn raw(
        &mut self,
        code: u16,
        params: &[u32],
        data_out: Option<&[u8]>,
    ) -> Result<(Response, Vec<u8>)> {
        let mut out = Vec::new();
        let r = self.t.transaction(code, params, data_out, &mut |b| {
            out.extend_from_slice(b);
            Ok(())
        })?;
        Ok((r, out))
    }

    // ---- device ------------------------------------------------------------

    pub fn device_info(&mut self) -> Result<DeviceInfo> {
        DeviceInfo::parse(&self.op_read(op::GET_DEVICE_INFO, &[])?)
    }

    pub fn close(mut self) -> Result<()> {
        self.op(op::CLOSE_SESSION, &[]).map(|_| ())
    }

    // ---- properties --------------------------------------------------------

    pub fn prop_desc(&mut self, code: u16) -> Result<PropDesc> {
        PropDesc::parse(&self.op_read(op::GET_DEVICE_PROP_DESC, &[code as u32])?)
    }

    pub fn prop_raw(&mut self, code: u16) -> Result<Vec<u8>> {
        self.op_read(op::GET_DEVICE_PROP_VALUE, &[code as u32])
    }

    pub fn prop_u16(&mut self, code: u16) -> Result<u16> {
        let b = self.prop_raw(code)?;
        if b.len() < 2 {
            bail!("property 0x{code:04x}: short value {}", hex(&b));
        }
        Ok(u16::from_le_bytes([b[0], b[1]]))
    }

    pub fn prop_u32(&mut self, code: u16) -> Result<u32> {
        let b = self.prop_raw(code)?;
        match b.len() {
            0 | 1 => bail!("property 0x{code:04x}: short value {}", hex(&b)),
            2 | 3 => Ok(u16::from_le_bytes([b[0], b[1]]) as u32),
            _ => Ok(u32::from_le_bytes([b[0], b[1], b[2], b[3]])),
        }
    }

    pub fn set_prop_raw(&mut self, code: u16, data: &[u8]) -> Result<()> {
        self.op_write(op::SET_DEVICE_PROP_VALUE, &[code as u32], data)
            .map(|_| ())
    }

    pub fn set_prop_u16(&mut self, code: u16, v: u16) -> Result<()> {
        self.set_prop_raw(code, &v.to_le_bytes())
    }

    pub fn set_prop_u32(&mut self, code: u16, v: u32) -> Result<()> {
        self.set_prop_raw(code, &v.to_le_bytes())
    }

    /// Set a property using the data type from its descriptor.
    pub fn set_prop_typed(&mut self, code: u16, v: &PropValue) -> Result<()> {
        let desc = self.prop_desc(code)?;
        let data = encode_value(desc.datatype, v)?;
        self.set_prop_raw(code, &data)
    }

    /// Fuji change list (property 0xD212). The app polls this instead of using PTP events.
    pub fn poll_events(&mut self) -> Result<Vec<(u16, u32)>> {
        parse_fuji_events(&self.prop_raw(prop::FUJI_EVENTS_LIST)?)
    }

    // ---- Fuji function mode -----------------------------------------------

    /// Tell the camera which function the client is going to use, then
    /// exchange the function version (as `CameraConnectActivity.startFunction`).
    pub fn set_mode(&mut self, mode: Mode) -> Result<()> {
        let state = match self.prop_u16(prop::FUJI_CAMERA_STATE) {
            Ok(s) => {
                info!("camera state (0xDF00) = {s}");
                Some(s)
            }
            Err(e) => {
                debug!("camera state unavailable: {e}");
                None
            }
        };
        // For browsing, the app picks mode and version from the camera state
        // (CameraConnectActivity.startFunction, `m_MsgArg1`): state 1/2 →
        // mode 2 with PHOTO_VIEWER_VERSION_2; state 4/6 → the "Ver1.2" mode 9
        // with PHOTO_VIEWER_VERSION. Sending the wrong pair makes the camera
        // ask for an app update.
        let mode = match (mode, state) {
            (Mode::PhotoView, Some(4 | 6)) => Mode::PhotoViewV12,
            _ => mode,
        };
        let (fmode, vprop, version) = match mode {
            Mode::Remote => (
                function_mode::REMOTE,
                prop::FUJI_VERSION_REMOTE,
                app_version::REMOTE_MODE,
            ),
            Mode::PhotoView => (
                function_mode::PHOTO_VIEW,
                prop::FUJI_VERSION_PHOTO_VIEW,
                if matches!(state, Some(1 | 2)) {
                    app_version::PHOTO_VIEWER_2
                } else {
                    app_version::PHOTO_VIEWER
                },
            ),
            Mode::PhotoViewV12 => (
                function_mode::PHOTO_VIEW_V12,
                prop::FUJI_VERSION_PHOTO_VIEW,
                app_version::PHOTO_VIEWER,
            ),
        };
        self.set_prop_u16(prop::FUJI_FUNCTION_MODE, fmode)
            .context("setting Fuji function mode (0xDF01)")?;
        match self.prop_u32(vprop) {
            Ok(v) => info!("camera {} = 0x{v:x}", prop::name(vprop)),
            Err(e) => debug!("reading {}: {e}", prop::name(vprop)),
        }
        if let Err(e) = self.set_prop_u32(vprop, version) {
            warn!("setting {} = 0x{version:x} failed: {e}", prop::name(vprop));
        }
        Ok(())
    }

    // ---- storage & objects ---------------------------------------------------

    pub fn storage_ids(&mut self) -> Result<Vec<u32>> {
        let b = self.op_read(op::GET_STORAGE_IDS, &[])?;
        crate::dataset::Reader::new(&b).array_u32()
    }

    pub fn storage_info(&mut self, id: u32) -> Result<StorageInfo> {
        StorageInfo::parse(&self.op_read(op::GET_STORAGE_INFO, &[id])?)
    }

    /// List object handles. Falls back to `1..=ObjectCount` (prop 0xD222),
    /// which is how Fuji's Wi-Fi viewer addresses images, if the camera does
    /// not implement GetObjectHandles in this mode.
    pub fn object_handles(&mut self) -> Result<Vec<u32>> {
        match self.op_read(op::GET_OBJECT_HANDLES, &[0xFFFF_FFFF, 0, 0]) {
            Ok(b) => crate::dataset::Reader::new(&b).array_u32(),
            Err(e) => {
                debug!("GetObjectHandles failed ({e}), using Fuji object count");
                let n = self.prop_u32(prop::FUJI_OBJECT_COUNT)?;
                Ok((1..=n).collect())
            }
        }
    }

    pub fn object_info(&mut self, handle: u32) -> Result<ObjectInfo> {
        ObjectInfo::parse(&self.op_read(op::GET_OBJECT_INFO, &[handle])?)
    }

    /// Download an object, streaming it to `out`. `progress(done)` is called per chunk.
    pub fn download(
        &mut self,
        handle: u32,
        out: &mut dyn Write,
        progress: &mut dyn FnMut(u64),
    ) -> Result<u64> {
        let mut done = 0u64;
        self.op_read_with(op::GET_OBJECT, &[handle], &mut |b| {
            out.write_all(b)?;
            done += b.len() as u64;
            progress(done);
            Ok(())
        })?;
        Ok(done)
    }

    /// Download with GetPartialObject in `chunk`-sized pieces.
    pub fn download_partial(
        &mut self,
        handle: u32,
        size: u64,
        chunk: u32,
        out: &mut dyn Write,
        progress: &mut dyn FnMut(u64),
    ) -> Result<u64> {
        let mut done = 0u64;
        while done < size {
            let want = chunk.min((size - done).min(u32::MAX as u64) as u32);
            let mut got = 0u64;
            self.op_read_with(
                op::GET_PARTIAL_OBJECT,
                &[handle, done as u32, want],
                &mut |b| {
                    out.write_all(b)?;
                    got += b.len() as u64;
                    Ok(())
                },
            )?;
            if got == 0 {
                bail!("camera returned no data at offset {done}");
            }
            done += got;
            progress(done);
        }
        Ok(done)
    }

    pub fn thumbnail(&mut self, handle: u32) -> Result<Vec<u8>> {
        self.op_read(op::GET_THUMB, &[handle])
    }

    pub fn delete(&mut self, handle: u32) -> Result<()> {
        self.op(op::DELETE_OBJECT, &[handle, 0]).map(|_| ())
    }

    // ---- remote shooting ---------------------------------------------------

    /// InitiateOpenCapture, then connect the event and live-view sockets
    /// (what `FTL_PTP_InitiateOpenCapture` does). Returns the open-capture
    /// transaction id, needed by [`Camera::stop_liveview`].
    pub fn start_liveview(&mut self) -> Result<(u32, LiveView, EventChannel)> {
        self.op(op::INITIATE_OPEN_CAPTURE, &[0, 0])?;
        let tid = self.t.last_tid();
        let ev = connect_retry(SocketAddr::new(self.ip, ptp::EVENT_PORT), self.io_timeout)
            .context("connecting event channel")?;
        let lv = connect_retry(
            SocketAddr::new(self.ip, ptp::LIVEVIEW_PORT),
            self.io_timeout,
        )
        .context("connecting live-view channel")?;
        Ok((tid, LiveView { stream: lv }, EventChannel { stream: ev }))
    }

    /// TerminateOpenCapture. Keep reading the live-view and event sockets
    /// until this returns: the camera may block on a full live-view socket
    /// and then never answer. Waits at most 5 s, since this is only sent when
    /// leaving remote mode or disconnecting anyway.
    pub fn stop_liveview(&mut self, open_capture_tid: u32) -> Result<()> {
        let stream = self.t.stream();
        stream.set_read_timeout(Some(Duration::from_secs(5)))?;
        let r = self
            .op(op::TERMINATE_OPEN_CAPTURE, &[open_capture_tid])
            .map(|_| ());
        self.t.stream().set_read_timeout(Some(self.io_timeout))?;
        r
    }

    /// Release the shutter.
    pub fn capture(&mut self) -> Result<()> {
        self.op(op::INITIATE_CAPTURE, &[0, 0]).map(|_| ())
    }

    /// Half-press (autofocus/AE lock).
    pub fn s1_lock(&mut self) -> Result<()> {
        self.op(op::FUJI_S1_LOCK, &[0]).map(|_| ())
    }

    pub fn s1_unlock(&mut self) -> Result<()> {
        self.op(op::FUJI_S1_UNLOCK, &[]).map(|_| ())
    }
}

/// The app retries the secondary sockets for a while after InitiateOpenCapture.
fn connect_retry(addr: SocketAddr, timeout: Duration) -> Result<TcpStream> {
    let start = Instant::now();
    loop {
        match TcpStream::connect_timeout(&addr, Duration::from_secs(3)) {
            Ok(s) => {
                s.set_nodelay(true).ok();
                s.set_read_timeout(Some(timeout))?;
                debug!("connected {addr}");
                return Ok(s);
            }
            Err(e) if start.elapsed() < Duration::from_secs(5) => {
                debug!("connect {addr}: {e}, retrying");
                sleep(Duration::from_millis(500));
            }
            Err(e) => return Err(e.into()),
        }
    }
}

/// Live-view stream on port 55742. Each packet is `u32 length` + payload;
/// the payload carries a small header followed by a JPEG frame.
pub struct LiveView {
    stream: TcpStream,
}

pub struct Frame {
    /// Bytes preceding the JPEG (vendor header).
    pub header: Vec<u8>,
    pub jpeg: Vec<u8>,
}

impl LiveView {
    pub fn next_frame(&mut self) -> Result<Frame> {
        loop {
            let mut lb = [0u8; 4];
            self.stream
                .read_exact(&mut lb)
                .context("reading live-view packet length")?;
            let len = u32::from_le_bytes(lb) as usize;
            if !(4..=32 * 1024 * 1024).contains(&len) {
                bail!("bad live-view packet length {len}");
            }
            let mut buf = vec![0u8; len - 4];
            self.stream
                .read_exact(&mut buf)
                .context("reading live-view packet")?;
            match buf.windows(3).position(|w| w == [0xff, 0xd8, 0xff]) {
                Some(soi) => {
                    let end = buf
                        .windows(2)
                        .rposition(|w| w == [0xff, 0xd9])
                        .map(|p| p + 2)
                        .filter(|&e| e > soi)
                        .unwrap_or(buf.len());
                    let jpeg = buf[soi..end].to_vec();
                    buf.truncate(soi);
                    return Ok(Frame { header: buf, jpeg });
                }
                None => debug!(
                    "live-view packet without JPEG ({} bytes): {}",
                    buf.len(),
                    hex(&buf[..buf.len().min(32)])
                ),
            }
        }
    }

    pub fn try_clone(&self) -> Result<LiveView> {
        Ok(LiveView {
            stream: self.stream.try_clone()?,
        })
    }

    /// Unblock a reader thread and close the stream.
    pub fn shutdown(&self) {
        let _ = self.stream.shutdown(std::net::Shutdown::Both);
    }
}

#[derive(Debug, Clone)]
pub struct Event {
    pub code: u16,
    pub tid: u32,
    pub params: Vec<u32>,
}

/// Asynchronous PTP events on port 55741 (USB-style event containers).
pub struct EventChannel {
    stream: TcpStream,
}

impl EventChannel {
    pub fn next_event(&mut self) -> Result<Event> {
        let h = Transport::read_header(&mut self.stream)?;
        let mut p = vec![0u8; h.payload_len()];
        self.stream.read_exact(&mut p)?;
        if h.kind != ptp::ContainerType::Event as u16 {
            debug!(
                "event channel: container type {} code 0x{:04x}",
                h.kind, h.code
            );
        }
        Ok(Event {
            code: h.code,
            tid: h.tid,
            params: ptp::decode_params(&p),
        })
    }

    /// Events can arrive any time; disable the read timeout for a background reader.
    pub fn set_blocking_forever(&self) -> Result<()> {
        self.stream.set_read_timeout(None)?;
        Ok(())
    }

    pub fn try_clone(&self) -> Result<EventChannel> {
        Ok(EventChannel {
            stream: self.stream.try_clone()?,
        })
    }

    pub fn shutdown(&self) {
        let _ = self.stream.shutdown(std::net::Shutdown::Both);
    }
}
