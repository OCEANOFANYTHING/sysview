# Changelog

All notable changes to **sysview** are documented here, newest first.

The format follows [Keep a Changelog](https://keepachangelog.com/en/1.1.0/).
Releases are tagged `v0.1.0` … `v0.1.4` with prebuilt binaries on the
[GitHub Releases](https://github.com/OCEANOFANYTHING/sysview/releases) page.

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