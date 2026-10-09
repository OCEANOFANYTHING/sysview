# sysview — architecture & internals

Design notes for maintainers and integrators. For usage, features and
deployment see the [README](../README.md).

## Overview

A single portable binary (Windows / Linux / macOS) with no runtime
dependencies. It has two front-ends:

- a **terminal CLI** — one-shot sections (`cpu`, `mem`, `procs`, …) and
  `--watch` refresh, rendered by `render.rs`;
- an **embedded HTTP dashboard** — `serve.rs` hosts one ES5 page
  (`web/dashboard.html`) that is compiled into the binary via `include_str!`
  (no build step, no node).

## Source map

| File | Responsibility |
|---|---|
| `src/main.rs` | clap CLI + section dispatch; shared globals (`--interval`, `--sort`, …) |
| `src/collect.rs` | sampling: `WebState`, lean warmup, `HostFacts` cache, snapshots |
| `src/model.rs` | all serialized types (one-shot + dashboard) |
| `src/render.rs` | terminal output (human + JSON) |
| `src/serve.rs` | HTTP server, sampler thread, history ring, `/metrics` |
| `web/dashboard.html` | embedded ES5 dashboard (embedded via `include_str!`) |
| `scripts/`, `dist/` | Debian/systemd deployment (see README) |

## Thread model (`serve`)

Three cooperating parts:

1. **Listener thread** — accept loop; spawns one thread per connection
   (capped at `MAX_CONNS = 64`; excess connections get `503`).
2. **Sampler thread** — the *sole owner* of the sysinfo state (`WebState`).
3. **Per-connection threads** — parse the request, respond from the shared
   cached payload.

## Sampling model (`src/collect.rs`)

CPU and per-process usage are **diffs between refreshes of the same sysinfo
instance**, so one `System` must live across ticks: that's `WebState`.

- **Lean warmup (`WebState::new`)** — two-sample CPU/process pass (they fire
  ~200 ms apart, above sysinfo's `MINIMUM_CPU_UPDATE_INTERVAL`) using a lean
  refresh kind: *cpu, memory, user, tasks*. It deliberately never fetches
  cmdline/exe/cwd/environ and never per-process disk I/O — the dashboard
  neither renders per-process strings/I/O, and on Linux those cost a
  `/proc/{pid}/stat`-based walk plus a `/proc/{pid}/io` read per process per
  tick.
- **`HostFacts`** — hostname, OS name/version, kernel, arch, CPU model,
  physical-core count and the uid→user map are captured **once per session**.
  sysinfo's Linux getters re-parse `/etc/os-release`, `/etc/passwd` and call
  `gethostname` on *every* invocation; re-reading them per tick was the single
  biggest avoidable 24/7 cost.
- **One `Components` object** is refreshed in place each tick — temperatures
  stay live without a fresh sysfs walk + label allocation per poll.
- **`snapshot()`** refreshes CPU, processes (same lean kind, incl. `tasks` →
  live thread counts on Linux), memory, networks, disks and sensors, then
  builds the lean JSON model via `web_snapshot`. Uptime and load average stay
  live (read per tick — they are genuinely time-varying).
- The **one-shot CLI paths** (`sample_cpu_and_processes`) instead use
  `ProcessRefreshKind::everything()`, because they render command lines.

## Idle lifecycle (`src/serve.rs`)

The sampler loop ticks at the global `--interval` (default 1 s) and samples
only while a connection is open or one occurred within `IDLE_GRACE` (30 s).
When nobody is watching:

- `WebState` is **dropped** and the cached payload **cleared** — the process
  drops to ≈2–4 MB private RSS regardless of uptime (measured on Debian 13);
- the **history ring (a few KB) is retained**, so the next viewer resumes the
  exact same 240-point window — charts never "start over".

Cold requests while asleep answer `503 {"error":"warming up"}` and wake the
sampler (`mark_watching`). `/health` deliberately does **not** re-arm idle, so
scripted liveness probes don't keep the machine sampling.

**History ring** — one capped `VecDeque` per metric (`cpu/mem/swap/rx/tx`),
a second set per logical core, and a third set per mounted volume (capped at
`MAX_DISK_MOUNTS = 64`). The same ring feeds every client, so all screens
show identical charts and every fetcher gets an identical payload.

## Serialization & memory posture

Per tick the snapshot is serialized **into the previous tick's buffer**
(`clear()` preserves capacity): exactly one payload-sized allocation is ever
retained and per-tick heap allocations are zero.

- `/` — the dashboard HTML (poll interval + kiosk injected at startup).
- `/api/snapshot` — the cached JSON bytes, served **under the lock** (no
  60–85 KB clone per poll).
- `/metrics` — Prometheus text 0.0.4: decodes the *cached bytes* into a
  minimal typed view; the process array is **counted, never materialized**;
  no intermediate JSON tree, retains nothing, costs nothing while idle.

Payload size is roughly 60–85 KB on a 16-core host (includes the per-core and
per-disk rings); the process list is capped by `--max-procs` (default 100).

## HTTP & auth

- Token via `?token=` query **or** `Authorization: Bearer`, compared
  constant-time; failures get `401` and the connection closes.
- HTTP/1.1 keep-alive with pipelining; a **5 s per-read timeout** releases
  quiet connection threads; error pages (`400/401/404/405/503`) always close.
- `MAX_CONNS = 64` → `503` "too many connections".
- Headless-friendly: `serve` never reads stdin and needs no TTY; the startup
  banner (URL, auth hint) prints to stdout → lands in the systemd journal.
  When bound beyond loopback it also prints the ready-to-open LAN URL with the
  token (`http://<lan-ip>:<port>/?token=…`).

### Endpoints

| Path | Auth | Idle re-arm | Response |
|---|---|---|---|
| `/` , `/index.html` | yes | yes | dashboard HTML |
| `/api/snapshot` | yes | yes | JSON snapshot, or `503 {"error":"warming up"}` |
| `/metrics` | yes | yes | Prometheus text 0.0.4, or `503 warming up` |
| `/health` | yes | **no** | `{"ok":true}` |
| `/favicon.ico` | yes | no | `204 No Content` |
| anything else | yes | — | `404` |

## Platform notes

- **Load average** — reported on Linux/macOS; `null` on Windows (header falls
  back to uptime only).
- **Owner names** — Linux/macOS populated; Windows may be `None`.
- **Thread counts** — live on Linux (refreshed via `with_tasks()` every tick);
  Windows never populates tasks and shows `-`.
- **Disk I/O** — rendered per *disk*, never per process (see lean warmup).
- **Deployment** — prebuilt Linux is a fully static musl binary
  (`sysview-linux-x86_64-musl` on GitHub Releases; no glibc; runs on any
  distro). `dist/` only holds local `scripts/build-linux.sh` output. See the
  README section *Debian / headless server setup (SSH)* for the systemd
  installer, which hardens and *enables at boot* the service (`Restart=always`,
  256 MB memory ceiling, boot autostart).