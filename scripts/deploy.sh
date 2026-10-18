#!/bin/sh
# Install postdoc as a systemd service — `worker` or `serve`.
#
#   scripts/deploy.sh worker https://hopper.example [build-binary]
#   scripts/deploy.sh serve  [https://hopper.example] [build-binary]
#
# Run as the deploying user; privileged steps prefer doas, then sudo.
#
# The binary owns its defaults. This script passes the hopper URL and installs
# the API and hopper credentials from their separate source files.
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
#   HOPPER_TOKEN_FILE  hopper bearer token (default ~/.tok/hopper; TOKEN_FILE is a legacy alias)
#   SCAN_TOKEN_FILE   API bearer token for Beamline (default ~/.tok/scan)
#   LLM_TOKEN_FILE    LLM endpoint bearer token (default ~/.tok/llm)

set -eu

MODE="${1:-}"
BUILD_BIN="${3:-target/release/postdoc}"
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
        echo "usage: $0 worker <hopper-url> [build-binary] | serve [hopper-url] [build-binary]" >&2
        exit 1
        ;;
esac

SERVICE_USER=postdoc
BINARY=postdoc
STATE_HOME=/var/lib/atomdrift/postdoc
BIN_DIR=${STATE_HOME}/bin
BIN_PATH=${BIN_DIR}/${BINARY}
UNIT_FILE=/etc/systemd/system/${SERVICE_NAME}.service

NICE="${NICE:--20}"
LLM="${LLM:-}"
LLM_MODEL="${LLM_MODEL:-}"
HOPPER_TOKEN_FILE="${HOPPER_TOKEN_FILE:-${TOKEN_FILE:-$HOME/.tok/hopper}}"
SCAN_TOKEN_FILE="${SCAN_TOKEN_FILE:-$HOME/.tok/scan}"
LLM_TOKEN_FILE="${LLM_TOKEN_FILE:-$HOME/.tok/llm}"

die() { echo "error: $*" >&2; exit 1; }
log() { printf '==> %s\n' "$*"; }

command -v systemctl >/dev/null 2>&1 || die "systemd not found"
[ -x "$BUILD_BIN" ] || die "$BUILD_BIN is missing or not executable"

SUDO=""
if [ "$(id -u)" -ne 0 ]; then
    if command -v doas >/dev/null 2>&1; then
        SUDO=doas
    elif command -v sudo >/dev/null 2>&1; then
        SUDO=sudo
    else
        die "need doas or sudo for privileged steps"
    fi
fi

ROOT_GROUP=root
CONFIG_DIR=/etc/postdoc
# Respect an existing account's home and resolve relocated state storage.
account_home=$(getent passwd "$SERVICE_USER" | cut -d: -f6 || true)
[ -z "$account_home" ] || STATE_HOME=$account_home
STATE_HOME=$($SUDO readlink -m -- "$STATE_HOME")
BIN_PATH=$STATE_HOME/bin/$BINARY
. "$(dirname "$0")/deploy-common.sh"
prepare_credentials
ensure_runtime_tools Linux
command -v python3 >/dev/null 2>&1 || die "python3 is required (run make deploy-server to bootstrap build prerequisites)"
# Resolve byte limits once, and persist explicit overrides separately below.
memory_settings=$(python3 "$DEPLOY_DIR/deploy-config.py" memory "$MODE") || die "invalid memory policy"
eval "$memory_settings"
prepare_arguments
EXEC_ARGS=$(printf '%s' "$EXEC_ARGS" | sed 's/%/%%/g')
UNIT_ENV=$(python3 "$DEPLOY_DIR/deploy-config.py" unit-env "$STATE_HOME" "$SERVICE_PATH" "$IDLE_MEMORY_MAX")

# --- Retire atomscan ---------------------------------------------------------
#
# postdoc replaces `atomscan worker` and `atomscan serve`, and the two must not
# run at the same time. Each sizes its memory ceiling against the whole host and
# each runs at nice -20, so a box running both carries two analysis daemons that
# each believe they own it: they claim from the same queue and arrive at the
# same memory ceiling together, which is how a host that survived one of them
# gets OOM-killed running both.
#
# Stopping scan *before* postdoc starts is deliberate. The overlap is the
# dangerous window; a short gap with nothing analyzing is not, because hopper
# re-leases anything unfinished.
#
# Retire disabled/static units too, plus the old server name and the separate
# idle companion installed by scan's server deploy. Remove local unit files
# after stopping; vendor units remain installed but disabled.
for unit in scan-worker.service scan-server.service scan.service scan-idle.service ascan-worker.service; do
    load_state=$(systemctl show "$unit" -p LoadState --value 2>/dev/null || true)
    if { [ -n "$load_state" ] && [ "$load_state" != not-found ]; } ||
       $SUDO systemctl is-active --quiet "$unit" ||
       [ -e "/etc/systemd/system/$unit" ] || [ -L "/etc/systemd/system/$unit" ]
    then
        log "Retiring ${unit}; postdoc replaces it"
        # Two verbs rather than `disable --now`: that form fails outright when
        # the unit file has already been removed, leaving a service that is
        # still running unstopped. Disable first so a reboot cannot restore it.
        $SUDO systemctl disable "$unit" >/dev/null 2>&1 || true
        $SUDO systemctl stop "$unit" >/dev/null 2>&1 || true
        # Starting postdoc anyway would produce exactly the simultaneous run
        # this section exists to prevent, so this is a hard gate.
        if $SUDO systemctl is-active --quiet "$unit"; then
            die "${unit} did not stop; refusing to start postdoc beside it"
        fi
        enabled_state=$($SUDO systemctl is-enabled "$unit" 2>/dev/null || true)
        case "$enabled_state" in
            enabled|enabled-runtime)
                die "${unit} is still enabled; refusing to leave it alongside postdoc"
                ;;
        esac
        $SUDO rm -f "/etc/systemd/system/$unit"
    fi
done

# --- Service account and state ----------------------------------------------

if ! getent passwd "$SERVICE_USER" >/dev/null; then
    log "Creating service user '$SERVICE_USER'"
    # doas may preserve a caller PATH without /usr/sbin.
    $SUDO env PATH="/usr/local/sbin:/usr/sbin:/sbin:$PATH" \
        useradd --system --user-group --no-create-home \
        --home-dir "$STATE_HOME" --shell /usr/sbin/nologin \
        --comment "Atomdrift postdoc" "$SERVICE_USER"
fi
$SUDO install -d -m0750 -o "$SERVICE_USER" -g "$SERVICE_USER" "$STATE_HOME"
$SUDO install -d -m0700 -o "$SERVICE_USER" -g "$SERVICE_USER" "$STATE_HOME/.tok"

install_credentials

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

TMP_UNIT=$DEPLOY_TMP/postdoc.service
install_binary

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

${UNIT_ENV}

Delegate=yes

# The hard backstop. postdoc throttles itself first, well below this, so
# reaching it means something is wrong rather than merely busy — and a restart
# is the right answer to that.
MemoryMax=${MEMORY_MAX}
MemoryLow=${MEMORY_LOW}
TasksMax=8192
LimitNOFILE=1048576

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

install_changed "$TMP_UNIT" "$UNIT_FILE" 0644 root root
save_configuration
$SUDO systemctl enable "$SERVICE_NAME"
if [ "$DEPLOY_CHANGED" -eq 1 ]; then
    $SUDO systemctl daemon-reload
    $SUDO systemctl restart "$SERVICE_NAME"
else
    log "No changes; leaving service running"
    $SUDO systemctl start "$SERVICE_NAME"
fi
if ! $SUDO systemctl is-active --quiet "$SERVICE_NAME"; then
    $SUDO journalctl -u "$SERVICE_NAME" -n 40 --no-pager || true
    die "$SERVICE_NAME failed to start"
fi
wait_ready
log "$SERVICE_NAME is running"
