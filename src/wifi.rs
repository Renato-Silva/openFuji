//! Join the camera's Wi-Fi with NetworkManager (`nmcli`) and switch back afterwards.

use anyhow::{Context, Result, bail};
use log::{info, warn};
use std::process::Command;

fn nmcli(args: &[&str]) -> Result<String> {
    let out = Command::new("nmcli")
        .args(args)
        .output()
        .context("running nmcli (is NetworkManager installed?)")?;
    if !out.status.success() {
        bail!(
            "nmcli {}: {}",
            args.first().unwrap_or(&""),
            String::from_utf8_lossy(&out.stderr).trim()
        );
    }
    Ok(String::from_utf8_lossy(&out.stdout).into_owned())
}

/// Name of the currently active Wi-Fi connection, if any.
pub fn active_wifi() -> Option<String> {
    let out = nmcli(&["-t", "-f", "NAME,TYPE", "connection", "show", "--active"]).ok()?;
    out.lines()
        .filter_map(|l| l.rsplit_once(':'))
        .find(|(_, t)| *t == "802-11-wireless")
        .map(|(n, _)| n.replace("\\:", ":"))
}

pub struct WifiSession {
    previous: Option<String>,
    profile: String,
}

/// Join `ssid`. Tries DHCP first, then a static 192.168.0.2/24.
pub fn join(ssid: &str, passphrase: &str) -> Result<WifiSession> {
    let previous = active_wifi().filter(|p| p != ssid);
    let profile = format!("openfuji-{ssid}");
    let _ = nmcli(&["connection", "delete", &profile]);
    let mut args = vec![
        "connection",
        "add",
        "type",
        "wifi",
        "con-name",
        &profile,
        "ifname",
        "*",
        "ssid",
        ssid,
        "ipv4.method",
        "auto",
        "ipv4.dhcp-timeout",
        "20",
        // Send no DHCP client identifier. The X-E3's DHCP server ignores the
        // default DUID-style one and mangles a MAC-style one in its offer
        // (70:a8:d3:14:ec:ea comes back as 70:a8:d3:14:00:00), which makes
        // NetworkManager drop the offer as not meant for us.
        "ipv4.dhcp-client-id",
        "none",
        "ipv4.never-default",
        "yes",
        "ipv6.method",
        "disabled",
        "connection.autoconnect",
        "no",
    ];
    if !passphrase.is_empty() {
        args.extend(["wifi-sec.key-mgmt", "wpa-psk", "wifi-sec.psk", passphrase]);
    }
    nmcli(&args)?;
    let _ = nmcli(&["device", "wifi", "rescan"]);
    std::thread::sleep(std::time::Duration::from_secs(2));
    info!("joining Wi-Fi {ssid:?} (DHCP)");
    if let Err(e) = nmcli(&["--wait", "30", "connection", "up", &profile]) {
        warn!("DHCP on camera Wi-Fi failed ({e}); using static 192.168.0.2");
        nmcli(&[
            "connection",
            "modify",
            &profile,
            "ipv4.method",
            "manual",
            "ipv4.addresses",
            "192.168.0.2/24",
        ])?;
        nmcli(&["--wait", "30", "connection", "up", &profile]).context("joining camera Wi-Fi")?;
    }
    Ok(WifiSession { previous, profile })
}

impl WifiSession {
    /// Leave the camera network and go back to the previous Wi-Fi.
    pub fn restore(self) {
        let _ = nmcli(&["connection", "down", &self.profile]);
        if let Some(p) = &self.previous {
            info!("switching back to Wi-Fi {p:?}");
            if let Err(e) = nmcli(&["connection", "up", p]) {
                warn!("could not restore {p}: {e}");
            }
        }
    }
}
