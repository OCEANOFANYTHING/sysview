# sysview

Portable one-shot system stats viewer for Windows, Linux and macOS.

Docs: usage below · maintainers/integrators: [docs/ARCHITECTURE.md](docs/ARCHITECTURE.md)
(sampling model, idle lifecycle, HTTP protocol, `/metrics` pipeline).

## Project layout

```
src/                Rust source
  main.rs           CLI parsing (clap) + section dispatch
  collect.rs        sampling: WebState, lean warmup, HostFacts cache
  model.rs          serialized snapshot types
  render.rs         terminal output (human + JSON)
  serve.rs          HTTP server, sampler thread, history ring, /metrics
web/dashboard.html  embedded ES5 dashboard (include_str!, no build step)
scripts/            build-linux.sh · setup-debian.sh (Debian/systemd)
dist/               build-linux.sh output goes here; prebuilt binaries ship
                    via GitHub Releases (Windows / Linux / macOS)
docs/               architecture & design notes
```

## Install

sysview ships as **one self-contained binary**. Nothing else needs to be
installed to run it — no runtime, no libraries, no Node, no Python. On Linux
the prebuilt binary is fully static (musl), so the exact same file runs on any
distro — Debian, Ubuntu, RHEL, Alpine — headless servers included. Rust is
required **only** if you build from source, and can be uninstalled afterwards;
the compiled binary never needs it again.

**Option 1 — precompiled binary (recommended, no Rust needed).** Download your
platform's file from the [Releases] page and run it directly:

| Platform | File |
|---|---|
| Windows | `sysview-windows-x86_64.exe` |
| Linux (any distro, incl. servers) | `sysview-linux-x86_64-musl` (fully static) |
| macOS (Intel + Apple Silicon) | `sysview-mac-universal` |

```bash
# Linux / macOS
chmod +x sysview-linux-x86_64-musl
./sysview-linux-x86_64-musl
```

Verify the checksums in `SHA256SUMS` on the Release page before running.

**Option 2 — build from source (Rust is build-time only).** The produced
binary is identical in kind to the prebuilt one and needs no Rust to run:

```bash
curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh -s -- -y
source "$HOME/.cargo/env"
git clone https://github.com/OCEANOFANYTHING/sysview.git && cd sysview
cargo build --release          # Windows: same command, target\release\sysview.exe
./target/release/sysview       # runs on its own — no Rust needed at runtime
# optional: rustup self uninstall -y     # remove the toolchain; the binary keeps working
```

Only the build needs Rust; the deployed artifact is just `sysview`. There is no
second install step, no shared library to keep in sync, and nothing to leave
behind when you uninstall the toolchain — on a lean server, build, copy the
binary, remove Rust.

[Releases]: https://github.com/OCEANOFANYTHING/sysview/releases

## Usage

```text
sysview                                  Full overview
sysview cpu --json                       CPU info as JSON
sysview mem --units si                   Memory in decimal units
sysview disks                            Disks/partitions + I/O since start
sysview net                              Network totals
sysview procs --top 20                   Top 20 processes by CPU
sysview procs --sort mem -r              Top 20 by memory (mem is already
                                         descending, -r gives ascending)
sysview procs --filter-name code         Only processes matching "code"
sysview vmstat                           vmstat-style summary
sysview sensors                          Temperature sensors (if reported)
sysview --watch --interval 1             Live refresh every second
sysview serve                            Embedded web dashboard on :8080
sysview serve --port 9090 --bind 0.0.0.0 Remote dashboard on all interfaces
sysview serve --kiosk                    Start in kiosk mode (fullscreen)
sysview serve --token s3cret             Require ?token=s3cret access
```

## Embedded web dashboard

`sysview serve` starts a small HTTP server built into the binary (no node, no
separate process) that serves a live, dark-theme monitoring page:

- Auto-refreshing gauges, charts/sparklines, and full stats (CPU, memory,
  disks, networks, sensors, processes), with per-core CPU bars and usage bars
  that heat up (green → amber → red) as load climbs.
- **Peak markers:** every chart and sparkline draws a dashed high-water line
  and label at the window max — no hovering needed.
- **Anomaly dots:** the CPU chart marks samples that stand out far above a
  robust window baseline (median + 3·MAD, floored) with small yellow dots —
  a hidden spike jumps out even when the chart is otherwise calm.
- **Session stats:** the CPU header and the memory panel show window
  avg / peak from the same shared ring that feeds the charts.
- **Disk-full forecast:** under each mount's trend, a "full ~Nd" / "~Nw" text
  from a linear fit on the server-side usage history — shown only while the
  disk is genuinely trending upward (amber < 60 d, red < 14 d).
- **Health score:** a badge in the header (0–100) weights CPU, memory, swap
  and the fullest disk; green ≥ 80, amber ≥ 60, red pulses below that.
- **Timeline markers:** press `M` to drop a dashed purple vertical line on the
  chart (persisted in localStorage, capped at 20); `Shift+M` clears them all.
- **Staleness readout:** the status bar shows "· Xs ago" and the whole bar
  fades out when the feed goes quiet (paused, server idle warm-up, or offline)
  for more than 15 s or three missed polls.
- **Alert flags:** gauge cards flash a pulsing border when CPU/memory/swap pass
  70% (amber) or 90% (red); disk rows light up the same way by usage.
- **Per-core history:** each logical core gets its own mini sparkline fed by a
  server-side per-core ring (~15 KB for 16 cores).
- **Disk donuts + trends:** each mount gets a donut gauge and a usage-trend
  sparkline from a per-mount server-side ring (a few KB).
- **Mini trends:** per-interface RX/TX and per-sensor temperature sparklines,
  accumulated entirely client-side (zero server memory).
- **Header load line:** load average (Unix only) with a tiny sparkline plus
  uptime, shown for every platform; Windows falls back to uptime alone.
- **Persisted prefs:** refresh interval, process sort and the name/user filter
  are remembered in localStorage across reloads.
- **Download:** the "↓" button saves the current snapshot as a JSON file,
  built entirely in the browser.
- **Shared history:** the server samples continuously and keeps the rolling
  240-sample window itself. Every device — and every reload — fetches the same
  server-side history, so all screens show identical charts from the moment
  they connect (nothing "starts over" on refresh).
- **Idle-friendly:** sampling runs only while at least one dashboard is open
  (plus a short grace period for reloads). With zero viewers the server drops
  its sampling state and sits at near-zero memory (≈4 MB private, measured on a
  headless Debian 13 server — the exact figure varies by platform) until
  someone connects again; the shared history
  window — including the per-core and per-mount rings — is preserved across
  idle periods. Even while active it stays lean: process sampling deliberately
  never fetches command lines, environments or executables (sysinfo otherwise
  retains those per-process strings for the whole session), so a live dashboard
  tops out at well under 1 MB over idle. Memory stays bounded in the remaining
  dimensions too: each connection handler runs on a small 256 KiB stack (head
  parsing and response writes are shallow; even the /metrics JSON decode is
  bounded-depth), so all 64 slots commit at most ~16 MiB of thread stacks, and
  the sampler reuses its serialized-payload buffer across ticks instead of
  reallocating per sample.
- Sortable/filterable process table (click a header, or type in the filter).
- Kiosk / full-screen mode for wall displays: open
  `http://host:8080/?kiosk=1`, or press `K` on the page. `--kiosk` starts
  every session in kiosk mode. In kiosk the status bar auto-dims after 8 s of
  inactivity, the clock is enlarged, and any touch/key/mouse wakes it.
- Keyboard shortcuts: `F` fullscreen, `K` kiosk, `P` pause/resume, `M` drop a
  chart marker, `Shift+M` clear markers.
- Works on old browsers (ES5, no CSS Grid — usable from Android 4 webviews).

### Prometheus metrics

`sysview serve` also exposes a Prometheus endpoint at **`/metrics`** (text
format 0.0.4). It covers uptime, load average, CPU, memory, swap, per-mount
disk usage and I/O rates, per-interface network totals/rates and per-sensor
temperature with proper label escaping:

```promql
sysview_cpu_usage_percent
sysview_memory_used_percent{mount="C:\"}
sysview_disk_used_percent{mount="C:\\"}
sysview_net_rx_bps{interface="eth0"}
sysview_sensor_temperature_celsius{sensor="cpu 0"}
```

The endpoint decodes the cached snapshot into a minimal typed view (only the
fields the metrics use; the process array is counted, not materialized) — no
intermediate JSON tree — and the sampler serializes each tick into a recycled
buffer instead of allocating a fresh payload, so `/metrics` parses on demand
and retains nothing, and costs nothing while idle. While the server is sleeping
(no viewers) it answers `503` "warming up" and wakes the sampler, exactly like
a new dashboard connection; a Prometheus scrape interval therefore keeps a
machine being scraped sampling.

### HTTP keep-alive

Responses use HTTP/1.1 keep-alive so a dashboard's polls ride a single
connection (pipelined requests included — leftover bytes from a socket read are
kept for the next request). The server applies a 5-second per-read idle cutoff
so quiet connections release their thread, and error pages (`400`/`401`/`404`/
`405`/too-many-connections warm-up `503`s) always close. Verified by hand with a
raw pipelined `GET /health` + `GET /metrics` over one TCP connection.

### Linux / 24×7 operation

The serve path is tuned to run continuously on Linux servers:

- **No per-tick static reads.** Host facts that cannot change while the sampler
  is up — hostname, OS/distro name, OS version, kernel version, arch, CPU
  model, physical-core count, and the uid→name map — are captured once per
  sampling session. sysinfo's Linux getters would otherwise re-parse
  `/etc/passwd` and `/etc/os-release`, open `/proc` files and call
  `gethostname` on *every* poll. Only genuinely live values (uptime, load
  average, CPU, memory, disks, network, sensors) are re-read each tick.
- **No per-process `/proc/{pid}/io` reads.** The dashboard shows disk I/O per
  *disk*, never per process, so per-process disk usage is deliberately not
  sampled on the serve path — that saves an extra `/proc/{pid}/io` open per
  process per poll.
- **Live thread counts.** The process table's Threads column is refreshed
  every tick (the same lean refresh kind that warms the session up), so on
  Linux it always shows current counts instead of going stale one frame after
  connect.
- **Sensors reuse one component list.** Temperature probes are re-read on the
  same cadence as everything else, but into a single reused `Components`
  object — no fresh sysfs walk + label allocations per poll.
- **Pauses itself.** With no viewer for 30 s the sampler is dropped and the
  process returns to near-idle (≈4 MB private RSS, measured on a headless
  Debian 13 server — idle figures on other platforms are in the same low-MB
  range); command lines/environments are never held, so active-session
  overhead stays at or under ~1 MB regardless of how long it runs (≈0.2 MB on
  the Debian test box). On a headless server use
  `--interval 3` to cut per-second `/proc` walks by two thirds.

```bash
# on the headless server
sysview serve --bind 0.0.0.0 --port 8080 --kiosk

# on the wall display / laptop
open http://server-ip:8080/?kiosk=1
```

The server samples at the global `--interval` (default 1s); the on-page
selector controls how often a given device fetches. Use `--interval 3` for a
slower, lighter load on big servers. Sampling happens once per interval on the
server regardless of how many devices are connected, and pauses entirely
(released to near-zero memory) when no dashboard has connected for 30 seconds —
it resumes automatically on the next connect, with the history window intact.
Each snapshot now also carries the per-core and per-disk rings, so a full
payload is around 60–85 KB on an average 16-core host — still fine over LAN
or Wi-Fi at 1–2 s refresh.

> **Security — two ways to expose the dashboard.** The server binds to
> **loopback only** by default, so by itself it is not reachable from the
> network:
>
> | Access model | How | Remote reach |
> |---|---|---|
> | **Loopback + SSH tunnel** (default) | `sysview serve` (binds `127.0.0.1`); on the laptop keep `ssh -N -L 8080:127.0.0.1:8080 user@server` running | Only inside the encrypted tunnel — no port is open on the server at all. Best when you are the only viewer. |
> | **All interfaces + firewall + token** | `sysview serve --bind 0.0.0.0 --token <secret>`; allow the chosen port through a host firewall (`ufw allow 8080/tcp`). On startup the daemon prints the ready-to-open LAN URL with your token, e.g. `http://192.168.100.114:8080/?token=<secret>` | Anyone allowed by the firewall can reach the port, but only with the token via `?token=<secret>` or an `Authorization: Bearer` header. Use when wall displays / several machines need direct access. |
>
> The `--token` gate applies to **every** endpoint (`/`, `/api/snapshot`,
> `/metrics`, `/health`, favicon included) with a constant-time compare, and
> applies equally in both models. Without a token on a non-loopback bind,
> anyone who can reach the port can read live system stats — process names,
> owners, CPU/RAM/disk usage, network traffic — so loopback is the safe
> default and `0.0.0.0` is the explicit, documented choice.

## Debian / headless server setup (SSH)

Prebuilt binaries are published on the [Releases] page; the Linux build is
fully static (musl — no glibc dependency, runs on any distro), so that exact
file is what you deploy on a server. The repo also ships two scripts: build
(`scripts/build-linux.sh`) and install (`scripts/setup-debian.sh`). A complete
headless install over SSH is three commands:

```bash
# from your workstation (download sysview-linux-x86_64-musl + setup-debian.sh first)
scp sysview-linux-x86_64-musl scripts/setup-debian.sh root@server:/tmp/
ssh root@server 'bash /tmp/setup-debian.sh /tmp/sysview-linux-x86_64-musl'
```

The installer puts `sysview` in `/usr/local/bin`, generates an access token
(persisted in `/etc/sysview/env`), and enables a hardened systemd service with
restart-on-failure. Like the CLI, it defaults to loopback (`BIND=127.0.0.1`),
so the printed URL is local: reach it over an SSH tunnel, or rerun with
`BIND=0.0.0.0` to expose it on the LAN (the token is always required).

### SSH tunnel instead of an open port

Loopback binding is the installer's default; to keep the port closed to the
network entirely, confirm that or pass `BIND=127.0.0.1` explicitly, then
forward it:

```bash
BIND=127.0.0.1 bash /tmp/setup-debian.sh /tmp/sysview-linux-x86_64-musl
# on your workstation, keep this running:
ssh -N -L 8080:127.0.0.1:8080 user@server
# then open http://localhost:8080/?token=<secret>
```

### Building on the server

```bash
sudo apt install -y git curl build-essential
curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh -s -- -y
source "$HOME/.cargo/env"
git clone <this-repo> && cd $(basename <this-repo>)
sudo bash scripts/build-linux.sh --static
sudo bash scripts/setup-debian.sh dist/sysview-linux-x86_64-musl
```

`--static` links musl for a self-contained binary (default builds glibc).
Either needs a recent toolchain: the locked `sysinfo` requires rustc ≥ 1.95,
which Debian's stock compiler is behind (bookworm ships 1.63, trixie 1.85) —
use rustup as shown.

Rust is **build-time only**. Once `setup-debian.sh` has installed and started
the service from the built binary, neither the service nor reboots ever touch
Rust again — `rustup self uninstall -y` (and, if you like, removing the distro
`rustc`/`cargo` packages afterwards) is safe; the dashboard keeps running from
`/usr/local/bin/sysview`. This is the intended lean-server flow: install Rust,
compile, hand the binary to the service, uninstall Rust.

### Manual background run (no systemd)

`serve` needs no terminal (it never reads stdin), so a plain SSH background
job works — but detach it or it dies on logout:

```bash
nohup sysview serve --bind 0.0.0.0 --port 8080 --token "$TOKEN" \
  </dev/null >>/var/log/sysview.log 2>&1 &
disown
pgrep -a sysview      # verify
tail -f /var/log/sysview.log
```

The systemd unit is still preferable on a server: it survives reboots, restarts
on failure and logs to the journal for free.

### Installer overrides

Env vars read by `setup-debian.sh`:

| Var | Default | Meaning |
|---|---|---|
| `BIND` | `127.0.0.1` | listen address (`0.0.0.0` = expose on the LAN; keep the token + firewall) |
| `PORT` | `8080` | listen port (keep ≥ 1024; the service runs unprivileged) |
| `INTERVAL` | `1` | server sampling seconds (`3` = lighter 24/7 load) |
| `MAX_PROCS` | `100` | dashboard process rows |
| `TOKEN` | auto | access token; always present, persists across reinstalls |
| `KIOSK` | `0` | `1` = start every session in kiosk mode |

### Day-2 operations

```bash
systemctl status sysview      # runtime state
journalctl -u sysview -f      # live logs (startup banner prints the URL)
systemctl restart sysview
sudo bash /tmp/setup-debian.sh --uninstall   # stop + remove everything
```

The unit runs as a throwaway unprivileged user (`DynamicUser`), with
`NoNewPrivileges`, `ProtectSystem=strict`, `ProtectHome`, `PrivateTmp`,
`ProtectKernel{Tunables,Modules,Logs}`, `ProtectControlGroups`,
`ProtectHostname`, `RestrictSUIDSGID`, `LockPersonality`,
`RestrictNamespaces`, `RestrictRealtime` and `Restart=on-failure`
(`systemd-analyze security sysview` will confirm). Two hardening keys are
deliberately **not** set (see the comments in `scripts/setup-debian.sh`):
`ProcSubset=pid` would hide `/proc/meminfo`, `/proc/stat`, `/proc/net/dev`
and `/proc/diskstats` from the sampler, which they are needed for, and
`MemoryDenyWriteExecute` could not be demonstrated compatible on a real host
in this project's testing, so it is left for operators to enable if they can
verify it. On the non-loopback model, allow the chosen port through a host
firewall: `ufw allow 8080/tcp`. The static release's SHA-256 is
`4dd11c9dabbd56901ff3ce1c850e2c6e8428a501f93c98b7ec7c4865f32b4c`.

## Options

| Flag | Meaning |
|---|---|
| `-w, --watch` | Continuous refresh mode (clears screen each tick) |
| `--interval <secs>` | Refresh interval for watch mode / dashboard (default 1.0, min 0.1) |
| `--sort <key>` | Process sort: `cpu` (default), `mem`, `pid`, `name`, `uptime`, `starttime` |
| `-r, --reverse` | Reverse the sort order |
| `--filter-name <text>` | Only processes whose name contains `<text>` (case-insensitive) |
| `--filter-user <name>` | Only processes owned by that user |
| `--top <n>` | Number of processes to show (default 10) |
| `--json` | Machine-readable output |
| `--no-header` | Omit `== Section ==` header lines |
| `--units <mode>` | Byte units: `iec` (KiB/MiB, default), `si` (kB/MB), `bytes` |
| `--color <mode>` | `auto` (default; TTY + no `NO_COLOR`), `always`, `never` |

### `serve` subcommand options

| Flag | Meaning |
|---|---|
| `--port <n>` | HTTP port (default 8080) |
| `--bind <addr>` | Address to bind (default `127.0.0.1` loopback; use `0.0.0.0` for remote) |
| `--max-procs <n>` | Max processes sent to the dashboard (default 100) |
| `--token <secret>` | Require `?token=<secret>` or `Authorization: Bearer` |
| `--kiosk` | Start in kiosk mode (fullscreen, minimal chrome) |

## Notes

- CPU%/process CPU% is two-sample diff sampling for accuracy.
- Load average is reported on Linux/macOS; omitted on Windows.
- Process owner, cmdline, and thread counts are populated where the OS
  provides them (Linux/macOS best; Windows shows `-` for owner and `null`
  for thread count in JSON).
- `--sort mem` sorts high→low; add `-r` for low→high. Other keys sort
  low→high and `-r` inverts.

## Development

The dashboard is a single embedded HTML file — `web/dashboard.html` is compiled
into the binary with `include_str!` (no separate build step, no node). The
client is deliberately ES5 (no framework, no CSS Grid/flexbox) so it renders on
old browsers. After editing it, rebuild the release binary before deploying:

```bash
# Full quality gate (fmt, check, test, clippy native + Linux, release build).
# Installs the rustfmt component / Linux target on demand when cargo came from
# rustup, so it passes on a fresh toolchain:
bash scripts/check-gate.sh

# ...or run the steps directly:
cargo fmt -- --check              # rustfmt gate (needs: rustup component add rustfmt)
cargo test                        # unit + integration tests (32: sampling, HTTP, auth,
                                  #   lifecycle/idle gating, limits/timeouts, ring, /metrics)
cargo clippy -- -D warnings       # zero-warning lint gate
cargo check --target x86_64-unknown-linux-gnu   # compile-check the Linux branch on any host
cargo clippy --target x86_64-unknown-linux-gnu -- -D warnings   # lint the Linux branch too
cargo build --release             # production binary (single file)
bash scripts/build-linux.sh --static            # fully static musl release (see README, Debian section)
```

> Publishing a release: push a tag and the [release workflow](.github/workflows/release.yml)
> builds and attaches the precompiled binaries (Windows, Linux musl-static, macOS
> universal) plus checksums to the GitHub Release:

```bash
git tag v0.1.1 && git push origin v0.1.1
```

Run the dashboard and verify it live on the default port (`http://127.0.0.1:8080/`),
or use `--port` for an ad-hoc check. The startup banner prints the URL, sample
interval and any token — useful in the systemd journal.