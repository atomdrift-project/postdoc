#!/bin/sh
# Install postdoc as a systemd service — `worker` or `serve`.
#
#   scripts/deploy.sh worker https://hopper.example
#   scripts/deploy.sh serve  [https://hopper.example]
#
# The binary owns its defaults. This script passes the two things it cannot
# know — where hopper is, and where the bearer token lives — and nothing else.
# Every other setting has a default in postdoc, and a default stated in two
# places is a default that will eventually disagree with itself.
#
# What this file *does* own is the unit: the resource limits, the OOM policy
# and the sandbox. Those are properties of running on a host, not of the
# program, and they carry measurements rather than preferences.
#
# Environment (all optional):
#   MEMORY_MAX   cgroup hard ceiling            (default 80%)
#   MEMORY_LOW   reclaim-protected floor        (default 50%)
#   NICE         scheduling priority            (default -20)
#   LLM          SCAN_LLM: `local`, `openrouter`, or a base URL
#   LLM_MODEL    SCAN_LLM_MODEL; unset lets postdoc pick
#   TOKEN_FILE   hopper bearer token to install (default ~/.tok/hopper)

set -eu

MODE="${1:-}"
case "$MODE" in
    worker)
        URL="${2:-}"
        [ -n "$URL" ] || { echo "error: worker needs a hopper URL" >&2; exit 1; }
        SERVICE_NAME=postdoc-worker
        ;;
    serve)
        URL="${2:-}"
        SERVICE_NAME=postdoc
        ;;
    *)
        echo "usage: $0 worker <hopper-url> | serve [hopper-url]" >&2
        exit 1
        ;;
esac

SERVICE_USER=postdoc
BINARY=postdoc
BIN_PATH=/usr/local/bin/${BINARY}
STATE_HOME=/var/lib/atomdrift/postdoc
UNIT_FILE=/etc/systemd/system/${SERVICE_NAME}.service

MEMORY_MAX="${MEMORY_MAX:-80%}"
MEMORY_LOW="${MEMORY_LOW:-50%}"
NICE="${NICE:--20}"
LLM="${LLM:-}"
LLM_MODEL="${LLM_MODEL:-}"
TOKEN_FILE="${TOKEN_FILE:-$HOME/.tok/hopper}"

die() { echo "error: $*" >&2; exit 1; }
log() { printf '==> %s\n' "$*"; }

command -v systemctl >/dev/null 2>&1 || die "systemd not found"
[ -x "$BIN_PATH" ] || die "$BIN_PATH is missing or not executable"

SUDO=""
[ "$(id -u)" -eq 0 ] || SUDO=sudo

# --- Service account and state ----------------------------------------------

if ! getent passwd "$SERVICE_USER" >/dev/null; then
    log "Creating service user '$SERVICE_USER'"
    $SUDO useradd --system --home-dir "$STATE_HOME" --shell /usr/sbin/nologin \
        --comment "Atomdrift postdoc" "$SERVICE_USER"
fi
$SUDO install -d -m0750 -o "$SERVICE_USER" -g "$SERVICE_USER" "$STATE_HOME"
$SUDO install -d -m0700 -o "$SERVICE_USER" -g "$SERVICE_USER" "$STATE_HOME/.tok"

# Hopper rejects an unauthenticated claim with 401, so a worker without this
# starts, looks healthy, and never gets a job.
if [ -r "$TOKEN_FILE" ]; then
    log "Installing hopper token from $TOKEN_FILE"
    $SUDO install -m0600 -o "$SERVICE_USER" -g "$SERVICE_USER" \
        "$TOKEN_FILE" "$STATE_HOME/.tok/hopper"
elif [ "$MODE" = worker ]; then
    die "no hopper token at $TOKEN_FILE; a worker cannot claim without one"
fi

# --- Arguments ---------------------------------------------------------------
#
# The whole list. Anything absent here is postdoc's own default, which is the
# point: one place decides, and `postdoc <mode> --help` states it.
if [ "$MODE" = worker ]; then
    EXEC_ARGS="worker --url $URL"
else
    EXEC_ARGS="serve --token-file $STATE_HOME/.tok/hopper"
    [ -n "$URL" ] && EXEC_ARGS="$EXEC_ARGS --hopper $URL"
fi

# ProtectControlGroups=true blocks the cgroup delegation `serve` needs to
# freeze its companion worker, and systemd < 252 has no private variant.
PROTECT_CGROUPS=true
if [ "$MODE" = serve ]; then
    SYSTEMD_MAJOR="$(systemctl --version 2>/dev/null | head -1 | awk '{print $2}' | tr -cd '0-9')"
    if [ "${SYSTEMD_MAJOR:-0}" -ge 252 ]; then
        PROTECT_CGROUPS=private
    else
        PROTECT_CGROUPS=false
    fi
fi

LLM_MODEL_LINE=""
[ -n "$LLM_MODEL" ] && LLM_MODEL_LINE="Environment=SCAN_LLM_MODEL=${LLM_MODEL}"

# --- Unit --------------------------------------------------------------------

TMP_UNIT="$(mktemp)"
trap 'rm -f "$TMP_UNIT"' EXIT

cat > "$TMP_UNIT" <<EOF
[Unit]
Description=Atomdrift postdoc (${MODE})
After=network-online.target
Wants=network-online.target

[Service]
Type=simple
User=${SERVICE_USER}
Group=${SERVICE_USER}

ReadWritePaths=${STATE_HOME}
WorkingDirectory=${STATE_HOME}
ExecStart=${BIN_PATH} ${EXEC_ARGS}
Restart=always
RestartSec=10s
TimeoutStopSec=30s

Environment=HOME=${STATE_HOME}
Environment=SCAN_LLM=${LLM}
${LLM_MODEL_LINE}

Delegate=yes

# The hard backstop. postdoc throttles itself first, well below this, so
# reaching it means something is wrong rather than merely busy — and a restart
# is the right answer to that.
MemoryMax=${MEMORY_MAX}
MemoryLow=${MEMORY_LOW}
TasksMax=8192

# Analysis is what this host is for. Shed a shell before shedding the service,
# and never let the kernel pick it first.
OOMScoreAdjust=-900
ManagedOOMPreference=avoid
OOMPolicy=continue

# Applied before the drop to ${SERVICE_USER}, so no CAP_SYS_NICE is needed in
# the (empty) bounding set, and analysis children inherit it. Not realtime: a
# CPU-bound analysis at realtime priority can starve sshd and lock the box out.
Nice=${NICE}
CPUWeight=10000
StartupCPUWeight=10000
IOWeight=10000
StartupIOWeight=10000
IOSchedulingClass=best-effort
IOSchedulingPriority=0

ProtectSystem=strict
ProtectHome=true
PrivateTmp=true
PrivateDevices=true
PrivateMounts=true
ProtectKernelTunables=true
ProtectKernelModules=true
ProtectKernelLogs=true
ProtectControlGroups=${PROTECT_CGROUPS}
ProtectClock=true
ProtectHostname=true
ProtectProc=invisible
UMask=0077

NoNewPrivileges=true
RestrictSUIDSGID=true
RestrictRealtime=true
RestrictNamespaces=true
LockPersonality=true
SystemCallArchitectures=native
SystemCallFilter=@system-service
CapabilityBoundingSet=
AmbientCapabilities=
RestrictAddressFamilies=AF_UNIX AF_INET AF_INET6

StandardOutput=journal
StandardError=journal

[Install]
WantedBy=multi-user.target
EOF

log "Installing ${UNIT_FILE}"
$SUDO install -m0644 "$TMP_UNIT" "$UNIT_FILE"
$SUDO systemctl daemon-reload
$SUDO systemctl enable "$SERVICE_NAME"
$SUDO systemctl restart "$SERVICE_NAME"

# A unit that starts and then dies still reports "started", so wait and look.
sleep 3
if $SUDO systemctl is-active --quiet "$SERVICE_NAME"; then
    log "${SERVICE_NAME} is running"
else
    $SUDO journalctl -u "$SERVICE_NAME" -n 40 --no-pager || true
    die "${SERVICE_NAME} failed to start"
fi
