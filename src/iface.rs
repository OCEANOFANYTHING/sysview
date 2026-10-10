//! Network-interface discovery for `--interface <name>` and the dashboard's
//! interface picker.
//!
//! Implemented entirely on top of `sysinfo` (already a dependency), so this
//! costs no extra binary weight and stays portable across Windows, Linux and
//! macOS. Discovery happens only at startup or when the settings page is
//! opened — there is no polling and no background state.

use std::net::{IpAddr, Ipv4Addr};

use serde::Serialize;
use sysinfo::Networks;

/// One interface as shown to the operator.
#[derive(Clone, Debug, Serialize)]
pub struct Iface {
    /// OS interface name, e.g. `wlx3460f9f543f9`, `Wi-Fi`, `en0`.
    pub name: String,
    /// IPv4/IPv6 addresses currently assigned to it.
    pub ips: Vec<String>,
    /// MAC address (empty when the OS does not report one).
    pub mac: String,
    /// Best-effort guess that this is a Wi-Fi radio (name heuristics only).
    pub wifi: bool,
}

impl Iface {
    /// The first non-loopback IPv4 address, if any (what `--interface` uses).
    pub fn first_ipv4(&self) -> Option<Ipv4Addr> {
        self.ips
            .iter()
            .filter_map(|s| s.parse::<IpAddr>().ok())
            .find_map(|ip| match ip {
                IpAddr::V4(v4) if !v4.is_loopback() => Some(v4),
                _ => None,
            })
    }
}

/// Lists interfaces (name-sorted) with their addresses.
pub fn list() -> Vec<Iface> {
    let nets = Networks::new_with_refreshed_list();
    let mut out: Vec<Iface> = nets
        .list()
        .iter()
        .map(|(name, data)| {
            let ips: Vec<String> = data
                .ip_networks()
                .iter()
                .map(|n| n.addr.to_string())
                .collect();
            let mac = data.mac_address().to_string();
            Iface {
                name: name.clone(),
                wifi: likely_wifi(name),
                mac: if mac == "00:00:00:00:00:00" {
                    String::new()
                } else {
                    mac
                },
                ips,
            }
        })
        .collect();
    out.sort_by(|a, b| a.name.cmp(&b.name));
    out
}

/// Resolves an interface name to its first non-loopback IPv4 address.
pub fn resolve_ipv4(name: &str) -> Option<Ipv4Addr> {
    list()
        .iter()
        .find(|i| i.name == name)
        .and_then(|i| i.first_ipv4())
}

/// A human list of every interface and its addresses, for error messages such
/// as an unknown `--interface` value.
pub fn describe_all() -> String {
    let mut s = String::from("available interfaces:\n");
    for i in list() {
        let ips = if i.ips.is_empty() {
            "(no address)".to_string()
        } else {
            i.ips.join(", ")
        };
        let wifi = if i.wifi { " [wifi?]" } else { "" };
        s.push_str(&format!("    {}{}  {}\n", i.name, wifi, ips));
    }
    s
}

/// Name-based Wi-Fi heuristic. Prediction is coarse by nature, so this is only
/// ever a label in the UI, never a filter.
fn likely_wifi(name: &str) -> bool {
    let n = name.to_ascii_lowercase();
    n.starts_with("wl")          // Linux predictable names: wlan0, wlp3s0, wlx…
        || n.starts_with("wlan")
        || n.contains("wi-fi")
        || n.contains("wifi")
        || n.contains("wireless")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn wifi_heuristic_matches_common_names() {
        assert!(likely_wifi("wlan0"));
        assert!(likely_wifi("wlp3s0"));
        assert!(likely_wifi("wlx3460f9f543f9"));
        assert!(likely_wifi("Wi-Fi"));
        assert!(!likely_wifi("eth0"));
        assert!(!likely_wifi("en0"));
        assert!(!likely_wifi("lo"));
    }

    #[test]
    fn describe_all_never_panics() {
        // Environment-independent: just ensure it renders on any host.
        let _ = describe_all();
        let _ = list();
    }
}
