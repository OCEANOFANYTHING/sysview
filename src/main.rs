mod collect;
mod iface;
mod keys;
mod model;
mod paths;
mod render;
mod serve;
mod settings;
mod update;

use clap::{Parser, Subcommand, ValueEnum};
use serde::Serialize;
use std::io::Write;
use sysinfo::System;

#[derive(Debug, Clone, Copy, ValueEnum)]
pub enum ColorMode {
    Auto,
    Always,
    Never,
}

#[derive(Debug, Clone, Copy, ValueEnum)]
pub enum Units {
    Iec,
    Si,
    Bytes,
}

#[derive(Debug, Clone, Copy, PartialEq, ValueEnum)]
pub enum SortKey {
    Cpu,
    Mem,
    Pid,
    Name,
    Uptime,
    Starttime,
}

#[derive(Parser)]
#[command(
    name = "sysview",
    version,
    about = "Portable system stats viewer for Windows, Linux and macOS",
    long_about = "One-shot system statistics viewer.\n\nExamples:\n  sysview                 Full overview\n  sysview cpu --json      CPU info as JSON\n  sysview procs --top 20  Top 20 processes by CPU\n  sysview serve           Web dashboard on :8080\n  sysview serve --bind 0.0.0.0 --token <secret>\n                          Dashboard open to the LAN from any device\n  sysview --watch --interval 1 --sort mem -r --top 15"
)]
pub struct Cli {
    /// Section to display (defaults to the full overview)
    #[command(subcommand)]
    pub section: Option<Section>,

    /// Print machine-readable JSON instead of human-readable output
    #[arg(long, global = true)]
    pub json: bool,

    /// Number of processes to show
    #[arg(long, global = true, default_value_t = 10)]
    pub top: usize,

    /// Watch mode (continuous refresh)
    #[arg(long, short = 'w', global = true)]
    pub watch: bool,

    /// Refresh interval in seconds (for watch mode)
    #[arg(long, global = true, default_value_t = 1.0)]
    pub interval: f64,

    /// Sort key for processes
    #[arg(long, global = true, value_enum, default_value_t = SortKey::Cpu)]
    pub sort: SortKey,

    /// Reverse sort order
    #[arg(long, short = 'r', global = true)]
    pub reverse: bool,

    /// Filter by process name (substring, case-insensitive)
    #[arg(long, global = true)]
    pub filter_name: Option<String>,

    /// Filter by username
    #[arg(long, global = true)]
    pub filter_user: Option<String>,

    /// Omit section headers in human output
    #[arg(long, global = true)]
    pub no_header: bool,

    /// Color mode (auto detects TTY; respects NO_COLOR)
    #[arg(long, global = true, value_enum, default_value_t = ColorMode::Auto)]
    pub color: ColorMode,

    /// Units for byte sizes
    #[arg(long, global = true, value_enum, default_value_t = Units::Iec)]
    pub units: Units,
}

#[derive(Clone, Subcommand)]
pub enum Section {
    /// Full overview of every section
    Overview,
    /// System info: hostname, OS, kernel, uptime, load average
    Sys,
    /// CPU model, core counts and usage
    Cpu,
    /// Memory and swap usage
    Mem,
    /// Disks and partitions
    Disks,
    /// Network interfaces and traffic totals
    Net,
    /// Top processes (sort with --sort, filter with --filter-name/--filter-user)
    Procs,
    /// Virtual memory summary (vmstat-like)
    Vmstat,
    /// Temperatures and hardware sensors (if available)
    Sensors,
    /// Check live values against thresholds and exit non-zero on a breach
    /// (for cron, CI and monitoring probes)
    Check {
        /// Breach when CPU usage reaches this percent
        #[arg(long)]
        cpu: Option<f64>,
        /// Breach when memory usage reaches this percent
        #[arg(long)]
        mem: Option<f64>,
        /// Breach when swap usage reaches this percent
        #[arg(long)]
        swap: Option<f64>,
        /// Breach when any mounted volume reaches this percent
        #[arg(long)]
        disk: Option<f64>,
        /// Breach when the 1-minute load average reaches this value
        #[arg(long)]
        load: Option<f64>,
        /// Print only failing checks
        #[arg(long)]
        quiet: bool,
    },
    /// Serve the embedded web dashboard (headless remote monitoring)
    Serve {
        /// HTTP port to listen on
        #[arg(long)]
        port: Option<u16>,
        /// Address to bind (default loopback; use 0.0.0.0 for remote access)
        #[arg(long)]
        bind: Option<String>,
        /// Bind to an interface by name (e.g. wlan0 / "Wi-Fi"); resolves its IPv4
        #[arg(long, conflicts_with = "bind")]
        interface: Option<String>,
        /// Max processes sent to the dashboard
        #[arg(long)]
        max_procs: Option<usize>,
        /// Require this token (?token= or Authorization: Bearer header)
        #[arg(long)]
        token: Option<String>,
        /// Start in kiosk mode (full screen, minimal chrome)
        #[arg(long)]
        kiosk: bool,
        /// Alert when CPU usage reaches this percent
        #[arg(long, value_name = "PCT")]
        alert_cpu: Option<f64>,
        /// Alert when memory usage reaches this percent
        #[arg(long, value_name = "PCT")]
        alert_mem: Option<f64>,
        /// Alert when swap usage reaches this percent
        #[arg(long, value_name = "PCT")]
        alert_swap: Option<f64>,
        /// Alert when the 1-minute load average reaches this value
        #[arg(long, value_name = "N")]
        alert_load: Option<f64>,
        /// Alert when a mount reaches this percent, as MOUNT:PCT. Use `*` for
        /// the fullest mount. Repeatable: --alert-disk /:90 --alert-disk '*:95'
        #[arg(long, value_name = "MOUNT:PCT")]
        alert_disk: Vec<String>,
        /// Webhook URL to POST alerts to (ntfy, Slack, Discord or generic)
        #[arg(long)]
        alert_webhook: Option<String>,
        /// Minimum seconds between repeats of the same alert
        #[arg(long, default_value_t = 300)]
        alert_cooldown: u64,
    },
    /// Manage dashboard access keys (add, remove, expiry)
    Key {
        #[command(subcommand)]
        cmd: KeyCmd,
    },
    /// Update sysview to the latest release (no git/curl needed)
    Update {
        /// Only check for a newer release; download nothing
        #[arg(long)]
        check: bool,
        /// Reinstall even when already on the latest version
        #[arg(long)]
        force: bool,
    },
}

#[derive(Clone, Subcommand)]
pub enum KeyCmd {
    /// List configured keys (labels, masked tokens, expiry)
    List,
    /// Add a key (generates a token unless --token is given)
    Add {
        /// Human label for this key
        #[arg(long)]
        label: Option<String>,
        /// Use a specific token instead of a generated one
        #[arg(long)]
        token: Option<String>,
        /// Expiry: 7d, 30d, 90d, 6mo, 1y, never, or a date like 2026-12-31
        #[arg(long, default_value = "never")]
        expires: String,
        /// Remove every other file key in the same step
        #[arg(long)]
        replace: bool,
        /// Access level: admin (settings/keys/update) or view (read-only)
        #[arg(long, default_value = "admin")]
        scope: String,
    },
    /// Remove a key by token or label
    Rm {
        /// Token or label to remove
        ident: String,
    },
    /// Change a key's expiry
    Expire {
        /// Token or label
        ident: String,
        /// Expiry: 7d, 30d, 90d, 6mo, 1y, never, or a date like 2026-12-31
        #[arg(long, default_value = "never")]
        expires: String,
    },
    /// Change a key's access level (admin or view)
    Scope {
        /// Token or label
        ident: String,
        /// Access level: admin or view
        #[arg(long, default_value = "view")]
        scope: String,
    },
}

fn main() {
    let cli = Cli::parse();

    // Clear a stale `<exe>.old` left by a previous Windows self-update, on
    // whatever path this invocation takes (serve included).
    update::cleanup_old_binary();

    // Subcommands that run to completion on their own (check / serve / key / update).
    match cli.section.clone() {
        Some(Section::Check {
            cpu,
            mem,
            swap,
            disk,
            load,
            quiet,
        }) => {
            run_check(cpu, mem, swap, disk, load, quiet, cli.json);
            return;
        }
        Some(Section::Serve {
            port,
            bind,
            interface,
            max_procs,
            token,
            kiosk,
            alert_cpu,
            alert_mem,
            alert_swap,
            alert_load,
            alert_disk,
            alert_webhook,
            alert_cooldown,
        }) => {
            let alerts = serve::AlertConfig {
                cpu: alert_cpu,
                mem: alert_mem,
                swap: alert_swap,
                load: alert_load,
                disks: parse_alert_disks(&alert_disk),
                webhook: alert_webhook,
                cooldown: alert_cooldown,
            };
            run_serve(
                cli.interval,
                port,
                bind,
                interface,
                max_procs,
                token,
                kiosk,
                alerts,
            );
            return;
        }
        Some(Section::Key { cmd }) => {
            run_key(cmd);
            return;
        }
        Some(Section::Update { check, force }) => {
            run_update(check, force);
            return;
        }
        _ => {}
    }

    if cli.watch {
        run_watch(&cli);
        return;
    }

    let mut sys = System::new();
    let opts = render::RenderOpts::from_cli(&cli);
    let section = cli.section.clone().unwrap_or(Section::Overview);
    let output = match section {
        Section::Overview => {
            let data = collect::overview_with(&cli, &mut sys);
            emit(&data, |d| render::overview_with(d, &opts), cli.json)
        }
        Section::Sys => {
            let data = collect::system_info();
            emit(&data, |d| render::system(d, &opts), cli.json)
        }
        Section::Cpu => {
            let data = collect::cpu(&mut sys);
            emit(&data, |d| render::cpu(d, &opts), cli.json)
        }
        Section::Mem => {
            let data = collect::memory(&mut sys);
            emit(&data, |d| render::memory(d, &opts), cli.json)
        }
        Section::Disks => {
            let data = collect::disks();
            emit(&data, |d| render::disks(d, &opts), cli.json)
        }
        Section::Net => {
            let data = collect::networks();
            emit(&data, |d| render::networks(d, &opts), cli.json)
        }
        Section::Procs => {
            let data = collect::processes_filtered(&mut sys, &cli);
            emit(&data, |d| render::processes(d, &opts), cli.json)
        }
        Section::Vmstat => {
            let data = collect::vmstat(&mut sys);
            emit(&data, |d| render::vmstat(d, &opts), cli.json)
        }
        Section::Sensors => {
            let data = collect::sensors();
            emit(&data, |d| render::sensors(d, &opts), cli.json)
        }
        // Handled above (returns before this match runs).
        Section::Check { .. } => unreachable!("check dispatch happens earlier"),
        Section::Serve { .. } => unreachable!("serve dispatch happens earlier"),
        Section::Key { .. } => unreachable!("key dispatch happens earlier"),
        Section::Update { .. } => unreachable!("update dispatch happens earlier"),
    };

    let mut out = output_stream(&cli);
    let _ = writeln!(out, "{output}");
}

/// Parses `--alert-disk MOUNT:PCT` entries. `*` matches the fullest volume;
/// the split is on the last colon so Windows mounts like `C:\` work too.
fn parse_alert_disks(specs: &[String]) -> Vec<(String, f64)> {
    let bad = |s: &str| -> ! {
        fail(&format!(
            "sysview serve: bad --alert-disk '{s}' (want MOUNT:PCT, e.g. /:90)"
        ))
    };
    let mut out = Vec::new();
    for s in specs {
        match s.rsplit_once(':') {
            Some((mount, pct)) if !mount.trim().is_empty() => match pct.trim().parse::<f64>() {
                Ok(v) => out.push((mount.trim().to_string(), v)),
                Err(_) => bad(s),
            },
            _ => bad(s),
        }
    }
    out
}

/// Resolves the effective serve config (CLI > saved settings > defaults),
/// turning `--interface`/a saved interface into a bind address, then runs the
/// dashboard to completion.
#[allow(clippy::too_many_arguments)]
fn run_serve(
    interval_flag: f64,
    port: Option<u16>,
    bind: Option<String>,
    interface: Option<String>,
    max_procs: Option<usize>,
    token: Option<String>,
    kiosk: bool,
    alerts: serve::AlertConfig,
) {
    let settings_path = paths::settings_file();
    let stored = settings::Settings::load(&settings_path);

    // Explicit `--bind` beats a saved interface; explicit `--interface` beats a
    // saved one; a saved interface applies only when neither was given.
    let interface = if bind.is_some() {
        None
    } else {
        interface.or_else(|| stored.interface.clone())
    };
    let bind = match &interface {
        Some(name) => match iface::resolve_ipv4(name) {
            Some(ip) => ip.to_string(),
            None => {
                eprintln!("sysview serve: interface '{name}' has no IPv4 address.");
                eprint!("{}", iface::describe_all());
                std::process::exit(1);
            }
        },
        None => bind
            .or_else(|| stored.bind.clone())
            .unwrap_or_else(|| "127.0.0.1".to_string()),
    };
    let cfg = serve::ServeConfig {
        bind,
        port: port.or(stored.port).unwrap_or(8080),
        interval_secs: stored.interval_secs.unwrap_or(interval_flag).max(0.1),
        max_procs: max_procs.or(stored.max_procs).unwrap_or(100),
        token,
        kiosk: kiosk || stored.kiosk.unwrap_or(false),
        interface,
        store: None,
        settings_path: Some(settings_path),
        alerts,
    };
    if let Err(e) = serve::run(cfg) {
        eprintln!("sysview serve: {e}");
        std::process::exit(1);
    }
}

/// `sysview key …`: manages the keys file (env keys are always read-only).
fn run_key(cmd: KeyCmd) {
    let store = keys::KeyStore::from_env();
    let now = keys::now_unix();
    match cmd {
        KeyCmd::List => {
            let entries = store.entries();
            if entries.is_empty() {
                println!("no access keys configured.");
                println!("add one with:  sysview key add --expires 30d");
                return;
            }
            println!(
                "{:<20} {:<20} {:<6} {:<6} EXPIRES",
                "LABEL", "TOKEN", "SOURCE", "SCOPE"
            );
            for (k, from_env) in entries {
                let label = if k.label.is_empty() {
                    "-"
                } else {
                    k.label.as_str()
                };
                let source = if from_env { "env" } else { "file" };
                let expiry = if k.expires == 0 {
                    "never".to_string()
                } else if k.expired(now) {
                    format!("expired ({})", keys::format_time(k.expires))
                } else {
                    keys::format_time(k.expires)
                };
                println!(
                    "{:<20} {:<20} {:<6} {:<6} {}",
                    label,
                    k.masked(),
                    source,
                    k.scope.as_str(),
                    expiry
                );
            }
            println!("\nkeys file: {}", store.file().display());
        }
        KeyCmd::Add {
            label,
            token,
            expires,
            replace,
            scope,
        } => {
            let exp = match keys::parse_expiry(&expires, now) {
                Ok(e) => e,
                Err(e) => fail(&format!("sysview key: {e}")),
            };
            let scope = match keys::Scope::parse(&scope) {
                Ok(s) => s,
                Err(e) => fail(&format!("sysview key: {e}")),
            };
            let token = token
                .filter(|t| !t.trim().is_empty())
                .unwrap_or_else(keys::generate_token);
            let label = label.unwrap_or_default();
            if let Err(e) = store.add(keys::Key {
                token: token.clone(),
                label: label.clone(),
                expires: exp,
                scope,
            }) {
                fail(&format!("sysview key: {e}"));
            }
            if replace && let Err(e) = store.keep_only(&token) {
                fail(&format!("sysview key: {e}"));
            }
            let what = if label.is_empty() {
                "added key".to_string()
            } else {
                format!("added key '{label}'")
            };
            let extra = if replace {
                " (replaced all others)"
            } else {
                ""
            };
            println!("{what}{extra}");
            println!("  token:   {token}");
            println!("  scope:   {}", scope.as_str());
            println!(
                "  expires: {}",
                if exp == 0 {
                    "never".to_string()
                } else {
                    keys::format_time(exp)
                }
            );
            println!("\nopen the dashboard with ?token={token}");
            println!("or pin it in a service environment variable: SYSVIEW_TOKEN={token}");
        }
        KeyCmd::Rm { ident } => match store.remove(&ident) {
            Ok(true) => println!("removed key '{ident}'"),
            Ok(false) => fail(&format!(
                "sysview key: no file key matches '{ident}' (environment keys are read-only)"
            )),
            Err(e) => fail(&format!("sysview key: {e}")),
        },
        KeyCmd::Expire { ident, expires } => {
            let exp = match keys::parse_expiry(&expires, now) {
                Ok(e) => e,
                Err(e) => fail(&format!("sysview key: {e}")),
            };
            match store.set_expiry(&ident, exp) {
                Ok(true) => println!(
                    "set expiry for '{ident}' to {}",
                    if exp == 0 {
                        "never".to_string()
                    } else {
                        keys::format_time(exp)
                    }
                ),
                Ok(false) => fail(&format!(
                    "sysview key: no file key matches '{ident}' (environment keys are read-only)"
                )),
                Err(e) => fail(&format!("sysview key: {e}")),
            }
        }
        KeyCmd::Scope { ident, scope } => {
            let scope = match keys::Scope::parse(&scope) {
                Ok(s) => s,
                Err(e) => fail(&format!("sysview key: {e}")),
            };
            match store.set_scope(&ident, scope) {
                Ok(true) => println!("set scope for '{ident}' to {}", scope.as_str()),
                Ok(false) => fail(&format!(
                    "sysview key: no file key matches '{ident}' (environment keys are read-only)"
                )),
                Err(e) => fail(&format!("sysview key: {e}")),
            }
        }
    }
}

/// `sysview update [--check] [--force]`.
fn run_update(check: bool, force: bool) {
    if check {
        match update::check() {
            Ok(update::Outcome::Available { current, latest }) => {
                println!("update available: {current} -> {latest}");
            }
            Ok(update::Outcome::UpToDate { version }) => {
                println!("sysview {version} is up to date");
            }
            Ok(_) => {}
            Err(e) => fail(&format!("sysview update: {e}")),
        }
        return;
    }
    match update::perform(force) {
        Ok(update::Outcome::Updated { from, to }) => {
            println!("updated sysview {from} -> {to}");
            println!("restart the dashboard to run the new build (the token is unchanged):");
            println!("  systemctl restart sysview    # or just reopen the dashboard");
        }
        Ok(update::Outcome::UpToDate { version }) => {
            println!("sysview {version} is already up to date (use --force to reinstall)");
        }
        Ok(update::Outcome::Available { current, latest }) => {
            println!("update available: {current} -> {latest} (run without --check to install)");
        }
        Err(e) => fail(&format!("sysview update: {e}")),
    }
}

/// `sysview check …`: compares live values against thresholds and exits `0`
/// when every check passes, `1` on any breach. One line per check (or JSON
/// with `--json`), so it drops straight into cron, CI and monitoring probes.
fn run_check(
    cpu: Option<f64>,
    mem: Option<f64>,
    swap: Option<f64>,
    disk: Option<f64>,
    load: Option<f64>,
    quiet: bool,
    json: bool,
) {
    let m = collect::check_metrics();
    let mut items: Vec<model::CheckItem> = Vec::new();
    if let Some(limit) = cpu {
        items.push(model::CheckItem::new("cpu", m.cpu_percent, limit));
    }
    if let Some(limit) = mem {
        items.push(model::CheckItem::new("mem", m.mem_percent, limit));
    }
    if let Some(limit) = swap {
        items.push(model::CheckItem::new("swap", m.swap_percent, limit));
    }
    if let Some(limit) = load
        && let Some(l) = m.load_one
    {
        items.push(model::CheckItem::new("load1", l, limit));
    }
    if let Some(limit) = disk {
        for (mount, pct) in &m.disks {
            items.push(model::CheckItem::new(format!("disk {mount}"), *pct, limit));
        }
    }

    let ok = items.iter().all(|i| i.ok);
    if json {
        let report = model::CheckReport { ok, checks: items };
        println!(
            "{}",
            serde_json::to_string_pretty(&report).unwrap_or_default()
        );
    } else if items.is_empty() {
        println!("no thresholds given; pass --cpu/--mem/--swap/--disk/--load");
    } else {
        for i in &items {
            if quiet && i.ok {
                continue;
            }
            println!(
                "{:<4} {:<24} {:>7.1}%  limit {:>6.1}%{}",
                if i.ok { "ok" } else { "FAIL" },
                i.name,
                i.value,
                i.limit,
                if i.ok { "" } else { "  <-- BREACH" }
            );
        }
    }
    std::process::exit(if ok { 0 } else { 1 });
}

fn fail(msg: &str) -> ! {
    eprintln!("{msg}");
    std::process::exit(1);
}

/// Adapts stdout for the CLI `--color` choice: `auto` enables VT processing
/// on Windows consoles and strips codes when piping; `always` forces codes
/// through; `never` strips them.
fn output_stream(cli: &Cli) -> Box<dyn Write> {
    match cli.color {
        ColorMode::Always => Box::new(anstream::AutoStream::always(std::io::stdout())),
        ColorMode::Never => Box::new(anstream::AutoStream::never(std::io::stdout())),
        ColorMode::Auto => Box::new(anstream::stdout()),
    }
}

fn run_watch(cli: &Cli) {
    if cli.json {
        eprintln!(
            "note: --json with --watch prints one snapshot per clear; consider piping instead"
        );
    }
    let mut sys = System::new();
    let interval = cli.interval.max(0.1);
    let opts = render::RenderOpts::from_cli(cli);
    let mut out = output_stream(cli);

    loop {
        let _ = write!(out, "\x1b[2J\x1b[H");
        let _ = out.flush();
        let data = collect::overview_with(cli, &mut sys);
        let _ = writeln!(out, "{}", render::overview_with(&data, &opts));
        let _ = out.flush();
        std::thread::sleep(std::time::Duration::from_secs_f64(interval));
    }
}

fn emit<T: Serialize>(data: &T, human: impl FnOnce(&T) -> String, json: bool) -> String {
    if json {
        serde_json::to_string_pretty(data).expect("JSON serialization failed")
    } else {
        human(data)
    }
}
