# Changelog

All notable changes to **sysview** are documented here, newest first.

The format follows [Keep a Changelog](https://keepachangelog.com/en/1.1.0/).
Releases are tagged `v0.1.0` … `v0.2.2` with prebuilt binaries on the
[GitHub Releases](https://github.com/OCEANOFANYTHING/sysview/releases) page.

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