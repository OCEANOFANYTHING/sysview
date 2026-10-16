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

use crate::model::IfaceKind;

/// One interface as shown to the operator.
#[derive(Clone, Debug, Serialize)]
pub struct Iface {
    /// OS interface name, e.g. `wlx3460f9f543f9`, `Wi-Fi`, `en0`.
    pub name: String,
    /// IPv4/IPv6 addresses currently assigned to it.
    pub ips: Vec<String>,
    /// MAC address (empty when the OS does not report one).
    pub mac: String,
    /// Best-effort guess at what the adapter is, from its name alone.
    pub kind: IfaceKind,
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
                kind: classify(name),
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
        s.push_str(&format!("    {}{}  {}\n", i.name, i.kind.marker(), ips));
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

/// Wired-Ethernet heuristic, kept to names that cannot be anything else.
///
/// Bare macOS `en0`/`en1` is deliberately not a match: it is the Wi-Fi radio on
/// most Macs and the Ethernet port on the desktops, so it is reported as
/// unknown instead of guessing and being wrong half the time.
fn likely_lan(name: &str) -> bool {
    let n = name.to_ascii_lowercase();
    // Virtual adapters carry "ethernet" in names that are not a NIC.
    if n.contains("vethernet")
        || n.contains("virtual")
        || n.contains("(wsl)")
        || n.contains("hyper-v")
    {
        return false;
    }
    n.starts_with("eth") // eth0, and Windows "Ethernet" / "Ethernet 2"
        || n.contains("local area connection") // legacy Windows naming
        // Linux predictable Ethernet: eno1, ens5, enp3s0, enx78e7d1ea46da …
        || (n.starts_with("en") && matches!(n.as_bytes().get(2), Some(b'o' | b's' | b'p' | b'x')))
}

/// Best guess at what an interface is, from its OS name alone.
///
/// Display-only and coarse by design: anything that cannot be told apart (a
/// bridge, a tunnel, macOS `en0`) stays [`IfaceKind::Other`] rather than being
/// labelled the wrong thing.
pub fn classify(name: &str) -> IfaceKind {
    if likely_wifi(name) {
        IfaceKind::Wifi
    } else if likely_lan(name) {
        IfaceKind::Lan
    } else {
        IfaceKind::Other
    }
}

/// The `[lan?]`/`[wifi?]` hint, in one place so the CLI, the `--interface`
/// error listing and the dashboard cannot drift apart. The `?` is the point:
/// the name is all this is based on.
impl IfaceKind {
    pub fn marker(self) -> &'static str {
        match self {
            IfaceKind::Wifi => " [wifi?]",
            IfaceKind::Lan => " [lan?]",
            IfaceKind::Other => "",
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn classify_reads_the_common_names() {
        for n in ["wlan0", "wlp3s0", "wlx3460f9f543f9", "Wi-Fi"] {
            assert_eq!(classify(n), IfaceKind::Wifi, "{n}");
        }
        for n in [
            "eth0",
            "enp3s0",
            "eno1",
            "ens5",
            "enx78e7d1ea46da",
            "Ethernet",
            "Ethernet 2",
            "Local Area Connection",
        ] {
            assert_eq!(classify(n), IfaceKind::Lan, "{n}");
        }
        // Tunnels, bridges, virtual switches, and the two names that genuinely
        // cannot be told apart: macOS en0/en1.
        for n in [
            "lo",
            "en0",
            "en1",
            "docker0",
            "br0",
            "veth1a2b3c",
            "utun3",
            "vEthernet (WSL)",
            "vEthernet (Default Switch)",
            "VirtualBox Host-Only Network",
        ] {
            assert_eq!(classify(n), IfaceKind::Other, "{n}");
        }
    }

    #[test]
    fn markers_spell_out_that_they_are_a_guess() {
        assert_eq!(IfaceKind::Wifi.marker(), " [wifi?]");
        assert_eq!(IfaceKind::Lan.marker(), " [lan?]");
        assert_eq!(IfaceKind::Other.marker(), "");
    }

    #[test]
    fn describe_all_never_panics() {
        // Environment-independent: just ensure it renders on any host.
        let _ = describe_all();
        let _ = list();
    }
}
