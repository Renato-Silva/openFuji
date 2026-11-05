//! Bluetooth-first connection: wake the camera's Wi-Fi over BLE, join it, and
//! undo both when done. Needed by cameras such as the X-E3 that only serve
//! Wi-Fi clients introduced over Bluetooth.

use anyhow::{Context, Result, bail};
use log::info;

use crate::ble::{self, BleLink, Function, Pairing, WifiInfo};
use crate::camera::Mode;
use crate::wifi::{self, WifiSession};

pub fn function_for(mode: Mode) -> Function {
    match mode {
        Mode::Remote => Function::Remote,
        Mode::PhotoView | Mode::PhotoViewV12 => Function::CameraViewer,
    }
}

pub struct BleWifi {
    ble: Option<BleLink>,
    wifi: Option<WifiSession>,
    pub info: WifiInfo,
}

/// The most recently paired camera, if any.
pub fn last_pairing() -> Option<Pairing> {
    ble::load_pairings().into_iter().next()
}

/// Reconnect to the paired camera over Bluetooth, request `mode`, and join its Wi-Fi.
pub fn start(mode: Mode, client_name: &str, join_wifi: bool) -> Result<BleWifi> {
    let Some(p) = last_pairing() else {
        bail!(
            "no paired camera yet: pair it first (camera: Bluetooth settings → Pairing registration)"
        );
    };
    info!("using paired camera {} ({})", p.name, p.address);
    if join_wifi
        && let Some(ssid) = wifi::active_wifi().filter(|s| s.contains("FUJIFILM-"))
    {
        // The camera's Wi-Fi is already up and joined; Bluetooth would only
        // ask for it again (and the camera may not answer while it's on).
        info!("already on camera Wi-Fi {ssid:?}; skipping Bluetooth");
        return Ok(BleWifi {
            ble: None,
            wifi: None,
            info: WifiInfo {
                ssid,
                passphrase: String::new(),
            },
        });
    }
    let link = BleLink::reconnect(&p, client_name).context("Bluetooth reconnect")?;
    let info = link.launch(function_for(mode))?;
    let wifi = if join_wifi {
        Some(wifi::join(&info.ssid, &info.passphrase)?)
    } else {
        None
    };
    Ok(BleWifi {
        ble: Some(link),
        wifi,
        info,
    })
}

impl BleWifi {
    pub fn stop(mut self) {
        if let Some(w) = self.wifi.take() {
            w.restore();
        }
        if let Some(b) = self.ble.take() {
            b.disconnect();
        }
    }
}

impl Drop for BleWifi {
    fn drop(&mut self) {
        if let Some(w) = self.wifi.take() {
            w.restore();
        }
        if let Some(b) = self.ble.take() {
            b.disconnect();
        }
    }
}
