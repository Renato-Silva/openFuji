//! Low-level TCP transport: init handshake and PTP transactions on the command channel.

use anyhow::{Context, Result, bail};
use log::{debug, trace, warn};
use std::fmt;
use std::io::{Read, Write};
use std::net::TcpStream;
use std::thread::sleep;
use std::time::Duration;

use crate::ptp::{self, ContainerType, HEADER_LEN, Header, op, rc};

/// Error returned when the camera answers a transaction with a non-OK response code.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PtpError {
    pub operation: u16,
    pub code: u16,
}

impl fmt::Display for PtpError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "{} (0x{:04x}) failed: {} (0x{:04x})",
            op::name(self.operation),
            self.operation,
            rc::name(self.code),
            self.code
        )
    }
}

impl std::error::Error for PtpError {}

/// Returns the PTP response code if `err` is a [`PtpError`].
pub fn ptp_code(err: &anyhow::Error) -> Option<u16> {
    err.downcast_ref::<PtpError>().map(|e| e.code)
}

#[derive(Debug, Clone)]
pub struct Response {
    pub code: u16,
    pub tid: u32,
    pub params: Vec<u32>,
}

#[derive(Debug, Clone)]
pub struct InitAck {
    pub connection_number: u32,
    pub camera_guid: [u8; 16],
    pub camera_name: String,
}

pub(crate) fn hex(b: &[u8]) -> String {
    b.iter()
        .map(|x| format!("{x:02x}"))
        .collect::<Vec<_>>()
        .join(" ")
}

/// Perform the Init_Command_Request / Init_Command_Ack handshake.
///
/// Mirrors `MngPTP::Open()`: on `InitFail(DeviceBusy)` the request is resent on
/// the same socket after 500 ms, up to `attempts` times.
pub fn register(
    stream: &mut TcpStream,
    guid: &[u8; 16],
    name: &str,
    attempts: u32,
) -> Result<InitAck> {
    let req = ptp::encode_init_request(guid, name);
    for attempt in 1..=attempts.max(1) {
        debug!(
            "-> Init_Command_Request (attempt {attempt}) guid={} name={name:?}",
            hex(guid)
        );
        trace!("   {}", hex(&req));
        stream.write_all(&req).context("sending init request")?;

        let mut len_buf = [0u8; 4];
        stream
            .read_exact(&mut len_buf)
            .context("reading init ack length")?;
        let len = u32::from_le_bytes(len_buf) as usize;
        if !(8..=4096).contains(&len) {
            bail!("unexpected init ack length {len}");
        }
        let mut rest = vec![0u8; len - 4];
        stream.read_exact(&mut rest).context("reading init ack")?;
        let mut pkt = len_buf.to_vec();
        pkt.extend_from_slice(&rest);
        trace!("<- {}", hex(&pkt));

        let kind = u32::from_le_bytes(pkt[4..8].try_into().unwrap());
        match kind {
            ptp::INIT_COMMAND_ACK => {
                if pkt.len() != ptp::INIT_ACK_LEN {
                    warn!(
                        "init ack is {} bytes, expected {}",
                        pkt.len(),
                        ptp::INIT_ACK_LEN
                    );
                }
                if pkt.len() < 28 {
                    bail!("init ack too short ({} bytes)", pkt.len());
                }
                let connection_number = u32::from_le_bytes(pkt[8..12].try_into().unwrap());
                let camera_guid: [u8; 16] = pkt[12..28].try_into().unwrap();
                if camera_guid != ptp::EXPECTED_CAMERA_GUID {
                    // The app rejects this; we only warn so other firmware can be tried.
                    warn!(
                        "camera GUID {} differs from the one the app expects",
                        hex(&camera_guid)
                    );
                }
                let name_end = pkt.len().min(60);
                let units: Vec<u16> = pkt[28..name_end]
                    .as_chunks::<2>()
                    .0
                    .iter()
                    .map(|c| u16::from_le_bytes([c[0], c[1]]))
                    .take_while(|&u| u != 0)
                    .collect();
                let camera_name = String::from_utf16_lossy(&units);
                debug!("<- Init_Command_Ack conn={connection_number} camera={camera_name:?}");
                return Ok(InitAck {
                    connection_number,
                    camera_guid,
                    camera_name,
                });
            }
            ptp::INIT_FAIL => {
                let reason = if pkt.len() >= 12 {
                    u32::from_le_bytes(pkt[8..12].try_into().unwrap()) as u16
                } else {
                    0
                };
                if reason == rc::DEVICE_BUSY && attempt < attempts {
                    debug!("<- Init_Fail(DeviceBusy), retrying in 500 ms");
                    sleep(Duration::from_millis(500));
                    continue;
                }
                bail!(
                    "camera rejected connection: Init_Fail {} (0x{reason:04x}). \
                     Make sure the camera is waiting for a smartphone connection.",
                    rc::name(reason)
                );
            }
            other => bail!("unexpected init response type {other}: {}", hex(&pkt)),
        }
    }
    bail!("camera stayed busy after {attempts} attempts")
}

/// A PTP command channel.
pub struct Transport {
    stream: TcpStream,
    tid: u32,
}

impl Transport {
    pub fn new(stream: TcpStream) -> Self {
        Transport { stream, tid: 0 }
    }

    pub fn stream(&self) -> &TcpStream {
        &self.stream
    }

    /// The app resets the transaction counter to 0 right before OpenSession.
    pub fn reset_tid(&mut self) {
        self.tid = 0;
    }

    pub fn last_tid(&self) -> u32 {
        self.tid
    }

    fn next_tid(&mut self) -> u32 {
        self.tid = self.tid.wrapping_add(1);
        self.tid
    }

    pub fn read_header(stream: &mut TcpStream) -> Result<Header> {
        let mut b = [0u8; HEADER_LEN];
        stream
            .read_exact(&mut b)
            .context("reading container header")?;
        let h = Header::parse(&b);
        if (h.length as usize) < HEADER_LEN {
            bail!("invalid container length {} ({})", h.length, hex(&b));
        }
        Ok(h)
    }

    /// Run one transaction. Data sent by the camera is streamed to `sink`.
    pub fn transaction(
        &mut self,
        code: u16,
        params: &[u32],
        data_out: Option<&[u8]>,
        sink: &mut dyn FnMut(&[u8]) -> Result<()>,
    ) -> Result<Response> {
        let tid = self.next_tid();
        let cmd = ptp::encode_command(code, tid, params);
        debug!(
            "-> {} (0x{code:04x}) tid={tid} params={params:x?}",
            op::name(code)
        );
        trace!("   {}", hex(&cmd));
        self.stream.write_all(&cmd).context("sending command")?;

        if let Some(data) = data_out {
            let pkt = ptp::encode(ContainerType::Data, code, tid, data);
            trace!("-> data {}", hex(&pkt[..pkt.len().min(64)]));
            self.stream.write_all(&pkt).context("sending data phase")?;
        }

        loop {
            let h = Self::read_header(&mut self.stream)?;
            match ContainerType::from_u16(h.kind) {
                Some(ContainerType::Data) => {
                    let mut left = h.payload_len();
                    debug!("<- data {left} bytes");
                    let mut buf = vec![0u8; left.min(256 * 1024)];
                    while left > 0 {
                        let n = left.min(buf.len());
                        self.stream
                            .read_exact(&mut buf[..n])
                            .context("reading data phase")?;
                        sink(&buf[..n])?;
                        left -= n;
                    }
                }
                Some(ContainerType::Response) => {
                    let mut p = vec![0u8; h.payload_len()];
                    self.stream.read_exact(&mut p).context("reading response")?;
                    let params = ptp::decode_params(&p);
                    debug!(
                        "<- {} (0x{:04x}) tid={} params={params:x?}",
                        rc::name(h.code),
                        h.code,
                        h.tid
                    );
                    if h.tid != tid {
                        warn!("response transaction id {} != {tid}", h.tid);
                    }
                    return Ok(Response {
                        code: h.code,
                        tid: h.tid,
                        params,
                    });
                }
                _ => {
                    let mut p = vec![0u8; h.payload_len()];
                    self.stream.read_exact(&mut p)?;
                    warn!(
                        "ignoring unexpected container type {} code 0x{:04x}: {}",
                        h.kind,
                        h.code,
                        hex(&p[..p.len().min(64)])
                    );
                }
            }
        }
    }
}
