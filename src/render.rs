use crate::model::*;
use crate::{Cli, ColorMode, Units};

/// Unit style for byte sizes.
#[derive(Clone, Copy, PartialEq)]
pub enum UnitMode {
    Iec,   // KiB, MiB, GiB (powers of 1024)
    Si,    // kB, MB, GB (powers of 1000)
    Bytes, // raw byte counts
}

/// Rendering options resolved from CLI flags.
pub struct RenderOpts {
    pub no_header: bool,
    pub color: bool,
    pub units: UnitMode,
}

impl RenderOpts {
    pub fn from_cli(cli: &Cli) -> Self {
        use std::io::IsTerminal;
        let color = match cli.color {
            ColorMode::Always => true,
            ColorMode::Never => false,
            ColorMode::Auto => {
                std::env::var_os("NO_COLOR").is_none() && std::io::stdout().is_terminal()
            }
        };
        RenderOpts {
            no_header: cli.no_header,
            color,
            units: match cli.units {
                Units::Iec => UnitMode::Iec,
                Units::Si => UnitMode::Si,
                Units::Bytes => UnitMode::Bytes,
            },
        }
    }
}

/// Renders the full overview.
pub fn overview_with(o: &Overview, opts: &RenderOpts) -> String {
    let mut out = String::new();
    out.push_str(&system(&o.system, opts));
    out.push('\n');
    out.push_str(&cpu(&o.cpu, opts));
    out.push('\n');
    out.push_str(&memory(&o.memory, opts));
    out.push('\n');
    out.push_str(&disks(&o.disks, opts));
    out.push('\n');
    out.push_str(&networks(&o.networks, opts));
    out.push('\n');
    out.push_str(&processes(&o.processes, opts));
    out
}

pub fn system(info: &SystemInfo, opts: &RenderOpts) -> String {
    let mut rows: Vec<(String, String)> = vec![
        ("Hostname".to_string(), value_or(&info.hostname)),
        ("OS".to_string(), os_line(info)),
        ("Kernel".to_string(), value_or(&info.kernel_version)),
        ("Arch".to_string(), info.arch.clone()),
        ("Uptime".to_string(), uptime(info.uptime_secs)),
    ];
    if let Some(la) = &info.load_average {
        rows.push((
            "Load avg".to_string(),
            format!("{:.2} {:.2} {:.2}", la.one, la.five, la.fifteen),
        ));
    }
    kv_section("System", &rows, opts)
}

pub fn cpu(info: &CpuInfo, opts: &RenderOpts) -> String {
    let mut rows: Vec<(String, String)> = vec![
        ("Model".to_string(), info.model.clone()),
        (
            "Cores".to_string(),
            match info.physical_cores {
                Some(p) => format!("{} physical / {} logical", p, info.logical_cores),
                None => format!("{} logical", info.logical_cores),
            },
        ),
        ("Usage".to_string(), heat(info.usage, pct(info.usage), opts)),
    ];
    for core in &info.cores {
        rows.push((
            format!("  {}", core.name),
            format!(
                "{} @ {} MHz",
                heat(core.usage, pct(core.usage), opts),
                core.frequency_mhz
            ),
        ));
    }
    kv_section("CPU", &rows, opts)
}

pub fn memory(info: &MemoryInfo, opts: &RenderOpts) -> String {
    let used_str = format!(
        "{} ({})",
        bytes_mode(info.used, opts.units),
        heat(info.used_percent, pct(info.used_percent), opts)
    );
    let rows = vec![
        ("Total", bytes_mode(info.total, opts.units)),
        ("Used", used_str),
        ("Available", bytes_mode(info.available, opts.units)),
        ("Free", bytes_mode(info.free, opts.units)),
        (
            "Swap",
            if info.swap_total == 0 {
                "none".to_string()
            } else {
                format!(
                    "{} / {} ({})",
                    bytes_mode(info.swap_used, opts.units),
                    bytes_mode(info.swap_total, opts.units),
                    pct_of(info.swap_used, info.swap_total)
                )
            },
        ),
    ];
    kv_section("Memory", &rows, opts)
}

pub fn disks(list: &[DiskInfo], opts: &RenderOpts) -> String {
    if list.is_empty() {
        return header("Disks", opts) + "  (none found)\n";
    }
    let headers = [
        "Filesystem",
        "Mount point",
        "Type",
        "Total",
        "Used",
        "Avail",
        "Use%",
        "Read",
        "Wrote",
    ];
    let rows: Vec<[String; 9]> = list
        .iter()
        .map(|d| {
            [
                d.name.clone(),
                d.mount_point.clone(),
                d.file_system.clone(),
                bytes_mode(d.total, opts.units),
                bytes_mode(d.used, opts.units),
                bytes_mode(d.available, opts.units),
                pct_of(d.used, d.total),
                bytes_mode(d.read_bytes, opts.units),
                bytes_mode(d.write_bytes, opts.units),
            ]
        })
        .collect();
    table_section("Disks", &headers, &rows, opts)
}

pub fn networks(list: &[NetworkInfo], opts: &RenderOpts) -> String {
    if list.is_empty() {
        return header("Networks", opts) + "  (none found)\n";
    }
    let headers = ["Interface", "Received", "Transmitted"];
    let rows: Vec<[String; 3]> = list
        .iter()
        .map(|n| {
            [
                n.name.clone(),
                bytes_mode(n.received, opts.units),
                bytes_mode(n.transmitted, opts.units),
            ]
        })
        .collect();
    table_section("Networks", &headers, &rows, opts)
}

pub fn processes(list: &[ProcessInfo], opts: &RenderOpts) -> String {
    if list.is_empty() {
        return header("Processes", opts) + "  (none found)\n";
    }
    let headers = ["PID", "User", "State", "CPU%", "MEM%", "Memory", "Name"];
    let rows: Vec<[String; 7]> = list
        .iter()
        .map(|p| {
            [
                p.pid.to_string(),
                p.user.clone().unwrap_or_else(|| "-".to_string()),
                p.state.clone(),
                heat(p.cpu_usage, format!("{:.1}%", p.cpu_usage), opts),
                heat(p.memory_percent, format!("{:.1}%", p.memory_percent), opts),
                bytes(p.memory),
                p.name.clone(),
            ]
        })
        .collect();
    table_section("Processes", &headers, &rows, opts)
}

pub fn vmstat(info: &VmstatInfo, opts: &RenderOpts) -> String {
    let rows = vec![
        (
            "Run/Block",
            format!("{} / {}", info.procs_running, info.procs_blocked),
        ),
        (
            "Memory",
            format!(
                "{} / {} ({})",
                bytes_mode(info.memory_used, opts.units),
                bytes_mode(info.memory_total, opts.units),
                pct_of(info.memory_used, info.memory_total)
            ),
        ),
        (
            "Swap",
            if info.swap_total == 0 {
                "none".to_string()
            } else {
                format!(
                    "{} / {} ({})",
                    bytes_mode(info.swap_used, opts.units),
                    bytes_mode(info.swap_total, opts.units),
                    pct_of(info.swap_used, info.swap_total)
                )
            },
        ),
        ("CPU", heat(info.cpu_usage, pct(info.cpu_usage), opts)),
        ("Uptime", uptime(info.uptime_secs)),
    ];
    kv_section("Vmstat", &rows, opts)
}

pub fn sensors(info: &SensorsInfo, opts: &RenderOpts) -> String {
    if info.temperatures.is_empty() {
        return header("Sensors", opts) + "  (no sensors reported)\n";
    }
    let headers = ["Sensor", "Temp", "Max", "Critical"];
    let rows: Vec<[String; 4]> = info
        .temperatures
        .iter()
        .map(|t| {
            [
                t.label.clone(),
                opt_temp(t.temperature_c, opts),
                opt_temp(t.max_c, opts),
                opt_temp(t.critical_c, opts),
            ]
        })
        .collect();
    table_section("Sensors", &headers, &rows, opts)
}

// --- formatting helpers ----------------------------------------------------

/// Color a percentage-like value: green < 70, yellow < 90, red >= 90.
fn heat(value: f32, text: String, opts: &RenderOpts) -> String {
    if !opts.color {
        return text;
    }
    let code = if value >= 90.0 {
        "31" // red
    } else if value >= 70.0 {
        "33" // yellow
    } else {
        "32" // green
    };
    format!("\x1b[{code}m{text}\x1b[0m")
}

fn opt_temp(t: Option<f32>, opts: &RenderOpts) -> String {
    match t {
        Some(v) if v > 0.0 && v < 500.0 => heat(v, format!("{v:.1} °C"), opts),
        _ => "-".to_string(),
    }
}

/// Section title line; suppressed by --no-header.
fn header(title: &str, opts: &RenderOpts) -> String {
    if opts.no_header {
        return String::new();
    }
    let mut s = format!("== {title} ");
    let pad = 44usize.saturating_sub(s.len());
    s.push_str(&"=".repeat(pad));
    s.push('\n');
    s
}

fn kv_section<K: AsRef<str>>(title: &str, rows: &[(K, String)], opts: &RenderOpts) -> String {
    let width = rows
        .iter()
        .map(|(k, _)| visible_len(k.as_ref()))
        .max()
        .unwrap_or(0);
    let mut out = header(title, opts);
    for (k, v) in rows {
        let k = k.as_ref();
        let pad = " ".repeat(width.saturating_sub(visible_len(k)));
        out.push_str(&format!("  {k}{pad}  {v}\n"));
    }
    out
}

fn table_section<const N: usize>(
    title: &str,
    headers: &[&str; N],
    rows: &[[String; N]],
    opts: &RenderOpts,
) -> String {
    let mut widths = [0usize; N];
    for (i, h) in headers.iter().enumerate() {
        widths[i] = h.len();
    }
    for row in rows {
        for (i, cell) in row.iter().enumerate() {
            widths[i] = widths[i].max(visible_len(cell));
        }
    }
    let mut out = header(title, opts);
    out.push_str("  ");
    for (i, h) in headers.iter().enumerate() {
        if i > 0 {
            out.push_str("  ");
        }
        out.push_str(&format!("{:<width$}", h, width = widths[i]));
    }
    out.push('\n');
    for row in rows {
        out.push_str("  ");
        for (i, cell) in row.iter().enumerate() {
            if i > 0 {
                out.push_str("  ");
            }
            let pad = " ".repeat(widths[i].saturating_sub(visible_len(cell)));
            out.push_str(cell);
            out.push_str(&pad);
        }
        out.push('\n');
    }
    out
}

/// Length of a string ignoring ANSI escape sequences.
fn visible_len(s: &str) -> usize {
    let mut len = 0;
    let mut chars = s.chars().peekable();
    while let Some(c) = chars.next() {
        if c == '\x1b' {
            // skip until 'm'
            for nc in chars.by_ref() {
                if nc == 'm' {
                    break;
                }
            }
        } else {
            len += 1;
        }
    }
    len
}

/// Human-readable byte size (IEC binary units).
pub fn bytes(n: u64) -> String {
    bytes_mode(n, UnitMode::Iec)
}

/// Human-readable byte size honoring the selected unit mode.
pub fn bytes_mode(n: u64, mode: UnitMode) -> String {
    match mode {
        UnitMode::Bytes => format!("{n} B"),
        UnitMode::Iec => {
            const UNITS: [&str; 6] = ["B", "KiB", "MiB", "GiB", "TiB", "PiB"];
            scaled(n, 1024.0, &UNITS)
        }
        UnitMode::Si => {
            const UNITS: [&str; 6] = ["B", "kB", "MB", "GB", "TB", "PB"];
            scaled(n, 1000.0, &UNITS)
        }
    }
}

fn scaled(n: u64, base: f64, units: &[&str; 6]) -> String {
    if (n as f64) < base {
        return format!("{n} B");
    }
    let mut value = n as f64;
    let mut unit = 0;
    while value >= base && unit < units.len() - 1 {
        value /= base;
        unit += 1;
    }
    format!("{:.1} {}", value, units[unit])
}

pub fn pct(x: f32) -> String {
    format!("{x:.1}%")
}

fn pct_of(part: u64, whole: u64) -> String {
    if whole == 0 {
        return "n/a".to_string();
    }
    format!("{:.1}%", part as f64 * 100.0 / whole as f64)
}

fn uptime(secs: u64) -> String {
    let days = secs / 86_400;
    let hours = (secs % 86_400) / 3_600;
    let mins = (secs % 3_600) / 60;
    if days > 0 {
        format!("{days}d {hours}h {mins}m")
    } else if hours > 0 {
        format!("{hours}h {mins}m")
    } else {
        format!("{mins}m")
    }
}

fn value_or(v: &Option<String>) -> String {
    v.clone().unwrap_or_else(|| "unknown".to_string())
}

fn os_line(info: &SystemInfo) -> String {
    match (&info.os_name, &info.os_version) {
        (Some(n), Some(v)) => format!("{n} {v}"),
        (Some(n), None) => n.clone(),
        (None, Some(v)) => v.clone(),
        (None, None) => "unknown".to_string(),
    }
}
