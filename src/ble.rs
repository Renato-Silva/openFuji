//! Bluetooth LE side of the Fujifilm protocol (pairing and starting the camera's Wi-Fi).
//!
//! Recovered from `BTCamera` / `BTManagerService` in FUJIFILM Camera Remote 4.10.2:
//!
//! * In pairing mode the camera advertises manufacturer data under company id
//!   0x04D8. Byte 0 is a flag byte: bit 0 means 4 extra bytes follow, bit 1 means a
//!   4-byte pairing key follows (at offset 5 if bit 0 is set, else at offset 1).
//! * The app connects, writes that pairing key back to `PAIRING_KEY`, writes its
//!   own name (NUL-terminated) to `DEVICE_NAME` and its BLE protocol version
//!   (u16 = 2) to `BLE_PROTOCOL_VERSION`, and subscribes to camera state
//!   notifications. No OS-level bonding is requested by the app.
//! * To start a function it writes a u16 to `FUNCTION_LAUNCH` (remote = 4,
//!   viewer = 3), waits for the access-point state notification (`AP_STATE`) to
//!   become 1 ("connectable"), then reads the SSID and passphrase.

use anyhow::{Context, Result, anyhow, bail};
use bluer::gatt::remote::Characteristic;
use bluer::{
    Adapter, AdapterEvent, Address, Device, DiscoveryFilter, DiscoveryTransport, Session, Uuid,
};
use futures_util::StreamExt;
use log::{debug, info, warn};
use std::collections::HashMap;
use std::path::PathBuf;
use std::time::Duration;
use tokio::time::{Instant, sleep, timeout};

pub const MANUFACTURER_ID: u16 = 0x04D8;

/// GATT characteristic UUIDs (service in comments).
pub mod uuid {
    use bluer::Uuid;
    // Connected device information service 91F1DE68-DFF6-466E-8B65-FF13B0F16FB8
    pub const PAIRING_KEY: Uuid = Uuid::from_u128(0xABA356EB_9633_4E60_B73F_F52516DBD671);
    pub const DEVICE_NAME: Uuid = Uuid::from_u128(0x85B9163E_62D1_49FF_A6F5_054B4630D4A1);
    pub const BLE_PROTOCOL_VERSION: Uuid = Uuid::from_u128(0xEB4166B0_9CCA_445E_A4E4_75B3817FD57A);
    pub const DEVICE_IDENTIFICATION: Uuid = Uuid::from_u128(0xF557D96B_8284_4667_8793_B971C1DECA2A);
    // Camera control service 6514EB81-4E8F-458D-AA2A-E691336CDFAC
    pub const FUNCTION_LAUNCH: Uuid = Uuid::from_u128(0x600655E6_3637_42F1_8FB2_44EFC5C63B13);
    // Camera setting service 4E941240-D01D-46B9-A5EA-67636806830B
    pub const SSID: Uuid = Uuid::from_u128(0xBF6DC9CF_3606_4EC9_A4C8_D77576E93EA4);
    pub const PASSPHRASE: Uuid = Uuid::from_u128(0xE809256A_915C_4967_92E8_53B7D4CAD213);
    pub const PAIRED_DEVICE_COUNT: Uuid = Uuid::from_u128(0x8814441B_1D7B_4046_891D_D8F80864CC8E);
    // Camera state service 4C0020FE-F3B6-40DE-ACC9-77D129067B14 (notify)
    pub const AP_STATE: Uuid = Uuid::from_u128(0xA68E3F66_0FCC_4395_8D4C_AA980B5877FA);
    pub const TRANSFER_STATE: Uuid = Uuid::from_u128(0xBD17BA04_B76B_4892_A545_B73BA1F74DAE);
    /// Camera asks for the date/time when this becomes 1.
    pub const DATE_SYNC_STATE: Uuid = Uuid::from_u128(0xF9150137_5D40_4801_A8DC_F7FC5B01DA50);
    pub const STATE_AD06: Uuid = Uuid::from_u128(0xAD06C7B7_F41A_46F4_A29A_712055319122);
    pub const STATE_049E: Uuid = Uuid::from_u128(0x049EC406_EF75_4205_A390_08FE209C51F0);
    // Camera information service 117C4142-EDD4-4C77-8696-DD18EEBB770A
    pub const CAMERA_BLE_PROTOCOL_VERSION: Uuid =
        Uuid::from_u128(0x389363E4_712E_4CF2_A72E_BFCF7FB6ADC1);
    pub const CAMERA_MAC: Uuid = Uuid::from_u128(0x49A12959_DFAA_4EB2_89CE_62548AD948F3);
    // Standard
    // Current time service E872B11F-D526-4AE1-9BB4-89A99D48FA59
    pub const CURRENT_DATETIME: Uuid = Uuid::from_u128(0xB9BFD37F_CCAD_4D36_A1EE_018E792B3EDF);
    // Further characteristics the app reads right after the pairing key, in its order.
    pub const READ_BD45: Uuid = Uuid::from_u128(0xBD45F887_A6BE_4CB7_8565_390DF38BF5BF);
    pub const LENS_FIRMWARE: Uuid = Uuid::from_u128(0x68887A7A_10A7_4A4A_AADD_6F9DB1ABAA0E);
    pub const LENS_PRODUCT: Uuid = Uuid::from_u128(0x621A98D8_6314_4C3E_A683_EEA1D6166606);
    pub const READ_9C72: Uuid = Uuid::from_u128(0x9C72C205_5740_4F17_9949_0D3FADF2F67A);
    pub const READ_4BF1: Uuid = Uuid::from_u128(0x4BF16F6D_7264_4E76_B0D2_52CDE16D2C7D);
    pub const READ_7EDE: Uuid = Uuid::from_u128(0x7EDE1988_B27E_43FC_80F4_6FEC994F0552);
    pub const READ_34D8: Uuid = Uuid::from_u128(0x34D8C8DE_E2A9_43FF_822C_7D945DD8D8E1);
    pub const READ_CAE8: Uuid = Uuid::from_u128(0xCAE8C738_4BB6_4605_98C1_58331C87F3B6);
    pub const GAP_DEVICE_NAME: Uuid = Uuid::from_u128(0x00002A00_0000_1000_8000_00805F9B34FB);
    pub const SERIAL_NUMBER: Uuid = Uuid::from_u128(0x00002A25_0000_1000_8000_00805F9B34FB);
    pub const FIRMWARE_REVISION: Uuid = Uuid::from_u128(0x00002A26_0000_1000_8000_00805F9B34FB);
}

/// Values for `FUNCTION_LAUNCH` (BTManagerService.FUNCTION_*).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u16)]
pub enum Function {
    PhotoReceiver = 1,
    GpsAssist = 2,
    CameraViewer = 3,
    Remote = 4,
}

/// AP state notification values.
pub mod ap_state {
    pub const OFF: u8 = 0;
    pub const CONNECTABLE: u8 = 1;
    pub const LAUNCHING: u8 = 2;
    pub const CANCELLED: u8 = 3;
}

/// Extract the pairing key from Fuji manufacturer data (company id already stripped).
pub fn pairing_key_from_adv(data: &[u8]) -> Option<[u8; 4]> {
    let flags = *data.first()?;
    if flags & 0x02 == 0 {
        return None;
    }
    let off = if flags & 0x01 != 0 { 5 } else { 1 };
    data.get(off..off + 4)?.try_into().ok()
}

/// Date/time as the app sends it: u16 LE year, month (1-12), day, hour, minute, second.
pub fn datetime_bytes<T: chrono::Datelike + chrono::Timelike>(t: &T) -> [u8; 7] {
    let y = (t.year() as u16).to_le_bytes();
    [
        y[0],
        y[1],
        t.month() as u8,
        t.day() as u8,
        t.hour() as u8,
        t.minute() as u8,
        t.second() as u8,
    ]
}

fn now_bytes() -> [u8; 7] {
    datetime_bytes(&chrono::Local::now())
}

#[derive(Debug, Clone)]
pub struct Found {
    pub address: Address,
    pub name: String,
    pub manufacturer_data: Vec<u8>,
    pub pairing_key: Option<[u8; 4]>,
    pub rssi: Option<i16>,
}

#[derive(Debug, Clone)]
pub struct WifiInfo {
    pub ssid: String,
    pub passphrase: String,
}

/// A remembered pairing (stored in `~/.config/openfuji/pairings`).
#[derive(Debug, Clone, PartialEq)]
pub struct Pairing {
    pub address: String,
    pub key: [u8; 4],
    pub name: String,
}

fn store_path() -> PathBuf {
    let base = std::env::var_os("XDG_CONFIG_HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|| {
            PathBuf::from(std::env::var("HOME").unwrap_or_else(|_| ".".into())).join(".config")
        });
    base.join("openfuji").join("pairings")
}

pub fn load_pairings() -> Vec<Pairing> {
    let Ok(text) = std::fs::read_to_string(store_path()) else {
        return vec![];
    };
    text.lines()
        .filter_map(|l| {
            let mut it = l.splitn(3, ' ');
            let address = it.next()?.to_string();
            let k = it.next()?;
            let key = (0..4)
                .map(|i| u8::from_str_radix(k.get(i * 2..i * 2 + 2)?, 16).ok())
                .collect::<Option<Vec<u8>>>()?;
            Some(Pairing {
                address,
                key: key.try_into().ok()?,
                name: it.next().unwrap_or("").to_string(),
            })
        })
        .collect()
}

pub fn save_pairing(p: &Pairing) -> Result<()> {
    let mut all: Vec<Pairing> = load_pairings()
        .into_iter()
        .filter(|x| x.address != p.address)
        .collect();
    all.insert(0, p.clone());
    let path = store_path();
    std::fs::create_dir_all(path.parent().unwrap())?;
    let text: String = all
        .iter()
        .map(|p| {
            format!(
                "{} {} {}\n",
                p.address,
                p.key.iter().map(|b| format!("{b:02x}")).collect::<String>(),
                p.name
            )
        })
        .collect();
    std::fs::write(&path, text).with_context(|| format!("writing {}", path.display()))
}

async fn adapter() -> Result<(Session, Adapter)> {
    let session = Session::new()
        .await
        .context("connecting to BlueZ (is bluetoothd running?)")?;
    let adapter = session
        .default_adapter()
        .await
        .context("no Bluetooth adapter")?;
    adapter
        .set_powered(true)
        .await
        .context("powering on Bluetooth")?;
    Ok((session, adapter))
}

/// Describe a sighted device. Unknown devices count only when they carry Fuji
/// manufacturer data; a camera we look for by address counts without it (the
/// X-E3 only includes manufacturer data while on its pairing screen).
async fn describe(device: &Device, wanted: bool) -> Option<Found> {
    let data = device
        .manufacturer_data()
        .await
        .ok()
        .flatten()
        .and_then(|m| m.get(&MANUFACTURER_ID).cloned());
    let data = match data {
        Some(d) => d,
        None if wanted => vec![],
        None => return None,
    };
    Some(Found {
        address: device.address(),
        name: device.name().await.ok().flatten().unwrap_or_default(),
        pairing_key: pairing_key_from_adv(&data),
        manufacturer_data: data,
        rssi: device.rssi().await.ok().flatten(),
    })
}

/// Tells live advertisements apart from BlueZ's cache. Discovery also reports
/// remembered devices that are out of range (with their old advertising data),
/// so a sighting only counts with a live RSSI, and for a device BlueZ already
/// knew before the scan only once it reports a fresh change for it.
struct Liveness {
    cached: std::collections::HashSet<Address>,
    sightings: HashMap<Address, u32>,
}

impl Liveness {
    async fn new(adapter: &Adapter) -> Self {
        Liveness {
            cached: adapter
                .device_addresses()
                .await
                .unwrap_or_default()
                .into_iter()
                .collect(),
            sightings: HashMap::new(),
        }
    }

    async fn is_live(&mut self, device: &Device) -> bool {
        let addr = device.address();
        let n = self.sightings.entry(addr).or_insert(0);
        *n += 1;
        let n = *n;
        let rssi = device.rssi().await.ok().flatten().is_some();
        rssi && (!self.cached.contains(&addr) || n > 1)
    }
}

/// Scan for Fujifilm cameras. With `pairing` set, return the first camera that
/// advertises a pairing key; with `address`, the first sighting of that camera.
/// Otherwise collect everything seen until `dur` passes.
pub async fn scan(dur: Duration, pairing: bool, address: Option<Address>) -> Result<Vec<Found>> {
    let (_session, adapter) = adapter().await?;
    adapter
        .set_discovery_filter(DiscoveryFilter {
            transport: DiscoveryTransport::Le,
            duplicate_data: true,
            ..Default::default()
        })
        .await
        .ok();
    let mut events = adapter
        .discover_devices_with_changes()
        .await
        .context("starting Bluetooth scan")?;
    let mut live = Liveness::new(&adapter).await;
    let deadline = Instant::now() + dur;
    let mut seen: HashMap<Address, Found> = HashMap::new();
    loop {
        let left = deadline.saturating_duration_since(Instant::now());
        if left.is_zero() {
            break;
        }
        let Ok(Some(ev)) = timeout(left, events.next()).await else {
            break;
        };
        let addr = match ev {
            AdapterEvent::DeviceAdded(a) => a,
            _ => continue,
        };
        let Ok(device) = adapter.device(addr) else {
            continue;
        };
        if !live.is_live(&device).await {
            continue;
        }
        let Some(found) = describe(&device, address == Some(addr)).await else {
            continue;
        };
        if !seen.contains_key(&addr) || seen[&addr].pairing_key != found.pairing_key {
            info!(
                "found {} {:?} mfr={} pairing={}",
                addr,
                found.name,
                found
                    .manufacturer_data
                    .iter()
                    .map(|b| format!("{b:02x}"))
                    .collect::<String>(),
                found.pairing_key.is_some()
            );
        }
        if (pairing && found.pairing_key.is_some()) || address == Some(addr) {
            return Ok(vec![found]);
        }
        seen.insert(addr, found);
    }
    if pairing {
        bail!(
            "no camera in pairing mode found. On the camera choose Bluetooth settings → Pairing registration{}",
            if seen.is_empty() {
                String::new()
            } else {
                format!(" (saw {} Fuji camera(s) not in pairing mode)", seen.len())
            }
        );
    }
    if let Some(a) = address {
        bail!(
            "camera {a} isn't advertising over Bluetooth: switch it on and check that its \
             Bluetooth is enabled"
        );
    }
    Ok(seen.into_values().collect())
}

/// Whether the camera at `address` is advertising right now (switched on with
/// Bluetooth enabled). See [`Liveness`] for how cached devices are ignored.
pub async fn is_nearby(
    address: Address,
    dur: Duration,
    cancel: &std::sync::atomic::AtomicBool,
) -> Result<bool> {
    let (_session, adapter) = adapter().await?;
    adapter
        .set_discovery_filter(DiscoveryFilter {
            transport: DiscoveryTransport::Le,
            duplicate_data: true,
            ..Default::default()
        })
        .await
        .ok();
    let mut events = adapter
        .discover_devices_with_changes()
        .await
        .context("starting Bluetooth scan")?;
    let mut live = Liveness::new(&adapter).await;
    let deadline = Instant::now() + dur;
    loop {
        // Wake up regularly so a cancel (the user clicked Connect) stops the
        // scan quickly: a second scan running alongside the reconnect makes
        // BlueZ drop the camera there.
        if cancel.load(std::sync::atomic::Ordering::Relaxed) {
            return Ok(false);
        }
        let left = deadline.saturating_duration_since(Instant::now());
        if left.is_zero() {
            return Ok(false);
        }
        let ev = match timeout(left.min(Duration::from_millis(200)), events.next()).await {
            Ok(Some(ev)) => ev,
            Ok(None) => return Ok(false),
            Err(_) => continue,
        };
        let AdapterEvent::DeviceAdded(a) = ev else {
            continue;
        };
        if a != address {
            continue;
        }
        if let Ok(d) = adapter.device(a)
            && live.is_live(&d).await
        {
            return Ok(true);
        }
    }
}

/// Blocking wrapper around [`is_nearby`] for a saved pairing.
pub fn probe(p: &Pairing, dur: Duration, cancel: &std::sync::atomic::AtomicBool) -> Result<bool> {
    let addr: Address = p
        .address
        .parse()
        .map_err(|_| anyhow!("bad address {}", p.address))?;
    runtime()?.block_on(is_nearby(addr, dur, cancel))
}

/// An open GATT connection to a camera.
pub struct BleCamera {
    _session: Session,
    _agent: Option<bluer::agent::AgentHandle>,
    device: Device,
    chars: HashMap<Uuid, Characteristic>,
}

/// A BlueZ agent that accepts the camera's "Just Works" bonding request, as
/// Android does for the official app.
async fn register_agent(session: &Session) -> Option<bluer::agent::AgentHandle> {
    use bluer::agent::Agent;
    let agent = Agent {
        request_default: true,
        request_confirmation: Some(Box::new(|req| {
            Box::pin(async move {
                info!(
                    "Bluetooth: accepting bonding with {} (passkey {:06})",
                    req.device, req.passkey
                );
                Ok(())
            })
        })),
        request_authorization: Some(Box::new(|req| {
            Box::pin(async move {
                info!("Bluetooth: authorizing {}", req.device);
                Ok(())
            })
        })),
        authorize_service: Some(Box::new(|_req| Box::pin(async { Ok(()) }))),
        display_passkey: Some(Box::new(|req| {
            Box::pin(async move {
                warn!(
                    "Bluetooth: camera bonding passkey {:06} (confirm on the camera if shown)",
                    req.passkey
                );
                Ok(())
            })
        })),
        request_passkey: Some(Box::new(|req| {
            Box::pin(async move {
                warn!(
                    "Bluetooth: {} asks for a passkey; not supported yet",
                    req.device
                );
                Err(bluer::agent::ReqError::Rejected)
            })
        })),
        ..Default::default()
    };
    match session.register_agent(agent).await {
        Ok(h) => Some(h),
        Err(e) => {
            warn!(
                "could not register Bluetooth agent ({e}); bonding may need confirming in your desktop's Bluetooth dialog"
            );
            None
        }
    }
}

/// Forget the camera in BlueZ and wait for it to be rediscovered, so the next
/// connection performs a full GATT service discovery. BlueZ otherwise reuses its
/// cached attribute database and skips discovery; the X-E3 then drops the link
/// about a second after the pairing key (Android, which the app targets, does
/// not cache GATT for unbonded devices).
async fn fresh_device(adapter: &Adapter, address: Address) -> Result<Device> {
    if adapter.device_addresses().await?.contains(&address) {
        if adapter.device(address)?.is_paired().await.unwrap_or(false) {
            // Keep real bonds; just use the device as is.
            return Ok(adapter.device(address)?);
        }
        debug!("Bluetooth: clearing BlueZ cache for {address}");
        let _ = adapter.remove_device(address).await;
    }
    adapter
        .set_discovery_filter(DiscoveryFilter {
            transport: DiscoveryTransport::Le,
            duplicate_data: true,
            ..Default::default()
        })
        .await
        .ok();
    let deadline = Instant::now() + Duration::from_secs(20);
    'discovery: loop {
        let mut events = adapter.discover_devices_with_changes().await?;
        loop {
            let left = deadline.saturating_duration_since(Instant::now());
            if left.is_zero() {
                bail!("camera {address} did not show up again; is its Bluetooth on?");
            }
            match timeout(left, events.next()).await {
                Ok(Some(AdapterEvent::DeviceAdded(a))) if a == address => break 'discovery,
                Ok(Some(_)) => continue,
                // The stream ends when discovery stops under us, e.g. while
                // another scan in this process is shutting down: start again.
                Ok(None) => {
                    debug!("Bluetooth: discovery stopped; restarting it");
                    drop(events);
                    sleep(Duration::from_millis(300)).await;
                    continue 'discovery;
                }
                Err(_) => bail!("camera {address} did not show up again; is its Bluetooth on?"),
            }
        }
    }
    Ok(adapter.device(address)?)
}

async fn connect_device(device: &Device) -> Result<()> {
    let mut attempt = 0;
    while !device.is_connected().await.unwrap_or(false) {
        attempt += 1;
        info!(
            "Bluetooth: connecting to {} (attempt {attempt})",
            device.address()
        );
        match device.connect().await {
            Ok(()) => break,
            Err(e) if attempt < 4 => {
                warn!("connect failed: {e}");
                sleep(Duration::from_secs(1)).await;
            }
            Err(e) => return Err(anyhow!(e).context("Bluetooth connect")),
        }
    }
    Ok(())
}

async fn discover_chars(device: &Device) -> Result<HashMap<Uuid, Characteristic>> {
    let mut chars = HashMap::new();
    for svc in device
        .services()
        .await
        .context("discovering GATT services")?
    {
        let suuid = svc.uuid().await.unwrap_or_default();
        for ch in svc.characteristics().await? {
            if let Ok(u) = ch.uuid().await {
                debug!("  service {suuid} characteristic {u}");
                chars.insert(u, ch);
            }
        }
    }
    if chars.is_empty() {
        bail!("camera exposes no GATT characteristics");
    }
    Ok(chars)
}

impl BleCamera {
    pub async fn connect(address: Address) -> Result<Self> {
        let (session, adapter) = adapter().await?;
        let agent = register_agent(&session).await;
        let device = fresh_device(&adapter, address).await?;
        connect_device(&device).await?;
        let chars = discover_chars(&device).await?;
        info!(
            "Bluetooth: connected, {} characteristics, paired={}",
            chars.len(),
            device.is_paired().await.unwrap_or(false)
        );
        Ok(BleCamera {
            _session: session,
            _agent: agent,
            device,
            chars,
        })
    }

    fn ch(&self, u: Uuid) -> Result<&Characteristic> {
        self.chars
            .get(&u)
            .ok_or_else(|| anyhow!("camera has no characteristic {u}"))
    }

    pub async fn read(&self, u: Uuid) -> Result<Vec<u8>> {
        let v = self
            .ch(u)?
            .read()
            .await
            .with_context(|| format!("reading {u}"))?;
        debug!("read {u} = {v:02x?}");
        Ok(v)
    }

    pub async fn read_string(&self, u: Uuid) -> Result<String> {
        let v = self.read(u).await?;
        let end = v.iter().position(|&b| b == 0).unwrap_or(v.len());
        Ok(String::from_utf8_lossy(&v[..end]).trim().to_string())
    }

    pub async fn write(&self, u: Uuid, data: &[u8]) -> Result<()> {
        debug!("write {u} = {data:02x?}");
        self.ch(u)?
            .write(data)
            .await
            .with_context(|| format!("writing {u}"))
    }

    /// Register this computer with the camera (the app's pairing sequence).
    /// Identify ourselves to the camera (pairing and every reconnect).
    /// Returns whether the camera asked for the date/time, which it does when it
    /// accepts a new pairing. No OS-level bond is needed: with a fresh pairing
    /// session the link stays up and the camera shows its date/time prompt.
    pub async fn register(&mut self, key: [u8; 4], client_name: &str) -> Result<bool> {
        // Order that works on the X-E3: key, name, protocol version back to back
        // (reading anything between key and name makes the camera fail with ATT
        // error 0x0E and drop the link), then the app's notification
        // subscriptions (AP state first) and the date/time exchange, and only
        // then the informational reads.
        let t0 = Instant::now();
        self.write(uuid::PAIRING_KEY, &key)
            .await
            .context("writing pairing key")?;
        let mut name: Vec<u8> = client_name.as_bytes().iter().copied().take(31).collect();
        name.push(0);
        self.write(uuid::DEVICE_NAME, &name)
            .await
            .context("writing device name")?;
        self.write(uuid::BLE_PROTOCOL_VERSION, &2u16.to_le_bytes())
            .await
            .context("writing protocol version")?;
        info!(
            "Bluetooth: key, name and protocol version accepted ({:?})",
            t0.elapsed()
        );
        let asked = self.subscribe_and_sync_time().await;
        for u in [
            uuid::GAP_DEVICE_NAME,
            uuid::SERIAL_NUMBER,
            uuid::READ_BD45,
            uuid::PAIRED_DEVICE_COUNT,
            uuid::FIRMWARE_REVISION,
            uuid::LENS_FIRMWARE,
            uuid::LENS_PRODUCT,
            uuid::READ_9C72,
            uuid::READ_4BF1,
            uuid::READ_7EDE,
            uuid::READ_34D8,
            uuid::READ_CAE8,
            uuid::CAMERA_BLE_PROTOCOL_VERSION,
        ] {
            if !self.chars.contains_key(&u) {
                continue;
            }
            match self.read(u).await {
                Ok(v) => info!(
                    "  read {u}: {:02x?} {:?}",
                    v,
                    String::from_utf8_lossy(&v).trim_end_matches('\0')
                ),
                Err(e) => info!("  read {u}: {e:#}"),
            }
        }
        info!("Bluetooth: registration sequence done ({:?})", t0.elapsed());
        Ok(asked)
    }

    /// Returns true if the camera requested (and got) the date/time.
    async fn subscribe_and_sync_time(&self) -> bool {
        let sent = self.subscribe_state().await;
        // The camera asks for the time right after a new pairing; answer that
        // request only, and send it ourselves just if no request arrives.
        for _ in 0..10 {
            if sent.load(std::sync::atomic::Ordering::SeqCst) {
                return true;
            }
            sleep(Duration::from_millis(100)).await;
        }
        if self.device.is_connected().await.unwrap_or(false) {
            if let Err(e) = self.sync_time().await {
                warn!("date/time sync failed: {e:#}");
            }
        } else {
            warn!("Bluetooth: the camera closed the connection");
        }
        false
    }

    /// Send the computer's local date/time to the camera.
    pub async fn sync_time(&self) -> Result<()> {
        let b = now_bytes();
        info!("Bluetooth: sending date/time {b:02x?}");
        self.write(uuid::CURRENT_DATETIME, &b).await
    }

    /// Subscribe to the camera-state notifications like the app does, and
    /// answer date-sync requests.
    /// Returns a flag that becomes true once the time was sent on request.
    async fn subscribe_state(&self) -> std::sync::Arc<std::sync::atomic::AtomicBool> {
        let sent = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        let time_ch = self.chars.get(&uuid::CURRENT_DATETIME).cloned();
        // Not AP_STATE here: pairing only succeeded on the X-E3 without it;
        // `launch` subscribes to it when starting Wi-Fi.
        for u in [
            uuid::TRANSFER_STATE,
            uuid::DATE_SYNC_STATE,
            uuid::STATE_AD06,
            uuid::STATE_049E,
        ] {
            let Ok(c) = self.ch(u) else { continue };
            let s = match c.notify().await {
                Ok(s) => s,
                Err(e) => {
                    debug!("notify {u}: {e}");
                    continue;
                }
            };
            let time_ch = time_ch.clone();
            let sent = sent.clone();
            tokio::spawn(async move {
                futures_util::pin_mut!(s);
                while let Some(v) = s.next().await {
                    if u == uuid::DATE_SYNC_STATE {
                        info!("Bluetooth: date-sync state {v:02x?}");
                    } else {
                        debug!("notify {u}: {v:02x?}");
                    }
                    if u == uuid::DATE_SYNC_STATE
                        && v.first() == Some(&1)
                        && let Some(t) = &time_ch
                    {
                        let b = now_bytes();
                        match t.write(&b).await {
                            Ok(()) => {
                                sent.store(true, std::sync::atomic::Ordering::SeqCst);
                                info!("Bluetooth: camera asked for the time, sent {b:02x?}");
                                match t.read().await {
                                    Ok(r) => info!("Bluetooth: camera date/time now {r:02x?}"),
                                    Err(e) => info!("Bluetooth: reading date/time back: {e}"),
                                }
                            }
                            Err(e) => warn!("sending date/time: {e}"),
                        }
                    }
                }
            });
        }
        sent
    }

    /// Ask the camera to start `f` and wait until its Wi-Fi access point is up.
    pub async fn launch(&self, f: Function, wait: Duration) -> Result<WifiInfo> {
        // Subscribe to the AP state before triggering, as the app does.
        let mut notes = match self.ch(uuid::AP_STATE) {
            Ok(c) => match c.notify().await {
                Ok(n) => Some(Box::pin(n)),
                Err(e) => {
                    warn!("AP state notifications unavailable ({e}); will poll instead");
                    None
                }
            },
            Err(e) => {
                warn!("{e}; will poll instead");
                None
            }
        };
        info!("Bluetooth: requesting function {f:?}");
        self.write(uuid::FUNCTION_LAUNCH, &(f as u16).to_le_bytes())
            .await
            .context("function launch request")?;

        let deadline = Instant::now() + wait;
        let mut last = None;
        loop {
            let left = deadline.saturating_duration_since(Instant::now());
            if left.is_zero() {
                bail!("camera did not bring up Wi-Fi (last AP state {last:?})");
            }
            let state = match notes.as_mut() {
                Some(n) => match timeout(left.min(Duration::from_secs(2)), n.next()).await {
                    Ok(Some(v)) => v.first().copied(),
                    Ok(None) => None,
                    Err(_) => self
                        .read(uuid::AP_STATE)
                        .await
                        .ok()
                        .and_then(|v| v.first().copied()),
                },
                None => {
                    sleep(Duration::from_secs(1)).await;
                    self.read(uuid::AP_STATE)
                        .await
                        .ok()
                        .and_then(|v| v.first().copied())
                }
            };
            if state.is_some() && state != last {
                info!("Bluetooth: AP state {state:?}");
                last = state;
            }
            match state {
                Some(ap_state::CONNECTABLE) => break,
                Some(ap_state::CANCELLED) => {
                    bail!("camera cancelled the Wi-Fi start (check its screen)")
                }
                _ => {}
            }
        }
        let ssid = self.read_string(uuid::SSID).await.context("reading SSID")?;
        let passphrase = self.read_string(uuid::PASSPHRASE).await.unwrap_or_default();
        info!("Bluetooth: camera Wi-Fi is {ssid:?}");
        Ok(WifiInfo { ssid, passphrase })
    }

    pub async fn is_connected(&self) -> bool {
        self.device.is_connected().await.unwrap_or(false)
    }

    pub async fn disconnect(&self) {
        let _ = self.device.disconnect().await;
    }
}

/// Blocking wrapper that keeps the BLE link (and its runtime) alive while the
/// Wi-Fi session is in use.
pub struct BleLink {
    rt: tokio::runtime::Runtime,
    cam: BleCamera,
}

pub fn runtime() -> Result<tokio::runtime::Runtime> {
    Ok(tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .enable_all()
        .build()?)
}

impl BleLink {
    /// Find a camera in pairing mode, register with it and remember it.
    pub fn pair(client_name: &str, scan_for: Duration) -> Result<(Pairing, BleLink)> {
        let rt = runtime()?;
        let found = rt.block_on(scan(scan_for, true, None))?.remove(0);
        let key = found.pairing_key.expect("pairing scan returns a key");
        info!(
            "Bluetooth: camera pairing key {}",
            key.iter().map(|b| format!("{b:02x}")).collect::<String>()
        );
        let mut cam = rt.block_on(BleCamera::connect(found.address))?;
        let asked = rt.block_on(cam.register(key, client_name))?;
        // Stay connected briefly so the camera can finish its own pairing steps
        // (it may ask for the date/time).
        rt.block_on(async { sleep(Duration::from_secs(5)).await });
        if !asked && !rt.block_on(cam.is_connected()) {
            bail!(
                "the camera did not accept the pairing (it closed the connection without asking \
                 for the date/time). On the camera: leave the pairing screen, delete the pairing \
                 registration, switch the camera off and on, then start Pairing registration again"
            );
        }
        let p = Pairing {
            address: found.address.to_string(),
            key,
            name: found.name,
        };
        save_pairing(&p)?;
        Ok((p, BleLink { rt, cam }))
    }

    /// Reconnect to a remembered camera (it must be on, Bluetooth enabled).
    pub fn reconnect(p: &Pairing, client_name: &str) -> Result<BleLink> {
        let rt = runtime()?;
        let addr: Address = p
            .address
            .parse()
            .map_err(|_| anyhow!("bad address {}", p.address))?;
        // A scan makes BlueZ (re)discover the device before connecting.
        let found = rt.block_on(scan(Duration::from_secs(15), false, Some(addr)))?;
        if found.iter().any(|f| f.pairing_key.is_some()) {
            warn!(
                "the camera is still on its pairing screen; it won't start Wi-Fi from there. \
                 Leave the pairing screen on the camera (Bluetooth stays on) and connect again"
            );
        }
        let mut cam = rt.block_on(BleCamera::connect(addr))?;
        rt.block_on(cam.register(p.key, client_name))?;
        Ok(BleLink { rt, cam })
    }

    pub fn launch(&self, f: Function) -> Result<WifiInfo> {
        self.rt
            .block_on(self.cam.launch(f, Duration::from_secs(45)))
    }

    pub fn disconnect(self) {
        self.rt.block_on(self.cam.disconnect());
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn datetime_layout() {
        use chrono::NaiveDate;
        let t = NaiveDate::from_ymd_opt(2026, 11, 3)
            .unwrap()
            .and_hms_opt(22, 45, 7)
            .unwrap();
        assert_eq!(datetime_bytes(&t), [0xEA, 0x07, 11, 3, 22, 45, 7]);
    }

    #[test]
    fn adv_key_offsets() {
        assert_eq!(
            pairing_key_from_adv(&[0x02, 1, 2, 3, 4]),
            Some([1, 2, 3, 4])
        );
        assert_eq!(
            pairing_key_from_adv(&[0x03, 9, 9, 9, 9, 1, 2, 3, 4]),
            Some([1, 2, 3, 4])
        );
        assert_eq!(pairing_key_from_adv(&[0x01, 9, 9, 9, 9]), None);
        assert_eq!(pairing_key_from_adv(&[0x02, 1, 2]), None);
    }
}
