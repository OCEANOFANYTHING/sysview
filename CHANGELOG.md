# Changelog

All notable changes to **sysview** are documented here, newest first.

The format follows [Keep a Changelog](https://keepachangelog.com/en/1.1.0/).
Releases are tagged `v0.1.0` … `v0.3.1` with prebuilt binaries on the
[GitHub Releases](https://github.com/OCEANOFANYTHING/sysview/releases) page.

## [v0.3.1] - 2026-10-16

### Fixed

- **`serve --qr` no longer prints blank rows into a file or a log.** The symbol
  is drawn entirely with background colours, and those escapes are stripped from
  any output that cannot display them — a redirected banner therefore held about
  twenty-five empty lines. It now asks whether colour is really enabled, so a
  pipe, a dumb `TERM` or `NO_COLOR` gets one line pointing at the URL instead,
  and a real terminal still gets the code.
- **`sysview service install --system` now sets `MemoryMax=256M`**, the ceiling
  `scripts/setup-debian.sh` has always installed and the docs advertise. The
  per-user unit is unchanged.

## [v0.3.0] - 2026-10-16

### Added

- **`sysview top`** — a full-screen process monitor that stays open: `q` quits,
  `c`/`m`/`p`/`n` sort by CPU, memory, pid or name, `r` flips the direction and
  `+`/`-` grow or shrink the list. It needs no dependency (`stty` on Unix, the
  C runtime's `_kbhit`/`_getch` on Windows), restores the terminal on exit even
  after `Ctrl+C`, and when stdin is not a terminal it simply refreshes without
  keys.
- **`sysview service install|uninstall|status`** — registers the dashboard with
  whichever supervisor the host already has: a systemd unit (same hardening as
  `scripts/setup-debian.sh`), a launchd plist, or a Windows Task Scheduler task.
  The service gets its own token, kept where the platform keeps secrets rather
  than in the process table, and `--print` shows the unit and the commands
  without touching anything. `--system` installs machine-wide.
- **`sysview export`** — Prometheus text (`/metrics` format) or CSV for host,
  disks, processes or network, rendered by the *same* code the HTTP endpoint
  uses, so the CLI and the dashboard cannot disagree.
- **`sysview diff BEFORE [AFTER]`** — host-level comparison between two
  snapshots or against the live host: uptime (plus a reboot flag), CPU, memory,
  swap and every mount and interface, with mounts and interfaces that appeared
  or disappeared marked `(new)` / `(gone)`. Reads snapshots saved by `--json`
  even when PowerShell wrote them as UTF-16.
- **`sysview cgroup`** (Linux) — the cgroup v2 limits this process actually runs
  under: `cpu.max` quota, memory and PID counters, and the PSI stall pressure
  for cpu, memory and io. These are the numbers a host-wide view cannot show on
  a container, where the host's totals are not what the process can use.
- **`sysview gpu`** (Linux) — the DRM cards the kernel reports, with only what
  sysfs really has: amdgpu and Intel publish busy-percent and VRAM, the
  proprietary NVIDIA module publishes neither, so that card is listed without
  invented figures.
- **`serve --history-file FILE`** — appends one compact JSON line per sample and
  replays the tail on start, so the charts resume after a restart instead of
  blanking. The file is trimmed to the newest window once it has grown to four
  of them, so a long-running server cannot fill a disk.
- **`GET /api/history`** — the rolling window on its own plus the sample
  spacing, for tools that scrape on a schedule and do not want the whole
  snapshot.
- **`serve --qr`** — prints a scannable QR code for the dashboard URL (token
  included when one is set) after the banner, drawn with ANSI background
  colours so it scans from a dark or a light terminal.

### Changed

- Process order has a single source of truth (`collect::sort_procs`), so `top`
  and the one-shot sections cannot drift apart.

## [v0.2.4] - 2026-10-16

### Fixed

- **Platform-meaningless rows are gone from the Windows output.** `sysview sys`
  no longer prints a `Kernel` line that only repeats the build number already on
  the OS line, `sysview mem` drops `Free` when the OS reports the same value as
  `Available`, and `sysview vmstat` omits `Run/Block` — Windows has no run
  queue, so the row was really printing the process count. `vmstat --json`
  reports `null` for both counts.
- **`sysview check` can no longer pass silently.** A check that compares
  nothing — no threshold given, or one this platform cannot evaluate (such as
  `--load` on Windows) — now explains itself on stderr and exits `2`. Exit
  codes are `0` pass, `1` breach, `2` nothing evaluated.

### Changed

- Release notes are always the matching `CHANGELOG.md` section. The publish job
  now fails when a tag has no section instead of falling back to GitHub's
  auto-generated commit list; the back catalogue (`v0.1.0` … `v0.1.3`) was
  republished from the changelog.

## [v0.2.3] - 2026-10-15

### Changed

- **Leaner per-tick process sampling.** The dashboard's snapshot builder now
  ranks the raw process handles by CPU first and only turns the top slice into
  owned `name`/`state`/`user` strings, instead of building every process row and
  throwing all but the displayed ones away. On a busy host that removes the
  bulk of the per-second string allocations on the hottest loop. Output is
  identical — same processes, same order, same values.
- **Process owners resolved by uid, not by string.** The per-session
  uid → name map is now keyed by the id value itself, dropping one
  `to_string()` allocation per process per tick.
- Dependency refresh (`cc`, `syn`, `unicode-ident`) — build-time only, no
  runtime or behaviour change.

## [v0.2.2] - 2026-10-11

### Added

- **Threshold alerts with outbound webhooks.** `sysview serve` can watch CPU,
  memory, swap, load and any mount (`--alert-disk '*:90'`) and POST when a
  threshold is crossed, again while it stays crossed, and once on recovery.
  Bodies are chosen per host: ntfy raw text, Slack `{text}`, Discord `{content}`,
  anything else generic JSON. POSTs run on a short-lived detached thread, so a
  slow endpoint never stalls sampling. When alerts are configured the sampler
  keeps running with no viewer — the same cost as one open dashboard — because
  otherwise an overnight crossing would never fire.
- **Per-key scopes.** Keys are now `admin` or `view`. A `view` key can read the
  page, snapshot, metrics, health, fleet and interfaces but gets `403` on every
  state-changing route, so a wall display is safe to hand out. `serve --token`
  and keys added without a scope default to `admin`. Manage with
  `sysview key add --scope view` / `sysview key scope <id> --scope view`, or the
  dashboard's keys table.
- **Brute-force throttling.** Repeated bad tokens from one IP get `429` after a
  small budget; a successful auth clears the counter. No external rate limiter.
- **JSON `/health`.** Returns `{ok, status, version, uptime_secs, sampling}` for
  uptime monitors, and `sysview check --cpu 90 --mem 90 …` exits non-zero on a
  breach (with `--json` and `--quiet`) for cron and CI probes.
- **Built-in HTTPS.** `serve --tls-cert cert.pem --tls-key key.pem` terminates
  TLS itself (PKCS#8, PKCS#1 or SEC1 keys) reusing the rustls already linked for
  the updater — no reverse proxy and no new dependency.
- **Fleet page.** `/fleet` watches several sysview hosts at once: add them in the
  browser, it polls each `/api/snapshot` and shows cpu/mem/swap/disk, uptime and
  up/down. `serve --cors` (opt-in) lets a fleet page on another origin poll this
  host; tokens ride the query string so there is no preflight. A web app manifest
  lets the dashboard install as a standalone app.
- **Listening ports and battery.** `sysview ports` and `sysview battery` (also a
  dashboard panel) read Linux `/proc/net` and `/sys/class/power_supply`; both are
  bounded and allocation-light, so the idle footprint is unchanged.

## [v0.2.1] - 2026-10-11

### Changed

- **About half the size, and a leaner idle server.** The bundled HTTPS client
  that powers `sysview update` now uses rustls with the compact **`ring`**
  provider instead of the default `aws-lc-rs`, which statically linked a large
  slice of BoringSSL. The client is a tiny hand-rolled HTTP/1.1 reader
  (`Content-Length` and chunked bodies, redirects) over rustls + webpki-roots,
  so the `minreq` dependency is gone. Together with a size-optimised release
  profile (`opt-level = "z"`) this shrinks the fully static Linux binary from
  5.1 MB to 2.2 MB (Windows: 3.3 MB to 1.8 MB). `sysview update` behaves
  exactly as before — same GitHub lookup, checksum verification, atomic replace,
  `--check` and `--force`.
- **Idle memory roughly halved.** The background sampler now runs on a small
  capped stack, so on hosts with `transparent_hugepage=always` it no longer
  costs a full 2 MB huge page for an almost-empty thread stack. On a 4-core
  Debian server the idle resident set fell from 4.9 MB to 2.2 MB, and the
  binary's resident text shrank with the smaller executable. Sampling
  behaviour, history and idle gating are unchanged.

### Fixed

- **Website dashboard screenshots.** The two example shots were captured
  full-page and so appeared very long; they are now viewport-sized (800×822).
  The "modern" shot had also been rendering in the legacy theme (a saved
  browser preference) and now shows the real dark theme.

## [v0.2.0] - 2026-10-10

### Added

- **Self-update with no external tools.** `sysview update` (and an **update**
  button in the dashboard) fetches the latest release over HTTPS with a tiny
  bundled client (`minreq` + rustls), verifies it against the release's
  `SHA256SUMS`, and replaces the running binary in place. No `git`, `curl`, or
  package manager needed — it works on Windows, Linux and macOS. Your token and
  settings are never touched, so existing dashboard URLs keep working. Use
  `sysview update --check` to only look, `--force` to reinstall. On Windows the
  running `.exe` is renamed aside and the leftover is cleaned up on next start.
- **CPU temperature card & load gauge.** The dashboard gains a CPU-temperature
  card (it picks the CPU/package sensor by label, falling back to the hottest)
  with its own gauge and trend spark, and the Load card now shows a gauge of the
  1-minute load relative to the core count alongside the uptime.
- **Multiple access keys, with expiry.** Credentials can come from a keys file,
  the environment, or both; every key can carry a label and an expiry
  (`7d`, `30d`, `90d`, `6mo`, `1y`, `never`, or a `YYYY-MM-DD` date). Manage them
  from the CLI:

  ```
  sysview key add --label phone --expires 30d     # generates a token
  sysview key add --label tv --expires 1y --replace
  sysview key list
  sysview key rm phone
  sysview key expire tv --expires never
  ```

  or from the new **Settings** panel in the dashboard (add, delete, change
  expiry, rotate in place). Keys also come from `SYSVIEW_TOKEN` and
  `SYSVIEW_KEYS` (`label:token[:expires],…`) environment variables, which are
  read-only in the UI and never expire. Comparisons stay constant-time and the
  keys file is re-read only when it changes on disk.
- **`--interface <name>`.** Bind to a named interface (e.g. `wlan0`, `en0`,
  `Wi-Fi`) instead of a fixed address; `serve` resolves its current IPv4 at
  startup and tells you the available interfaces if the name is unknown.
- **Settings panel.** A gear button in the dashboard opens a live panel for the
  refresh interval, process cap and kiosk default (applied instantly), the
  network bind/interface/port (with a **restart** button), key management, and
  version/update. Everything persists across restarts.

### Changed

- Server settings from the panel are stored next to the keys file and merged at
  startup with the precedence **explicit CLI flag > saved setting > built-in
  default**, so the UI can never silently shadow an explicit flag.
- Live changes (interval, process cap, kiosk) need no restart; bind, interface
  and port mark the server as needing one, and the restart honours `systemd`
  when present and otherwise re-execs the binary.

## [v0.1.6] - 2026-10-09

### Added

- **Reboot-proof binds.** When a specific bind address is not assigned to any
  interface yet — the classic "systemd beat the Wi-Fi/DHCP to the punch" moment
  right after a reboot — `serve` now waits up to 30 seconds for the address to
  appear (retrying every 2 s, with a note printed to the journal) instead of
  exiting and crash-looping under `Restart=always`. Everything else (port in
  use, permission denied) still fails fast. A pinned bind like
  `--bind 192.168.100.114` now survives boot cleanly.

## [v0.1.5] - 2026-10-09

### Added

- **Brand assets.** The real sysview logo now ships everywhere. The site gets a
  32 px favicon, a 180 px apple-touch icon, the logo in the nav and a wide hero
  masthead — all small PNG derivatives of the master artwork — and the
  dashboard serves a real icon at `/favicon.ico` from a ~1.7 KB PNG embedded
  in the binary. Branding without a weight penalty.
- **Legacy theme showcase.** The site now shows a full demo-data screenshot of
  the retro/legacy dashboard right below the modern one, so visitors can see
  the theme before they download. Demo data again — no real host values on the
  page.

### Changed

- The dashboard HTML links `favicon.ico` and the server answers `200 OK` with
  the icon bytes instead of an empty `204 No Content`.

## [v0.1.4] - 2026-10-09

### Added

- **Kiosk escape button.** While the dashboard is in kiosk / full-screen mode,
  an **Exit** button now floats at the bottom-right corner. It is visible
  whenever the screen is awake (it fades together with the status bar after
  inactivity and reappears on any touch, key or mouse move), so kiosk displays
  can be exited with a tap or a click — no keyboard required. Pressing `Esc`
  leaves kiosk and full-screen as well.
- **Legacy retro theme.** The dashboard keeps the modern dark UI as the
  default, and a new **legacy** button in the header switches to a
  Windows-95 style retro theme (bevels, silver panels, navy title bar,
  MS Sans Serif). The button turns into **modern** to switch back. The choice
  is remembered per browser and can be forced with `?legacy=1` on the URL
  (handy for kiosk wall displays).
- **CHANGELOG.md** — this file. Every release now documents exactly what
  changed; the GitHub Release notes for each tag include the matching
  changelog section, and the project site lists all versions.

### Changed

- **Docs language.** Removed references tied to one specific deployment
  ("your workstation", "your token", a private LAN example address) in favor
  of neutral operator wording and `<lan-ip>`-style placeholders.
- **Website.** New **Releases** section lists every version and what changed
  in it; version references updated to v0.1.4.

### Technical notes

- The legacy theme is pure CSS (a single `html.legacy` class, no extra
  requests, no extra timers, no layout changes) and only uses old-browser-safe
  features — class selectors and outset/inset borders — so it adds no
  meaningful weight and runs on the same old tablets the dashboard already
  supports.

## [v0.1.3] - 2026-10-09

### Added

- Dashboard status bar credits **N&D Co — Brand & Growth Agency** with a single
  do-follow link to <https://ndcompany.in>; the same credit appears on the
  project site.
- Brand-palette restyle of the project site and a demo-data dashboard
  screenshot.

### Changed

- Docs: stale claims fixed, measured idle-memory figure (2–4 MB on a headless
  Debian 13 server) documented.
- The v0.1.3 musl build hash was pinned in the README at release time (it
  changes every release and is now deliberately not pinned).

## [v0.1.2] - 2026-10-09

### Added

- **Boot autostart** for the systemd installer: the unit is installed
  `enabled` (`WantedBy=multi-user.target`) with `Restart=always`, so the
  dashboard comes back on its own after a reboot or crash and survives
  `kill -9`.
- **Enforced memory ceiling**: the systemd service caps sysview at 256 MB.

### Changed

- Per-core CPU charts are now rendered at device-pixel resolution — the
  "bottom blue graphs" are no longer blurry.
- DPR-aware canvas sizing for every chart (crisp on HiDPI displays and wide
  windows instead of up-scaled).

## [v0.1.1] - 2026-10-09

### Added

- When serving on all interfaces (`--bind 0.0.0.0`), the startup banner prints
  the ready-to-open LAN URL with the access token —
  `http://<lan-ip>:<port>/?token=…`.
- Test coverage for the LAN-bind path.

### Changed

- Renovate dependency bumps (workflow actions).

## [v0.1.0] - 2026-10-09

### Added

- **Initial release** of sysview: a portable one-shot system stats viewer for
  Windows, Linux and macOS.
- Terminal CLI: full overview, per-section views (CPU, memory, disks, network,
  processes, vmstat, sensors), watch mode, JSON output, sorting/filtering.
- **Embedded web dashboard** in `sysview serve`: live gauges, charts and
  sparklines, per-core CPU bars, process table, health score, timeline
  markers — a single self-contained binary, no runtime.
- **Token auth** on every route (`/`, `/api/snapshot`, `/metrics`, `/health`,
  favicon included) with constant-time compare.
- **Prometheus `/metrics` endpoint** (text format 0.0.4) decoded from the
  cached snapshot with zero per-poll allocations.
- **Lean sampling model**: host facts cached once per session, no per-process
  I/O reads, sample drops after 30 s without viewers, shared 240-sample history
  preserved across idle.
- Hardened **Debian/systemd installer** (`setup-debian.sh`): unprivileged
  `DynamicUser`, `ProtectSystem=strict`, hardened service unit.
- Prebuilt binaries for Windows, Linux (fully static musl) and macOS
  (universal), published by the release workflow with `SHA256SUMS`.