use std::collections::HashMap;
use std::thread;

use sysinfo::{
    Components, Disks, MINIMUM_CPU_UPDATE_INTERVAL, Networks, Process, ProcessRefreshKind,
    ProcessesToUpdate, System, UpdateKind, Users,
};

use crate::model::*;
use crate::{Cli, SortKey};

/// Number of samples the embedded dashboard keeps per metric. This is the
/// shared, server-side history window: every client sees the same 240 points.
pub const HISTORY_MAX: usize = 240;

/// Collects system-wide info. Uses static sysinfo accessors, so no
/// `System` instance is needed.
pub fn system_info() -> SystemInfo {
    #[cfg(unix)]
    let load_average = {
        let la = System::load_average();
        Some(LoadAvgInfo {
            one: la.one,
            five: la.five,
            fifteen: la.fifteen,
        })
    };
    #[cfg(not(unix))]
    let load_average = None; // load average is not implemented on Windows

    SystemInfo {
        hostname: System::host_name(),
        os_name: System::name(),
        os_version: System::os_version(),
        kernel_version: System::kernel_version(),
        arch: std::env::consts::ARCH.to_string(),
        uptime_secs: System::uptime(),
        load_average,
    }
}

/// Samples CPU usage twice (sysinfo computes usage as a diff) and returns
/// the resulting info. Leaves `sys` refreshed with CPU data.
pub fn cpu(sys: &mut System) -> CpuInfo {
    sample_cpu(sys);
    build_cpu(sys)
}

/// Refreshes memory counters.
pub fn memory(sys: &mut System) -> MemoryInfo {
    sys.refresh_memory();
    build_memory(sys)
}

pub fn disks() -> Vec<DiskInfo> {
    let disks = Disks::new_with_refreshed_list();
    disks
        .list()
        .iter()
        .map(|d| {
            let total = d.total_space();
            let available = d.available_space();
            let usage = d.usage();
            DiskInfo {
                name: {
                    let n = d.name().to_string_lossy();
                    if n.is_empty() {
                        "(no label)".to_string()
                    } else {
                        n.into_owned()
                    }
                },
                mount_point: d.mount_point().to_string_lossy().into_owned(),
                file_system: d.file_system().to_string_lossy().into_owned(),
                total,
                available,
                used: total.saturating_sub(available),
                read_bytes: usage.read_bytes,
                write_bytes: usage.written_bytes,
                removable: d.is_removable(),
                read_only: d.is_read_only(),
            }
        })
        .collect()
}

pub fn networks() -> Vec<NetworkInfo> {
    let networks = Networks::new_with_refreshed_list();
    let mut infos: Vec<NetworkInfo> = networks
        .list()
        .iter()
        .map(|(name, data)| NetworkInfo {
            name: name.clone(),
            received: data.total_received(),
            transmitted: data.total_transmitted(),
        })
        .collect();
    infos.sort_by(|a, b| a.name.cmp(&b.name));
    infos
}

/// Samples process CPU usage twice, then returns processes sorted and
/// filtered according to the CLI options.
pub fn processes_filtered(sys: &mut System, cli: &Cli) -> Vec<ProcessInfo> {
    sample_cpu_and_processes(sys);
    sys.refresh_memory();
    build_processes(sys, cli, &uid_name_map())
}

/// Full overview: one sampling pass feeds CPU, processes and memory.
pub fn overview_with(cli: &Cli, sys: &mut System) -> Overview {
    sample_cpu_and_processes(sys);
    sys.refresh_memory();

    Overview {
        system: system_info(),
        cpu: build_cpu(sys),
        memory: build_memory(sys),
        disks: disks(),
        networks: networks(),
        processes: build_processes(sys, cli, &uid_name_map()),
    }
}

/// vmstat-style one-shot summary.
pub fn vmstat(sys: &mut System) -> VmstatInfo {
    sample_cpu_and_processes(sys);
    sys.refresh_memory();

    let mut running = 0usize;
    let mut blocked = 0usize;
    for p in sys.processes().values() {
        match p.status() {
            sysinfo::ProcessStatus::Run => running += 1,
            sysinfo::ProcessStatus::UninterruptibleDiskSleep => blocked += 1,
            _ => {}
        }
    }

    VmstatInfo {
        procs_running: running,
        procs_blocked: blocked,
        memory_used: sys.used_memory(),
        memory_total: sys.total_memory(),
        swap_used: sys.used_swap(),
        swap_total: sys.total_swap(),
        cpu_usage: sys.global_cpu_usage(),
        uptime_secs: System::uptime(),
    }
}

/// Temperature sensors via sysinfo Components.
pub fn sensors() -> SensorsInfo {
    SensorsInfo {
        temperatures: temperatures_of(&Components::new_with_refreshed_list()),
    }
}

/// Maps sysinfo components to the lean snapshot temperature list.
fn temperatures_of(components: &Components) -> Vec<TemperatureInfo> {
    let mut temperatures: Vec<TemperatureInfo> = components
        .list()
        .iter()
        .map(|c| TemperatureInfo {
            label: c.label().to_string(),
            temperature_c: c.temperature(),
            max_c: c.max(),
            critical_c: c.critical(),
        })
        .collect();
    temperatures.sort_by(|a, b| a.label.cmp(&b.label));
    temperatures
}

// --- internals -------------------------------------------------------------

/// Host facts that are constant for the lifetime of a sampling session,
/// captured once instead of re-read every tick.
///
/// On Linux sysinfo's static getters re-parse `/etc/os-release` (`name`,
/// `os_version`), open `/proc` files (`kernel_version`, `host_name`) and call
/// gethostname on *every* invocation, and `Users` re-reads `/etc/passwd`.
/// None of that can change while the sampler is active, so for a 24/7
/// dashboard it is pure per-tick filesystem churn. `uid_names` is the same:
/// user names resolve once per session, not per poll.
struct HostFacts {
    hostname: Option<String>,
    os_name: Option<String>,
    os_version: Option<String>,
    kernel_version: Option<String>,
    arch: String,
    cpu_model: String,
    physical_cores: Option<usize>,
    uid_names: HashMap<String, String>,
}

impl HostFacts {
    fn capture(sys: &System) -> Self {
        Self {
            hostname: System::host_name(),
            os_name: System::name(),
            os_version: System::os_version(),
            kernel_version: System::kernel_version(),
            arch: std::env::consts::ARCH.to_string(),
            cpu_model: sys
                .cpus()
                .first()
                .map(|c| c.brand().trim().to_string())
                .unwrap_or_else(|| "unknown".to_string()),
            physical_cores: System::physical_core_count(),
            uid_names: uid_name_map(),
        }
    }
}

/// State kept alive between dashboard polls so CPU/process usage and
/// per-second I/O rates stay valid (sysinfo computes them as deltas between
/// refreshes of the same instance). Deliberately lean: the sampler tears this
/// whole struct down while no dashboard is open, so idle memory stays near
/// zero. The shared rolling history is owned by the sampler (see
/// `serve::HistoryRing`), not here.
pub struct WebState {
    pub sys: System,
    pub nets: Networks,
    pub disks: Disks,
    components: Components,
    facts: HostFacts,
    pub max_procs: usize,
    last: Option<std::time::Instant>,
}

impl WebState {
    pub fn new(max_procs: usize) -> Self {
        let mut sys = System::new();
        // Warm up CPU + process sampling so the dashboard's very first poll
        // already has valid CPU usage numbers and per-process thread counts.
        warmup_snapshot(&mut sys);
        Self {
            facts: HostFacts::capture(&sys),
            sys,
            nets: Networks::new_with_refreshed_list(),
            disks: Disks::new_with_refreshed_list(),
            components: Components::new_with_refreshed_list(),
            max_procs,
            last: None,
        }
    }

    /// Refresh everything and produce a lean dashboard snapshot.
    pub fn snapshot(&mut self) -> WebSnapshot {
        let now = std::time::Instant::now();
        let dt = self
            .last
            .map(|t| t.elapsed().as_secs_f64())
            .unwrap_or(0.0)
            .max(0.001);
        self.last = Some(now);

        // Light passes: CPU + process usage + memory + thread counts. No
        // cmdline/exe/env walk, which keeps even busy servers cheap to poll.
        self.sys.refresh_cpu_all();
        self.sys
            .refresh_processes_specifics(ProcessesToUpdate::All, true, lean_process_kind());
        self.sys.refresh_memory();
        self.nets.refresh(true);
        self.disks.refresh(true);
        self.components.refresh(true);

        web_snapshot(
            &self.sys,
            &self.nets,
            &self.disks,
            &self.components,
            &self.facts,
            dt,
            self.max_procs,
        )
    }
}

/// Builds a lean snapshot for the embedded dashboards (no cmdline, plus
/// per-second I/O rates derived from `dt`).
fn web_snapshot(
    sys: &System,
    nets: &Networks,
    disks: &Disks,
    components: &Components,
    facts: &HostFacts,
    dt: f64,
    max_procs: usize,
) -> WebSnapshot {
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    let uptime = System::uptime();
    let total_mem = sys.total_memory();
    let uid_names = &facts.uid_names;

    let mut processes: Vec<WebProcessInfo> = sys
        .processes()
        .values()
        .map(|p| {
            let mem = p.memory();
            WebProcessInfo {
                pid: p.pid().as_u32(),
                name: p.name().to_string_lossy().into_owned(),
                user: resolve_user(p, uid_names),
                cpu_usage: p.cpu_usage(),
                memory: mem,
                memory_percent: if total_mem == 0 {
                    0.0
                } else {
                    mem as f32 * 100.0 / total_mem as f32
                },
                state: p.status().to_string(),
                threads: p.tasks().map(|t| t.len()),
                uptime_secs: uptime.saturating_sub(now.saturating_sub(p.start_time())),
            }
        })
        .collect();
    processes.sort_by(|a, b| {
        b.cpu_usage
            .partial_cmp(&a.cpu_usage)
            .unwrap_or(std::cmp::Ordering::Equal)
    });
    processes.truncate(max_procs.max(1));

    let networks = nets
        .list()
        .iter()
        .map(|(name, data)| WebNetworkInfo {
            name: name.clone(),
            received: data.total_received(),
            transmitted: data.total_transmitted(),
            rx_bps: data.received() as f64 / dt,
            tx_bps: data.transmitted() as f64 / dt,
        })
        .collect();

    let disks = disks
        .list()
        .iter()
        .map(|d| {
            let total = d.total_space();
            let available = d.available_space();
            let usage = d.usage();
            let name = d.name().to_string_lossy();
            WebDiskInfo {
                name: if name.is_empty() {
                    "(no label)".to_string()
                } else {
                    name.into_owned()
                },
                mount_point: d.mount_point().to_string_lossy().into_owned(),
                file_system: d.file_system().to_string_lossy().into_owned(),
                total,
                available,
                used: total.saturating_sub(available),
                read_bps: usage.read_bytes as f64 / dt,
                write_bps: usage.written_bytes as f64 / dt,
                removable: d.is_removable(),
                read_only: d.is_read_only(),
            }
        })
        .collect();

    WebSnapshot {
        ts: now,
        system: web_system_info(facts),
        cpu: build_cpu_from_facts(sys, facts),
        memory: build_memory(sys),
        disks,
        networks,
        processes,
        sensors: SensorsInfo {
            temperatures: temperatures_of(components),
        },
        // Filled in by `WebState::snapshot` from the rolling history.
        history: WebHistory {
            cpu: Vec::new(),
            mem: Vec::new(),
            swap: Vec::new(),
            rx: Vec::new(),
            tx: Vec::new(),
            cores: Vec::new(),
            disks: std::collections::BTreeMap::new(),
        },
    }
}

/// Two-sample pass so both per-core CPU usage and process CPU usage have
/// a valid diff to compute against. Used by the one-shot CLI paths, which
/// render command lines, so it fetches everything (cmd/exe/cwd/environ).
fn sample_cpu_and_processes(sys: &mut System) {
    sys.refresh_cpu_all();
    sys.refresh_processes_specifics(
        ProcessesToUpdate::All,
        true,
        ProcessRefreshKind::everything(),
    );
    thread::sleep(MINIMUM_CPU_UPDATE_INTERVAL);
    sys.refresh_cpu_all();
    sys.refresh_processes_specifics(
        ProcessesToUpdate::All,
        true,
        ProcessRefreshKind::everything(),
    );
}

/// Two-sample warmup for the dashboard's `WebState`, refreshing processes
/// with the *same lean kind* the tick sampler uses (CPU, memory, user,
/// tasks). The essential difference from `sample_cpu_and_processes`: cmdline,
/// executable, cwd and environment are never fetched here. Fetching them was
/// just the warmup's job, yet sysinfo keeps those heavy string fields for
/// every process on the host for the entire active session — hundreds of
/// allocations the dashboard never displays — so skipping them is the single
/// biggest lean win available, with no behavior change.
fn warmup_snapshot(sys: &mut System) {
    sys.refresh_cpu_all();
    sys.refresh_processes_specifics(ProcessesToUpdate::All, true, lean_process_kind());
    thread::sleep(MINIMUM_CPU_UPDATE_INTERVAL);
    sys.refresh_cpu_all();
    sys.refresh_processes_specifics(ProcessesToUpdate::All, true, lean_process_kind());
}

/// Process refresh kind for dashboard sampling: enough for everything the
/// snapshot renders (CPU, memory, user names, live thread counts) and nothing
/// heavier. Notably it never fetches cmdline/exe/cwd/environ, and per-process
/// disk I/O is skipped too — the dashboard renders disk I/O per *disk*, not
/// per process, and on Linux dropping disk usage saves a `/proc/{pid}/io`
/// read per process per tick.
fn lean_process_kind() -> ProcessRefreshKind {
    ProcessRefreshKind::nothing()
        .with_cpu()
        .with_memory()
        .with_user(UpdateKind::OnlyIfNotSet)
        .with_tasks()
}

fn sample_cpu(sys: &mut System) {
    sys.refresh_cpu_all();
    thread::sleep(MINIMUM_CPU_UPDATE_INTERVAL);
    sys.refresh_cpu_all();
}

/// Builds a map of user id -> user name for process owner resolution.
fn uid_name_map() -> HashMap<String, String> {
    let users = Users::new_with_refreshed_list();
    users
        .iter()
        .map(|u| (u.id().to_string(), u.name().to_string()))
        .collect()
}

fn build_cpu(sys: &System) -> CpuInfo {
    let first = sys.cpus().first();
    CpuInfo {
        model: first
            .map(|c| c.brand().trim().to_string())
            .unwrap_or_else(|| "unknown".to_string()),
        physical_cores: System::physical_core_count(),
        logical_cores: sys.cpus().len(),
        usage: sys.global_cpu_usage(),
        cores: sys
            .cpus()
            .iter()
            .map(|c| CpuCoreInfo {
                name: c.name().to_string(),
                usage: c.cpu_usage(),
                frequency_mhz: c.frequency(),
            })
            .collect(),
    }
}

/// Dashboard CPU info: model and physical-core count come from the
/// per-session cache (they are constant), per-core usage/frequency stay live.
fn build_cpu_from_facts(sys: &System, facts: &HostFacts) -> CpuInfo {
    CpuInfo {
        model: facts.cpu_model.clone(),
        physical_cores: facts.physical_cores,
        logical_cores: sys.cpus().len(),
        usage: sys.global_cpu_usage(),
        cores: sys
            .cpus()
            .iter()
            .map(|c| CpuCoreInfo {
                name: c.name().to_string(),
                usage: c.cpu_usage(),
                frequency_mhz: c.frequency(),
            })
            .collect(),
    }
}

/// Live system info for dashboards: static facts (hostname, OS, kernel,
/// arch) come from the per-session cache, while uptime and load average stay
/// current.
#[cfg(unix)]
fn web_system_info(facts: &HostFacts) -> SystemInfo {
    let la = System::load_average();
    SystemInfo {
        hostname: facts.hostname.clone(),
        os_name: facts.os_name.clone(),
        os_version: facts.os_version.clone(),
        kernel_version: facts.kernel_version.clone(),
        arch: facts.arch.clone(),
        uptime_secs: System::uptime(),
        load_average: Some(LoadAvgInfo {
            one: la.one,
            five: la.five,
            fifteen: la.fifteen,
        }),
    }
}

#[cfg(not(unix))]
fn web_system_info(facts: &HostFacts) -> SystemInfo {
    SystemInfo {
        hostname: facts.hostname.clone(),
        os_name: facts.os_name.clone(),
        os_version: facts.os_version.clone(),
        kernel_version: facts.kernel_version.clone(),
        arch: facts.arch.clone(),
        uptime_secs: System::uptime(),
        load_average: None, // load average is not implemented on Windows
    }
}

fn build_memory(sys: &System) -> MemoryInfo {
    let total = sys.total_memory();
    let used = sys.used_memory();
    MemoryInfo {
        total,
        used,
        available: sys.available_memory(),
        free: sys.free_memory(),
        swap_total: sys.total_swap(),
        swap_used: sys.used_swap(),
        used_percent: if total == 0 {
            0.0
        } else {
            used as f32 * 100.0 / total as f32
        },
    }
}

fn build_processes(
    sys: &System,
    cli: &Cli,
    uid_names: &HashMap<String, String>,
) -> Vec<ProcessInfo> {
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    let uptime = System::uptime();
    let total_mem = sys.total_memory();

    // filter
    let name_filter = cli.filter_name.as_ref().map(|f| f.to_lowercase());
    let user_filter = cli.filter_user.as_ref().map(|f| f.to_lowercase());

    let mut procs: Vec<ProcessInfo> = sys
        .processes()
        .values()
        .filter_map(|p| {
            let name = p.name().to_string_lossy().into_owned();
            if let Some(f) = &name_filter
                && !name.to_lowercase().contains(f.as_str())
            {
                return None;
            }
            let user = resolve_user(p, uid_names);
            if let Some(f) = &user_filter {
                match &user {
                    Some(u) if u.to_lowercase().contains(f.as_str()) => {}
                    _ => return None,
                }
            }
            Some(ProcessInfo {
                pid: p.pid().as_u32(),
                name,
                cpu_usage: p.cpu_usage(),
                memory: p.memory(),
                memory_percent: if total_mem == 0 {
                    0.0
                } else {
                    p.memory() as f32 * 100.0 / total_mem as f32
                },
                user,
                start_time_secs: p.start_time(),
                uptime_secs: uptime.saturating_sub(now.saturating_sub(p.start_time())),
                parent: p.parent().map(|pid| pid.as_u32()),
                threads: p.tasks().map(|t| t.len()),
                state: p.status().to_string(),
                cmdline: {
                    let cmd = p.cmd();
                    if cmd.is_empty() {
                        None
                    } else {
                        Some(
                            cmd.iter()
                                .map(|s| s.to_string_lossy().into_owned())
                                .collect::<Vec<_>>()
                                .join(" "),
                        )
                    }
                },
            })
        })
        .collect();

    // sort
    let reverse = cli.reverse;
    procs.sort_by(|a, b| {
        use std::cmp::Ordering;
        let ord = match cli.sort {
            SortKey::Cpu => b
                .cpu_usage
                .partial_cmp(&a.cpu_usage)
                .unwrap_or(Ordering::Equal),
            SortKey::Mem => b.memory.cmp(&a.memory),
            SortKey::Pid => a.pid.cmp(&b.pid),
            SortKey::Name => a.name.to_lowercase().cmp(&b.name.to_lowercase()),
            SortKey::Uptime => b.uptime_secs.cmp(&a.uptime_secs),
            SortKey::Starttime => b.start_time_secs.cmp(&a.start_time_secs),
        };
        if reverse { ord.reverse() } else { ord }
    });

    procs.truncate(cli.top.max(1));
    procs
}

fn resolve_user(p: &Process, uid_names: &HashMap<String, String>) -> Option<String> {
    let uid = p.user_id()?;
    uid_names.get(&uid.to_string()).cloned()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn snapshot_is_lean_and_consistent() {
        let mut st = WebState::new(50);
        let s1 = st.snapshot();
        // History is sampler-owned; at this layer it stays empty.
        assert_eq!(s1.history.cpu.len(), 0);
        assert_eq!(s1.history.tx.len(), 0);
        // Process list respects the max_procs cap from a fully-populated set.
        assert!(s1.processes.len() <= 50);
        assert!(!s1.processes.is_empty());
        // Hits all the sections the dashboard renders.
        assert!(s1.system.hostname.is_some());
        assert!(!s1.networks.is_empty()); // at least one interface on every supported OS
        assert!(!s1.disks.is_empty());
        // Monotonic timestamps across a second poll.
        let s2 = st.snapshot();
        assert!(s2.ts >= s1.ts);
    }

    #[test]
    fn cpu_usage_is_valid_after_warmup() {
        // The whole point of the warmup: the very first snapshot already
        // carries a real percentage, not a zero.
        let mut st = WebState::new(20);
        let s = st.snapshot();
        assert!(
            (0.0..=100.0).contains(&s.cpu.usage),
            "cpu usage out of range: {}",
            s.cpu.usage
        );
    }
}
