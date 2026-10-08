mod collect;
mod model;
mod render;
mod serve;

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
    long_about = "One-shot system statistics viewer.\n\nExamples:\n  sysview                 Full overview\n  sysview cpu --json      CPU info as JSON\n  sysview procs --top 20  Top 20 processes by CPU\n  sysview serve           Web dashboard on :8080\n  sysview --watch --interval 1 --sort mem -r --top 15"
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
    /// Serve the embedded web dashboard (headless remote monitoring)
    Serve {
        /// HTTP port to listen on
        #[arg(long, default_value_t = 8080)]
        port: u16,
        /// Address to bind (default loopback; use 0.0.0.0 for remote access)
        #[arg(long, default_value = "127.0.0.1")]
        bind: String,
        /// Max processes sent to the dashboard
        #[arg(long, default_value_t = 100)]
        max_procs: usize,
        /// Require this token (?token= or Authorization: Bearer header)
        #[arg(long)]
        token: Option<String>,
        /// Start in kiosk mode (full screen, minimal chrome)
        #[arg(long)]
        kiosk: bool,
    },
}

fn main() {
    let cli = Cli::parse();

    // Serve the embedded web dashboard (blocks forever).
    if let Some(Section::Serve {
        port,
        bind,
        max_procs,
        token,
        kiosk,
    }) = cli.section.clone()
    {
        let cfg = serve::ServeConfig {
            bind,
            port,
            interval_secs: cli.interval.max(0.1),
            max_procs,
            token,
            kiosk,
        };
        if let Err(e) = serve::run(cfg) {
            eprintln!("sysview serve: {e}");
            std::process::exit(1);
        }
        return;
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
        Section::Serve { .. } => unreachable!("serve dispatch happens earlier"),
    };

    let mut out = output_stream(&cli);
    let _ = writeln!(out, "{output}");
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
