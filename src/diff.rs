//! `sysview diff`: what changed between two saved overviews
//! (`sysview --json > a.json`) — or between a saved one and the live host.

use std::collections::BTreeMap;
use std::path::Path;

use crate::model::{DiffReport, DiffRow, Overview};
use crate::render::{self, RenderOpts, UnitMode};

/// Reads a snapshot written by `sysview --json` (the full overview).
pub fn load(path: &Path) -> Result<Overview, String> {
    let bytes = std::fs::read(path).map_err(|e| format!("cannot read {}: {e}", path.display()))?;
    let text = decode(&bytes);
    serde_json::from_str(&text)
        .map_err(|e| format!("{} is not a `sysview --json` snapshot: {e}", path.display()))
}

/// The snapshot is JSON, but a redirection on Windows can leave UTF-16
/// (PowerShell 5.1) or a UTF-8 BOM in the file, so decode before parsing.
fn decode(bytes: &[u8]) -> String {
    let utf16 = |b: &[u8], le: bool| -> String {
        let mut units = Vec::with_capacity(b.len() / 2);
        let mut i = 0;
        while i + 1 < b.len() {
            units.push(if le {
                u16::from_le_bytes([b[i], b[i + 1]])
            } else {
                u16::from_be_bytes([b[i], b[i + 1]])
            });
            i += 2;
        }
        String::from_utf16_lossy(&units)
    };
    if let Some(rest) = bytes.strip_prefix(&[0xFF, 0xFE]) {
        utf16(rest, true)
    } else if let Some(rest) = bytes.strip_prefix(&[0xFE, 0xFF]) {
        utf16(rest, false)
    } else {
        String::from_utf8_lossy(bytes.strip_prefix(&[0xEF, 0xBB, 0xBF]).unwrap_or(bytes))
            .into_owned()
    }
}

/// Builds the report. Only host-level metrics are compared: the process list
/// in a snapshot is already truncated to `--top`, so it cannot be diffed
/// honestly.
pub fn report(from: &str, to: &str, before: &Overview, after: &Overview) -> DiffReport {
    let mut rows: Vec<DiffRow> = Vec::new();
    let mut push = |metric: String, b: f64, a: f64, unit: &str| {
        rows.push(row(metric, b, a, unit));
    };

    push(
        "uptime".to_string(),
        before.system.uptime_secs as f64,
        after.system.uptime_secs as f64,
        "seconds",
    );
    push(
        "cpu usage".to_string(),
        before.cpu.usage as f64,
        after.cpu.usage as f64,
        "percent",
    );
    push(
        "memory used".to_string(),
        before.memory.used as f64,
        after.memory.used as f64,
        "bytes",
    );
    push(
        "memory used %".to_string(),
        before.memory.used_percent as f64,
        after.memory.used_percent as f64,
        "percent",
    );
    push(
        "memory available".to_string(),
        before.memory.available as f64,
        after.memory.available as f64,
        "bytes",
    );
    push(
        "swap used".to_string(),
        before.memory.swap_used as f64,
        after.memory.swap_used as f64,
        "bytes",
    );

    disks_diff(before, after, &mut rows);
    nets_diff(before, after, &mut rows);

    DiffReport {
        from: from.to_string(),
        to: to.to_string(),
        reboot: after.system.uptime_secs < before.system.uptime_secs,
        rows,
    }
}

fn row(metric: String, before: f64, after: f64, unit: &str) -> DiffRow {
    let delta = after - before;
    DiffRow {
        metric,
        before,
        after,
        delta,
        unit: unit.to_string(),
        change_percent: if before == 0.0 {
            None
        } else {
            Some(delta * 100.0 / before)
        },
    }
}

/// Used bytes per mount, matched by mount point so volumes that appeared or
/// went away show up as `(new)` / `(gone)` instead of vanishing silently.
fn disks_diff(before: &Overview, after: &Overview, rows: &mut Vec<DiffRow>) {
    let mut map: BTreeMap<&str, (Option<u64>, Option<u64>)> = BTreeMap::new();
    for d in &before.disks {
        map.entry(d.mount_point.as_str()).or_default().0 = Some(d.used);
    }
    for d in &after.disks {
        map.entry(d.mount_point.as_str()).or_default().1 = Some(d.used);
    }
    for (mount, (b, a)) in map {
        let suffix = match (b.is_some(), a.is_some()) {
            (false, true) => " (new)",
            (true, false) => " (gone)",
            _ => "",
        };
        rows.push(row(
            format!("disk {mount} used{suffix}"),
            b.unwrap_or(0) as f64,
            a.unwrap_or(0) as f64,
            "bytes",
        ));
    }
}

/// Per-interface `(rx, tx)` totals on each side of the comparison.
type NetPair = (Option<(u64, u64)>, Option<(u64, u64)>);

/// Since-boot byte counters per interface, matched by name.
fn nets_diff(before: &Overview, after: &Overview, rows: &mut Vec<DiffRow>) {
    let mut map: BTreeMap<&str, NetPair> = BTreeMap::new();
    for n in &before.networks {
        map.entry(n.name.as_str()).or_default().0 = Some((n.received, n.transmitted));
    }
    for n in &after.networks {
        map.entry(n.name.as_str()).or_default().1 = Some((n.received, n.transmitted));
    }
    for (name, (b, a)) in map {
        let suffix = match (b.is_some(), a.is_some()) {
            (false, true) => " (new)",
            (true, false) => " (gone)",
            _ => "",
        };
        let (bx, ax) = (b.map(|v| v.0).unwrap_or(0), a.map(|v| v.0).unwrap_or(0));
        let (bt, at) = (b.map(|v| v.1).unwrap_or(0), a.map(|v| v.1).unwrap_or(0));
        rows.push(row(
            format!("net {name} rx{suffix}"),
            bx as f64,
            ax as f64,
            "bytes",
        ));
        rows.push(row(
            format!("net {name} tx{suffix}"),
            bt as f64,
            at as f64,
            "bytes",
        ));
    }
}

/// Renders the report as an aligned table.
pub fn render(rep: &DiffReport, opts: &RenderOpts) -> String {
    let mut out = String::from("== Diff ==========================================\n");
    out.push_str(&format!("  from: {}\n", rep.from));
    out.push_str(&format!("  to:   {}\n", rep.to));
    if rep.reboot {
        out.push_str(
            "  note: the host rebooted in between, so since-boot counters\n\
             \x20       (disk and network) reset and are not comparable\n",
        );
    }
    out.push('\n');
    out.push_str(&format!(
        "  {:<26} {:>14} {:>14} {:>22}\n",
        "METRIC", "BEFORE", "AFTER", "DELTA"
    ));
    for r in &rep.rows {
        out.push_str(&format!(
            "  {:<26} {:>14} {:>14} {:>22}\n",
            truncate(&r.metric, 26),
            value(r.before, &r.unit, opts.units),
            value(r.after, &r.unit, opts.units),
            delta(r, opts.units),
        ));
    }
    out
}

fn value(v: f64, unit: &str, mode: UnitMode) -> String {
    match unit {
        "bytes" => render::bytes_mode(v.max(0.0) as u64, mode),
        "percent" => format!("{v:.1}%"),
        "seconds" => dur(v.max(0.0) as u64),
        _ => format!("{v:.0}"),
    }
}

fn delta(r: &DiffRow, mode: UnitMode) -> String {
    let sign = if r.delta < 0.0 { "-" } else { "+" };
    let mag = r.delta.abs();
    let base = match r.unit.as_str() {
        "bytes" => render::bytes_mode(mag as u64, mode),
        "percent" => format!("{mag:.1}%"),
        "seconds" => dur(mag as u64),
        _ => format!("{mag:.0}"),
    };
    let mut s = format!("{sign}{base}");
    if r.unit == "bytes"
        && r.delta != 0.0
        && let Some(p) = r.change_percent
    {
        s.push_str(&format!(" ({p:+.1}%)"));
    }
    s
}

/// Compact duration, e.g. `3d 4h`, `3h 39m`, `4m 12s`, `42s`.
fn dur(secs: u64) -> String {
    let (d, h, m, s) = (
        secs / 86400,
        (secs % 86400) / 3600,
        (secs % 3600) / 60,
        secs % 60,
    );
    if d > 0 {
        format!("{d}d {h}h")
    } else if h > 0 {
        format!("{h}h {m}m")
    } else if m > 0 {
        format!("{m}m {s}s")
    } else {
        format!("{s}s")
    }
}

fn truncate(s: &str, n: usize) -> String {
    if s.chars().count() <= n {
        s.to_string()
    } else {
        s.chars().take(n.saturating_sub(1)).collect::<String>() + "\u{2026}"
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{CpuInfo, MemoryInfo, SystemInfo};

    fn ov(uptime: u64, cpu: f32, mem_used: u64) -> Overview {
        Overview {
            system: SystemInfo {
                hostname: None,
                os_name: None,
                os_version: None,
                kernel_version: None,
                arch: "x86_64".to_string(),
                uptime_secs: uptime,
                load_average: None,
            },
            cpu: CpuInfo {
                model: "test".to_string(),
                physical_cores: None,
                logical_cores: 1,
                usage: cpu,
                cores: Vec::new(),
            },
            memory: MemoryInfo {
                total: 1000,
                used: mem_used,
                available: 1000 - mem_used,
                free: 0,
                swap_total: 0,
                swap_used: 0,
                used_percent: mem_used as f32 / 10.0,
            },
            disks: Vec::new(),
            networks: Vec::new(),
            processes: Vec::new(),
        }
    }

    #[test]
    fn report_has_deltas_and_spots_a_reboot() {
        let a = ov(1000, 10.0, 400);
        let b = ov(1100, 25.0, 500);
        let rep = report("a.json", "live", &a, &b);
        assert!(!rep.reboot);
        let cpu = r(&rep, "cpu usage");
        assert_eq!(cpu.delta, 15.0);
        assert_eq!(cpu.change_percent, Some(150.0));
        assert!(report("a", "b", &ov(10, 0.0, 0), &ov(5, 0.0, 0)).reboot);
    }

    fn r<'a>(rep: &'a DiffReport, metric: &str) -> &'a DiffRow {
        rep.rows
            .iter()
            .find(|x| x.metric == metric)
            .expect("metric is in the report")
    }

    #[test]
    fn durations_read_like_a_person_wrote_them() {
        assert_eq!(dur(42), "42s");
        assert_eq!(dur(252), "4m 12s");
        assert_eq!(dur(13_140), "3h 39m");
        assert_eq!(dur(273_600), "3d 4h");
    }

    #[test]
    fn snapshots_survive_windows_redirection_encodings() {
        assert_eq!(decode(b"{}"), "{}");
        let mut utf16 = vec![0xFF, 0xFE];
        for u in "{}".encode_utf16() {
            utf16.extend_from_slice(&u.to_le_bytes());
        }
        assert_eq!(decode(&utf16), "{}");
        let mut bom = vec![0xEF, 0xBB, 0xBF];
        bom.extend_from_slice(b"{}");
        assert_eq!(decode(&bom), "{}");
    }

    #[test]
    fn long_metric_names_are_clipped_not_wrapped() {
        assert_eq!(truncate("short", 26), "short");
        assert_eq!(truncate(&"x".repeat(40), 5), "xxxx\u{2026}");
    }
}
