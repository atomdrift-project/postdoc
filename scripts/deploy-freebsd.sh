#!/bin/sh
# Install postdoc as a FreeBSD rc.d service — `worker` or `serve`.
#
#   scripts/deploy-freebsd.sh worker https://hopper.example [build-binary]
#   scripts/deploy-freebsd.sh serve  [https://hopper.example] [build-binary]
#
# Same contract as the systemd path: the binary owns its defaults and this
# passes the hopper URL and installs the separate API and hopper credentials.
# What differs is the supervision — daemon(8) rather than systemd — and two
# things FreeBSD needs that Linux does not.
#
# Environment (all optional):
#   NICE        scheduling priority     (default -20)
#   LLM         SCAN_LLM: `local`, `openrouter`, or a base URL
#   LLM_MODEL   SCAN_LLM_MODEL; unset lets postdoc pick
#   HOPPER_TOKEN_FILE  hopper bearer token (default ~/.tok/hopper; TOKEN_FILE is a legacy alias)
#   SCAN_TOKEN_FILE   API bearer token for Beamline (default ~/.tok/scan)
#   LLM_TOKEN_FILE    LLM endpoint bearer token (default ~/.tok/llm)

set -eu
# pw, sysrc, and service live in sbin, which a deploying user's PATH may omit.
PATH="/usr/local/sbin:/usr/sbin:/sbin:$PATH"
export PATH

MODE="${1:-}"
BUILD_BIN="${3:-target/release/postdoc}"
case "$MODE" in
    worker)
        URL="${2:-}"
        [ -n "$URL" ] || { echo "error: worker needs a hopper URL" >&2; exit 1; }
        SERVICE_NAME=postdoc_worker
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
STATE_HOME=/var/db/atomdrift/postdoc
BIN_DIR=${STATE_HOME}/bin
BIN_PATH=${BIN_DIR}/${BINARY}
RC_FILE=/usr/local/etc/rc.d/${SERVICE_NAME}
LOGFILE=/var/log/${SERVICE_NAME}.log

NICE="${NICE:--20}"
LLM="${LLM:-}"
LLM_MODEL="${LLM_MODEL:-}"
HOPPER_TOKEN_FILE="${HOPPER_TOKEN_FILE:-${TOKEN_FILE:-$HOME/.tok/hopper}}"
SCAN_TOKEN_FILE="${SCAN_TOKEN_FILE:-$HOME/.tok/scan}"
LLM_TOKEN_FILE="${LLM_TOKEN_FILE:-$HOME/.tok/llm}"

die() { echo "error: $*" >&2; exit 1; }
log() { printf '==> %s\n' "$*"; }

[ "$(uname -s)" = FreeBSD ] || die "this is the FreeBSD path"
# As root nothing is needed; otherwise prefer doas and fall back to sudo.
SUDO=""
if [ "$(id -u)" -ne 0 ]; then
    if command -v doas >/dev/null 2>&1; then
        SUDO=doas
    elif command -v sudo >/dev/null 2>&1; then
        SUDO=sudo
    else
        die "need doas or sudo"
    fi
fi

[ -x "$BUILD_BIN" ] || die "$BUILD_BIN is missing or not executable"

ROOT_GROUP=wheel
CONFIG_DIR=/usr/local/etc/postdoc
account_home=$(getent passwd "$SERVICE_USER" | cut -d: -f6 || true)
[ -z "$account_home" ] || STATE_HOME=$account_home
BIN_PATH=$STATE_HOME/bin/$BINARY
. "$(dirname "$0")/deploy-common.sh"
prepare_credentials
ensure_runtime_tools FreeBSD
command -v python3 >/dev/null 2>&1 || die "python3 is required; use make deploy-server to bootstrap prerequisites"
prepare_arguments
# Escape the serialized argument string for rc.d's enclosing double quotes.
RC_EXEC_ARGS=$(printf '%s' "$EXEC_ARGS" | sed 's/\\/\\\\/g; s/"/\\"/g; s/\$/\\$/g; s/`/\\`/g')

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
# Disable and stop before removing the rc.d scripts. Include the older worker
# name, server aliases, and the separate idle companion.
retired=""
for svc in scan-worker scan-server scan scan-idle ascan-worker; do
    [ -f "/usr/local/etc/rc.d/${svc}" ] || [ -L "/usr/local/etc/rc.d/${svc}" ] || continue
    log "Retiring ${svc}; postdoc replaces it"
    # An rc.conf variable spells a hyphenated service with an underscore.
    $SUDO sysrc "$(echo "$svc" | tr - _)_enable=NO" >/dev/null
    # Force the stop even when the service was already disabled in rc.conf.
    $SUDO service "$svc" onestop >/dev/null 2>&1 || true
    retired="$retired $svc"
done

# daemon(8)'s pidfile names the supervisor, not the atomscan child it forked, so
# a stop that times out leaves the analysis process running and still holding
# its leases. scan's own uninstaller reaps it by name for the same reason. Only
# after a service was stood down, so a hand-run atomscan on a host with no scan
# service installed is left alone. postdoc's binary is `postdoc`, so this can
# never reach postdoc itself.
if [ -n "$retired" ] && $SUDO pgrep -x atomscan >/dev/null 2>&1; then
    log "Reaping the atomscan process daemon(8) left behind"
    $SUDO pkill -9 -x atomscan >/dev/null 2>&1 || true
    sleep 1
    if $SUDO pgrep -x atomscan >/dev/null 2>&1; then
        die "atomscan is still running; refusing to start postdoc beside it"
    fi
fi

for svc in $retired; do
    if $SUDO service "$svc" onestatus >/dev/null 2>&1; then
        die "${svc} did not stop; refusing to start postdoc beside it"
    fi
    $SUDO rm -f "/usr/local/etc/rc.d/${svc}"
done

# --- Service account and state ----------------------------------------------

if ! pw usershow "$SERVICE_USER" >/dev/null 2>&1; then
    log "Creating service user '$SERVICE_USER'"
    $SUDO pw useradd -n "$SERVICE_USER" -d "$STATE_HOME" -s /usr/sbin/nologin \
        -c "Atomdrift postdoc"
fi
$SUDO install -d -m0750 -o "$SERVICE_USER" -g "$SERVICE_USER" "$STATE_HOME"
$SUDO install -d -m0700 -o "$SERVICE_USER" -g "$SERVICE_USER" "$STATE_HOME/.tok"

install_credentials
LLM_MODEL_ENV=""
[ -z "$LLM_MODEL" ] || LLM_MODEL_ENV="SCAN_LLM_MODEL=$LLM_MODEL"
TMP_RC=$DEPLOY_TMP/postdoc.rc
install_binary

cat > "$TMP_RC" <<RCEOF
#!/bin/sh
#
# PROVIDE: ${SERVICE_NAME}
# REQUIRE: NETWORKING
# KEYWORD: shutdown

. /etc/rc.subr

name="${SERVICE_NAME}"
rcvar="${SERVICE_NAME}_enable"
load_rc_config \$name

: \${${SERVICE_NAME}_enable:="NO"}
: \${${SERVICE_NAME}_nice:="${NICE}"}
: \${${SERVICE_NAME}_llm:="${LLM}"}
: \${${SERVICE_NAME}_logfile:="${LOGFILE}"}
# Seconds a graceful stop waits for in-flight analyses to drain before the
# whole daemon(8) tree is SIGKILLed. Hopper re-leases anything unfinished.
: \${${SERVICE_NAME}_stop_timeout:="20"}
# FreeBSD has no oom_score_adj; protect(1) sets P_PROTECTED, which exempts the
# process from the swap-exhaustion killer. Inherited across fork and survives
# daemon(8)'s setuid.
: \${${SERVICE_NAME}_protect:="YES"}

pidfile="/var/run/\${name}.pid"
command="/usr/sbin/daemon"

# MALLOC_CONF tunes FreeBSD's in-libc jemalloc to return freed memory promptly
# instead of holding dirty pages for 10s. Set via /usr/bin/env so it survives
# daemon(8)'s user switch and login.conf environment filtering.
#
# Do NOT add background_thread:true. FreeBSD's libc jemalloc is built without
# JEMALLOC_BACKGROUND_THREAD, so background_thread_boot0() fails — and it is
# called from malloc_init_hard() *after* the state is malloc_init_recursible.
# Init returns early, malloc_initialized() is false forever, and EVERY
# allocation re-enters malloc_init_hard() and serializes on the global
# init_lock. It is not a config error, so abort_conf:true does not catch it and
# nothing is logged. Measured on a 128-core host: with the option, 14-28 cores
# busy and the worker wedged with every slot occupied and zero completions for
# 14h27m; without it, 126/128 and steady completions.
#
# junk:false turns off fill-on-malloc/fill-on-free. FreeBSD builds libc's
# jemalloc with --enable-fill on -CURRENT, so without this every allocation and
# free memsets its region, charged to whoever called malloc and invisible in a
# profile. Measured over four large nested archives: 284.7s stock, 227.7s with
# junk:false, identical result hashes. Peak RSS 16.0 GiB -> 14.0 GiB.
malloc_conf="dirty_decay_ms:1000,muzzy_decay_ms:0,abort_conf:true,junk:false"

# -r -R 5 supervises and restarts forever after any exit, so an OOM kill or a
# panic self-heals. RUST_BACKTRACE=1 so a panic names the frame that raised it;
# the cost is paid only on a panic, which is already a lost job.
command_args="-c -f -r -R 5 -P \${pidfile} -o \${${SERVICE_NAME}_logfile} -u ${SERVICE_USER} \\
  /usr/bin/env PATH=${SERVICE_PATH} MALLOC_CONF=\${malloc_conf} RUST_BACKTRACE=1 SCAN_LLM=\${${SERVICE_NAME}_llm} ${LLM_MODEL_ENV} \\
  ${BIN_PATH} ${RC_EXEC_ARGS}"

# daemon(8) -u switches user with setusercontext(3), which applies the login
# class's priority capability — 0 for the default class — *after* rc's nice(1).
# The supervisor keeps the nice value; the child it forks is reset to 0, so the
# priority this service exists to hold is silently lost. Re-apply it to the
# child once it appears; analysis children forked later inherit it.
start_postcmd="${SERVICE_NAME}_postcmd"
${SERVICE_NAME}_postcmd()
{
	_p=0
	while [ \$_p -lt 10 ]; do
		_sup=\$(cat "\${pidfile}" 2>/dev/null)
		case "\${_sup}" in
		''|*[!0-9]*) _sup="" ;;
		esac
		if [ -n "\${_sup}" ]; then
			_kid=\$(pgrep -P "\${_sup}" 2>/dev/null | head -1)
			if [ -n "\${_kid}" ]; then
				# Absolute priority, not \`renice -n\`, which is an increment
				# on FreeBSD and would compound on every restart.
				renice "\${${SERVICE_NAME}_nice}" -p "\${_kid}" >/dev/null 2>&1
				checkyesno ${SERVICE_NAME}_protect && \\
					protect -p "\${_kid}" >/dev/null 2>&1
				return 0
			fi
		fi
		sleep 1
		_p=\$((_p + 1))
	done
	echo "${SERVICE_NAME}: could not find the supervised process." >&2
}

# Bounded, orphan-free stop. rc.subr's default waits on the supervisor forever,
# so a slow drain blocks a reboot. SIGTERM the supervisor, wait, then force the
# tree down: SIGKILL the supervisor first so -r cannot respawn, then any child
# still standing (a SIGKILLed supervisor cannot reap its own child).
stop_cmd="${SERVICE_NAME}_stop"
${SERVICE_NAME}_stop()
{
	_sup=\$(cat "\${pidfile}" 2>/dev/null)
	case "\${_sup}" in
	''|*[!0-9]*) _sup="" ;;
	esac
	if [ -z "\${_sup}" ] || ! kill -0 "\${_sup}" 2>/dev/null; then
		echo "${SERVICE_NAME} not running."
		rm -f "\${pidfile}"
		return 0
	fi
	echo "Stopping ${SERVICE_NAME} (pid \${_sup}); up to \${${SERVICE_NAME}_stop_timeout}s to drain."
	kill -TERM "\${_sup}" 2>/dev/null
	_waited=0
	while kill -0 "\${_sup}" 2>/dev/null; do
		[ "\${_waited}" -ge "\${${SERVICE_NAME}_stop_timeout}" ] && break
		sleep 1
		_waited=\$((_waited + 1))
	done
	if kill -0 "\${_sup}" 2>/dev/null; then
		echo "${SERVICE_NAME} did not drain in time; forcing SIGKILL."
		kill -KILL "\${_sup}" 2>/dev/null
		pkill -9 -x ${BINARY} 2>/dev/null || true
	fi
	rm -f "\${pidfile}"
}

run_rc_command "\$1"
RCEOF

install_changed "$TMP_RC" "$RC_FILE" 0755 root wheel
# Persist empty too: LLM= deliberately turns a previous endpoint off.
old_llm=$($SUDO sysrc -n "${SERVICE_NAME}_llm" 2>/dev/null || true)
[ "$old_llm" = "$LLM" ] || DEPLOY_CHANGED=1
$SUDO sysrc "${SERVICE_NAME}_llm=${LLM}" >/dev/null
$SUDO sysrc "${SERVICE_NAME}_enable=YES" >/dev/null
$SUDO touch "$LOGFILE"
$SUDO chown "${SERVICE_USER}:${SERVICE_USER}" "$LOGFILE"
save_configuration
if [ "$DEPLOY_CHANGED" -eq 1 ]; then
    $SUDO service "$SERVICE_NAME" restart
else
    log "No changes; leaving service running"
    $SUDO service "$SERVICE_NAME" status >/dev/null 2>&1 || $SUDO service "$SERVICE_NAME" start
fi
$SUDO service "$SERVICE_NAME" status >/dev/null || die "$SERVICE_NAME failed to start"
wait_ready
log "$SERVICE_NAME is running"
