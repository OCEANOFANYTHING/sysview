//! `sysview export`: one live sample in a format another tool can ingest —
//! Prometheus text or CSV (for cron, CI, a spreadsheet or a scraper).

use std::fmt::Write as _;

use crate::collect;
use crate::{Cli, ExportSection};

/// Prometheus text for a live sample. Reuses the dashboard's own renderer (the
/// one behind `/metrics`) so the two can never drift apart.
pub fn prometheus(max_procs: usize) -> String {
    let mut state = collect::WebState::new(max_procs.max(1));
    let snap = state.snapshot();
    let bytes = serde_json::to_vec(&snap).expect("snapshot serialization failed");
    crate::serve::json_to_metrics(&bytes)
}

/// CSV for one section. Each section is its own table (its own header), which
/// keeps every row the same shape for the parser on the other end.
pub fn csv(section: ExportSection, cli: &Cli) -> String {
    match section {
        ExportSection::Host => host_csv(),
        ExportSection::Procs => procs_csv(cli),
        ExportSection::Disks => disks_csv(),
        ExportSection::Net => net_csv(),
    }
}

fn host_csv() -> String {
    let mut out = String::from("section,metric,value\n");
    let facts = collect::system_info();
    let mut sys = sysinfo::System::new();
    let cpu = collect::cpu(&mut sys);
    let mem = collect::memory(&mut sys);

    let mut put = |section: &str, metric: &str, value: String| {
        let _ = writeln!(out, "{section},{metric},{}", csv_field(&value));
    };
    let mut put_opt = |section: &str, metric: &str, value: Option<String>| {
        if let Some(v) = value {
            put(section, metric, v);
        }
    };

    put_opt("system", "hostname", facts.hostname);
    put_opt("system", "os", facts.os_name);
    put_opt("system", "os_version", facts.os_version);
    put_opt("system", "kernel", facts.kernel_version);
    put("system", "arch", facts.arch);
    put("system", "uptime_secs", facts.uptime_secs.to_string());
    if let Some(la) = facts.load_average {
        put("system", "load1", format!("{:.2}", la.one));
        put("system", "load5", format!("{:.2}", la.five));
        put("system", "load15", format!("{:.2}", la.fifteen));
    }
    put("cpu", "model", cpu.model);
    if let Some(cores) = cpu.physical_cores {
        put("cpu", "physical_cores", cores.to_string());
    }
    put("cpu", "logical_cores", cpu.logical_cores.to_string());
    put("cpu", "usage_percent", format!("{:.2}", cpu.usage));
    put("memory", "total_bytes", mem.total.to_string());
    put("memory", "used_bytes", mem.used.to_string());
    put("memory", "available_bytes", mem.available.to_string());
    put("memory", "free_bytes", mem.free.to_string());
    put("memory", "swap_total_bytes", mem.swap_total.to_string());
    put("memory", "swap_used_bytes", mem.swap_used.to_string());
    put("memory", "used_percent", format!("{:.2}", mem.used_percent));
    out
}

fn procs_csv(cli: &Cli) -> String {
    let mut out = String::from(
        "pid,name,user,cpu_percent,memory_bytes,memory_percent,state,threads,uptime_secs\n",
    );
    let mut sys = sysinfo::System::new();
    for p in collect::processes_filtered(&mut sys, cli) {
        let _ = writeln!(
            out,
            "{},{},{},{:.1},{},{:.2},{},{},{}",
            p.pid,
            csv_field(&p.name),
            csv_field(p.user.as_deref().unwrap_or("")),
            p.cpu_usage,
            p.memory,
            p.memory_percent,
            csv_field(&p.state),
            p.threads.map(|t| t.to_string()).unwrap_or_default(),
            p.uptime_secs,
        );
    }
    out
}

fn disks_csv() -> String {
    let mut out =
        String::from("name,mount,fs,total_bytes,available_bytes,used_bytes,used_percent\n");
    for d in collect::disks() {
        let pct = if d.total == 0 {
            0.0
        } else {
            d.used as f64 * 100.0 / d.total as f64
        };
        let _ = writeln!(
            out,
            "{},{},{},{},{},{},{:.2}",
            csv_field(&d.name),
            csv_field(&d.mount_point),
            csv_field(&d.file_system),
            d.total,
            d.available,
            d.used,
            pct,
        );
    }
    out
}

fn net_csv() -> String {
    let mut out = String::from("name,received_bytes,transmitted_bytes\n");
    for n in collect::networks() {
        let _ = writeln!(
            out,
            "{},{},{}",
            csv_field(&n.name),
            n.received,
            n.transmitted
        );
    }
    out
}

/// RFC 4180 quoting: only when the field needs it.
fn csv_field(s: &str) -> String {
    if s.contains([',', '"', '\n', '\r']) {
        format!("\"{}\"", s.replace('"', "\"\""))
    } else {
        s.to_string()
    }
}

#[cfg(test)]
mod tests {
    use super::csv_field;

    #[test]
    fn csv_fields_are_quoted_only_when_needed() {
        assert_eq!(csv_field("plain"), "plain");
        assert_eq!(csv_field("a,b"), "\"a,b\"");
        assert_eq!(csv_field("say \"hi\""), "\"say \"\"hi\"\"\"");
        assert_eq!(csv_field("two\nlines"), "\"two\nlines\"");
    }
}
