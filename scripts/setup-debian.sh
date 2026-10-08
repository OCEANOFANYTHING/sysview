#!/usr/bin/env bash
# Install sysview as a hardened systemd service on a headless Debian server.
#
#   sudo bash scripts/setup-debian.sh [binary]       install / update
#   sudo bash scripts/setup-debian.sh --uninstall    remove everything
#
# `binary` is optional: defaults to /tmp/sysview-linux-x86_64, or an already
# installed /usr/local/bin/sysview (e.g. after scripts/build-linux.sh --install).
#
# Env overrides (all optional):
#   BIND=127.0.0.1   address to bind (default 127.0.0.1 = loopback only, i.e.
#                    reachable only over an SSH tunnel; set 0.0.0.0 to expose
#                    on the network, which additionally needs a firewall rule
#                    and the access token below)
#   PORT=8080         listen port
#   INTERVAL=1        server sampling interval in seconds (3 = lighter load)
#   MAX_PROCS=100     processes shown in the dashboard table
#   TOKEN=<secret>    access token; auto-generated and stored in
#                     /etc/sysview/env when unset
#   KIOSK=1           start every session in kiosk mode
set -euo pipefail

SVC=sysview
BIN_DIR=/usr/local/bin
CFG_DIR=/etc/sysview
UNIT=/etc/systemd/system/sysview.service

if [ "${1:-}" = "--uninstall" ] || [ "${1:-}" = "-u" ]; then
    systemctl disable --now "$SVC" >/dev/null 2>&1 || true
    rm -f "$UNIT"
    rm -rf "$CFG_DIR"
    rm -f "$BIN_DIR/$SVC"
    systemctl daemon-reload
    echo "sysview removed (binary, systemd unit and $CFG_DIR deleted)."
    exit 0
fi

if [ "$(id -u)" -ne 0 ]; then
    echo "error: run as root:  sudo bash $0 ..." >&2
    exit 1
fi

BIN_SRC="${1:-}"
if [ -z "$BIN_SRC" ]; then
    # No argument: prefer the prebuilt in /tmp, fall back to an already
    # installed copy (e.g. after scripts/build-linux.sh --install).
    if [ -f /tmp/sysview-linux-x86_64 ]; then
        BIN_SRC=/tmp/sysview-linux-x86_64
    elif [ -f "$BIN_DIR/$SVC" ]; then
        BIN_SRC="$BIN_DIR/$SVC"
    fi
fi
# Convenience: a bare filename also resolves from ./dist.
if [ -n "$BIN_SRC" ] && [ ! -f "$BIN_SRC" ] && [ -f "dist/$BIN_SRC" ]; then
    BIN_SRC="dist/$BIN_SRC"
fi
if [ -z "$BIN_SRC" ] || [ ! -f "$BIN_SRC" ]; then
    echo "error: sysview binary not found (pass it: bash $0 /path/to/sysview)." >&2
    echo "  build one (scripts/build-linux.sh --static) or scp it over first." >&2
    exit 1
fi

chmod 0755 "$BIN_SRC"
echo ">> installing $BIN_SRC -> $BIN_DIR/$SVC"
install -m 0755 "$BIN_SRC" "$BIN_DIR/$SVC"
"$BIN_DIR/$SVC" --version

mkdir -p "$CFG_DIR"
chmod 0755 "$CFG_DIR"

# Token: reuse the existing one on reinstall, otherwise generate a fresh secret.
TOKEN="${TOKEN:-}"
if [ -z "$TOKEN" ] && [ -f "$CFG_DIR/env" ] && grep -q '^SYSVIEW_TOKEN=' "$CFG_DIR/env" 2>/dev/null; then
    TOKEN=$(sed -n 's/^SYSVIEW_TOKEN=//p' "$CFG_DIR/env" | head -n 1)
fi
if [ -z "$TOKEN" ]; then
    if command -v openssl >/dev/null 2>&1; then
        TOKEN=$(openssl rand -hex 16)
    else
        TOKEN=$(LC_ALL=C tr -dc 'a-f0-9' </dev/urandom | head -c 32)
    fi
fi

umask 077
cat > "$CFG_DIR/env" <<EOF
SYSVIEW_TOKEN=$TOKEN
EOF
chmod 0600 "$CFG_DIR/env"

BIND="${BIND:-127.0.0.1}"
PORT="${PORT:-8080}"
INTERVAL="${INTERVAL:-1}"
MAX_PROCS="${MAX_PROCS:-100}"
KIOSK_ARGS=""
if [ "${KIOSK:-0}" = "1" ]; then
    KIOSK_ARGS=" --kiosk"
fi

cat > "$UNIT" <<EOF
[Unit]
Description=sysview system dashboard
After=network-online.target
Wants=network-online.target

[Service]
Type=simple
ExecStart=$BIN_DIR/$SVC --interval ${INTERVAL} serve --bind ${BIND} --port ${PORT} --max-procs ${MAX_PROCS} --token \${SYSVIEW_TOKEN}${KIOSK_ARGS}
EnvironmentFile=$CFG_DIR/env
Restart=on-failure
RestartSec=3
# The daemon only reads /proc, /sys, /etc and binds a high port, so it runs
# as a throwaway unprivileged user instead of root. (Keep PORT >= 1024.)
DynamicUser=yes
NoNewPrivileges=true
ProtectSystem=strict
ProtectHome=true
PrivateTmp=true
# Kernel/hostname/namespace hardening: nothing sysview reads is affected.
ProtectKernelTunables=true
ProtectKernelModules=true
ProtectKernelLogs=true
ProtectControlGroups=true
ProtectHostname=true
RestrictSUIDSGID=true
LockPersonality=true
RestrictNamespaces=true
RestrictRealtime=true
# Deliberately NOT set, because they would break sysview's own sampling:
#   ProcSubset=pid          exposes only per-PID entries in /proc, but the
#                           sampler also reads /proc/meminfo, /proc/stat,
#                           /proc/net/dev and /proc/diskstats
#   MemoryDenyWriteExecute  could not be demonstrated compatible on this
#                           host, so it is left out (comment in README)
# Optional memory ceiling for the daemon (it normally stays ~10-20 MB; the
# sampler is dropped to near-zero while no dashboard is connected):
# MemoryMax=256M

[Install]
WantedBy=multi-user.target
EOF

systemctl daemon-reload
systemctl enable --now "$SVC" >/dev/null 2>&1
sleep 1

if [ "$(systemctl is-active "$SVC" 2>/dev/null)" != "active" ]; then
    echo "error: service failed to start." >&2
    systemctl --no-pager status "$SVC" --lines=5 || true
    exit 1
fi

echo
echo "sysview is running:"
echo "  http://$BIND:$PORT/?token=$TOKEN"
if [ "$BIND" != "127.0.0.1" ]; then
    echo "  exposed on all interfaces; from another machine:"
    echo "    http://<this-host>:${PORT}/?token=$TOKEN"
    echo "  harden it first: the token is required, and if a host firewall is"
    echo "  enabled:  ufw allow ${PORT}/tcp"
else
    echo "  loopback only; reach it over an SSH tunnel:"
    echo "    ssh -N -L ${PORT}:127.0.0.1:${PORT} user@<this-host>"
    echo "    then open http://localhost:${PORT}/?token=$TOKEN"
    echo "  to expose it on the LAN instead, rerun with BIND=0.0.0.0"
    echo "  (then also allow the port through the firewall + keep the token)"
fi
echo
echo "manage:   systemctl status $SVC   restart   stop"
echo "logs:     journalctl -u $SVC -f"
echo "update:   scp a new binary, then rerun this script with its path"
echo "uninstall: sudo bash $0 --uninstall"