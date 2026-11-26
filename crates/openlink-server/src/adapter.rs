//! The Steam Frame's USB wireless adapter (Valve 28de:2432, a Realtek 6 GHz Wi-Fi card). It
//! joins the headset's own access point (a hidden `frame_…` SSID), a direct link beside the
//! home network. Steam creates the NetworkManager profile for it when it pairs the headset;
//! OpenLink only brings that profile up and streams over the link like over any other network.

use std::net::{IpAddr, Ipv4Addr};
use std::process::Command;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use crate::config::{ConnectionMode, Settings};
use crate::session::Status;

const VENDOR: &str = "28de";
const PRODUCT: &str = "2432";
/// The headset's access point; Steam names it after the headset.
const SSID_PREFIX: &str = "frame_";

#[derive(Debug, Clone, Default, PartialEq)]
pub struct AdapterStatus {
    /// The adapter's network interface (e.g. `wlan0`).
    pub interface: String,
    /// Its IPv4 address and prefix length, once joined to the headset's access point.
    pub ipv4: Option<(Ipv4Addr, u8)>,
    pub ssid: Option<String>,
    pub tx_mbps: Option<f32>,
    pub signal_dbm: Option<i32>,
    /// What stands in the way, if anything.
    pub problem: Option<String>,
}

impl AdapterStatus {
    /// Whether `ip` is on the adapter's network (the headset's side of the link).
    pub fn reaches(&self, ip: IpAddr) -> bool {
        let (Some((own, prefix)), IpAddr::V4(ip)) = (self.ipv4, ip) else { return false };
        let mask = if prefix == 0 { 0 } else { u32::MAX << (32 - prefix.min(32)) };
        u32::from(own) & mask == u32::from(ip) & mask
    }
}

/// Watches the adapter every couple of seconds and, while the connection mode asks for it,
/// joins the headset's access point when the adapter is plugged in but not connected.
pub fn spawn_monitor(status: Arc<Mutex<Status>>, settings: Arc<Mutex<Settings>>) {
    std::thread::Builder::new()
        .name("frame-adapter".into())
        .spawn(move || {
            let mut last_attempt: Option<Instant> = None;
            loop {
                let wanted = settings.lock().unwrap().connection == ConnectionMode::SteamFrameAdapter;
                let mut current = probe();
                if wanted {
                    if let Some(a) = &mut current {
                        let due = last_attempt.is_none_or(|t| t.elapsed() > Duration::from_secs(20));
                        if a.ipv4.is_none() && due {
                            last_attempt = Some(Instant::now());
                            match join() {
                                Ok(profile) => {
                                    log::info!("Steam Frame adapter: joined the headset's network ({profile})");
                                    current = probe();
                                }
                                Err(e) => {
                                    log::warn!("Steam Frame adapter: {e}");
                                    a.problem = Some(e);
                                }
                            }
                        }
                    }
                }
                let changed = {
                    let mut st = status.lock().unwrap();
                    let changed = st.adapter.as_ref().map(|a| (&a.interface, a.ipv4))
                        != current.as_ref().map(|a| (&a.interface, a.ipv4));
                    st.adapter = current.clone();
                    changed
                };
                if changed {
                    match &current {
                        Some(AdapterStatus { interface, ipv4: Some((ip, prefix)), .. }) => {
                            log::info!("Steam Frame adapter {interface}: {ip}/{prefix}")
                        }
                        Some(a) => log::info!("Steam Frame adapter {}: not connected to the headset", a.interface),
                        None => log::info!("Steam Frame adapter: not plugged in"),
                    }
                }
                std::thread::sleep(Duration::from_secs(2));
            }
        })
        .expect("spawn adapter monitor");
}

/// The adapter's current state, or `None` if it is not plugged in.
fn probe() -> Option<AdapterStatus> {
    let interface = find_interface()?;
    let ipv4 = ipv4_of(&interface);
    let mut a = AdapterStatus { interface, ipv4, ..Default::default() };
    if a.ipv4.is_some() {
        link_info(&mut a);
    }
    Some(a)
}

/// The network interface whose USB device is Valve's adapter.
fn find_interface() -> Option<String> {
    let mut names: Vec<String> = std::fs::read_dir("/sys/class/net")
        .ok()?
        .filter_map(|e| e.ok()?.file_name().into_string().ok())
        .collect();
    names.sort();
    names.into_iter().find(|n| {
        // `device` is the USB interface; its parent directory is the USB device.
        let id = |f: &str| std::fs::read_to_string(format!("/sys/class/net/{n}/device/../{f}")).ok();
        id("idVendor").is_some_and(|v| v.trim() == VENDOR) && id("idProduct").is_some_and(|p| p.trim() == PRODUCT)
    })
}

#[cfg(target_os = "linux")]
fn ipv4_of(interface: &str) -> Option<(Ipv4Addr, u8)> {
    let mut found = None;
    // SAFETY: getifaddrs/freeifaddrs pair; the list is only read in between.
    unsafe {
        let mut list: *mut libc::ifaddrs = std::ptr::null_mut();
        if libc::getifaddrs(&mut list) != 0 {
            return None;
        }
        let mut cur = list;
        while !cur.is_null() {
            let ifa = &*cur;
            cur = ifa.ifa_next;
            if ifa.ifa_addr.is_null() || ifa.ifa_netmask.is_null() || (*ifa.ifa_addr).sa_family as i32 != libc::AF_INET {
                continue;
            }
            if std::ffi::CStr::from_ptr(ifa.ifa_name).to_str() != Ok(interface) {
                continue;
            }
            let addr = &*(ifa.ifa_addr as *const libc::sockaddr_in);
            let mask = &*(ifa.ifa_netmask as *const libc::sockaddr_in);
            let ip = Ipv4Addr::from(u32::from_be(addr.sin_addr.s_addr));
            let prefix = u32::from_be(mask.sin_addr.s_addr).count_ones() as u8;
            found = Some((ip, prefix));
            break;
        }
        libc::freeifaddrs(list);
    }
    found
}

#[cfg(not(target_os = "linux"))]
fn ipv4_of(_interface: &str) -> Option<(Ipv4Addr, u8)> {
    None
}

/// SSID, transmit rate and signal, from `iw` if it is installed.
fn link_info(a: &mut AdapterStatus) {
    let Ok(out) = Command::new("iw").args(["dev", &a.interface, "link"]).output() else { return };
    for line in String::from_utf8_lossy(&out.stdout).lines() {
        let line = line.trim();
        if let Some(s) = line.strip_prefix("SSID: ") {
            a.ssid = Some(s.to_string());
        } else if let Some(s) = line.strip_prefix("tx bitrate: ") {
            a.tx_mbps = s.split_whitespace().next().and_then(|v| v.parse().ok());
        } else if let Some(s) = line.strip_prefix("signal: ") {
            a.signal_dbm = s.split_whitespace().next().and_then(|v| v.parse().ok());
        }
    }
}

/// Brings up the NetworkManager profile Steam made for the headset's access point.
fn join() -> Result<String, String> {
    let profile = steam_profile()?;
    let out = Command::new("nmcli")
        .args(["--wait", "30", "connection", "up", "id", &profile])
        .output()
        .map_err(|e| format!("cannot run nmcli ({e}); is NetworkManager installed?"))?;
    if out.status.success() {
        Ok(profile)
    } else {
        let err = String::from_utf8_lossy(&out.stderr);
        Err(format!(
            "cannot join the headset's network with profile {profile:?}: {} (is the Steam Frame on, with its PC link active?)",
            err.lines().next().unwrap_or("").trim()
        ))
    }
}

/// The Wi-Fi profile whose SSID is the headset's access point.
fn steam_profile() -> Result<String, String> {
    let out = Command::new("nmcli")
        .args(["-t", "-f", "NAME,TYPE", "connection", "show"])
        .output()
        .map_err(|e| format!("cannot run nmcli ({e}); is NetworkManager installed?"))?;
    let names: Vec<String> = String::from_utf8_lossy(&out.stdout)
        .lines()
        .filter_map(|l| l.rsplit_once(':'))
        .filter(|(_, ty)| *ty == "802-11-wireless")
        .map(|(name, _)| name.replace("\\:", ":"))
        .collect();
    for name in &names {
        let ssid = Command::new("nmcli")
            .args(["-g", "802-11-wireless.ssid", "connection", "show", "id", name])
            .output()
            .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string())
            .unwrap_or_default();
        if ssid.starts_with(SSID_PREFIX) {
            return Ok(name.clone());
        }
    }
    Err("no NetworkManager profile for the Steam Frame's network; pair the headset with Steam once (Steam Link) so Steam creates it".into())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reaches_only_the_adapter_network() {
        let a = AdapterStatus { ipv4: Some((Ipv4Addr::new(10, 35, 78, 28), 24)), ..Default::default() };
        assert!(a.reaches(IpAddr::V4(Ipv4Addr::new(10, 35, 78, 1))));
        assert!(!a.reaches(IpAddr::V4(Ipv4Addr::new(192, 168, 188, 126))));
        assert!(!AdapterStatus::default().reaches(IpAddr::V4(Ipv4Addr::new(10, 35, 78, 1))));
    }
}
