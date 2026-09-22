#!/bin/sh
# Install postdoc as a FreeBSD rc.d service — `worker` or `serve`.
#
#   scripts/deploy-freebsd.sh worker https://hopper.example
#   scripts/deploy-freebsd.sh serve  [https://hopper.example]
#
# Same contract as the systemd path: the binary owns its defaults and this
# passes only what it cannot know. What differs is the supervision — daemon(8)
# rather than systemd — and two things FreeBSD needs that Linux does not.
#
# Environment (all optional):
#   NICE        scheduling priority     (default -20)
#   LLM         SCAN_LLM: `local`, `openrouter`, or a base URL
#   LLM_MODEL   SCAN_LLM_MODEL; unset lets postdoc pick
#   TOKEN_FILE  hopper bearer token     (default ~/.tok/hopper)

set -eu

MODE="${1:-}"
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
        echo "usage: $0 worker <hopper-url> | serve [hopper-url]" >&2
        exit 1
        ;;
esac

SERVICE_USER=postdoc
BINARY=postdoc
BIN_PATH=/usr/local/bin/${BINARY}
STATE_HOME=/var/db/atomdrift/postdoc
RC_FILE=/usr/local/etc/rc.d/${SERVICE_NAME}
LOGFILE=/var/log/${SERVICE_NAME}.log

NICE="${NICE:--20}"
LLM="${LLM:-}"
LLM_MODEL="${LLM_MODEL:-}"
TOKEN_FILE="${TOKEN_FILE:-$HOME/.tok/hopper}"

die() { echo "error: $*" >&2; exit 1; }
log() { printf '==> %s\n' "$*"; }

[ "$(uname -s)" = FreeBSD ] || die "this is the FreeBSD path"
[ -x "$BIN_PATH" ] || die "$BIN_PATH is missing or not executable"

# As root nothing is needed; otherwise prefer doas, which is in the FreeBSD
# base system, and fall back to sudo. Testing `command -v "$SUDO"` with SUDO
# empty reports "not found" and would pick sudo even when already root.
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
# Disabled as well as stopped, so a reboot between now and scan's uninstall does
# not quietly restore the collision. The rc.d script itself is left alone:
# removing it is uninstalling scan, which is a separate decision from standing
# it down here. `ascan-worker` is scan-worker's pre-rename name, and a host
# installed before that rename runs the same daemon under it.
retired=""
for svc in scan-worker scan ascan-worker; do
    [ -f "/usr/local/etc/rc.d/${svc}" ] || continue
    log "Retiring ${svc}; postdoc replaces it"
    # An rc.conf variable spells a hyphenated service with an underscore.
    $SUDO sysrc "$(echo "$svc" | tr - _)_enable=NO" >/dev/null 2>&1 || true
    $SUDO service "$svc" stop >/dev/null 2>&1 || true
    retired=yes
done

# daemon(8)'s pidfile names the supervisor, not the atomscan child it forked, so
# a stop that times out leaves the analysis process running and still holding
# its leases. scan's own uninstaller reaps it by name for the same reason. Only
# after a service was stood down, so a hand-run atomscan on a host with no scan
# service installed is left alone. postdoc's binary is `postdoc`, so this can
# never reach postdoc itself.
if [ -n "$retired" ] && pgrep -x atomscan >/dev/null 2>&1; then
    log "Reaping the atomscan process daemon(8) left behind"
    $SUDO pkill -9 -x atomscan >/dev/null 2>&1 || true
    sleep 1
    if pgrep -x atomscan >/dev/null 2>&1; then
        die "atomscan is still running; refusing to start postdoc beside it"
    fi
fi

# --- Service account and state ----------------------------------------------

if ! pw usershow "$SERVICE_USER" >/dev/null 2>&1; then
    log "Creating service user '$SERVICE_USER'"
    $SUDO pw useradd -n "$SERVICE_USER" -d "$STATE_HOME" -s /usr/sbin/nologin \
        -c "Atomdrift postdoc"
fi
$SUDO install -d -m0750 -o "$SERVICE_USER" -g "$SERVICE_USER" "$STATE_HOME"
$SUDO install -d -m0700 -o "$SERVICE_USER" -g "$SERVICE_USER" "$STATE_HOME/.tok"

# Hopper rejects an unauthenticated claim with 401, so a worker without this
# starts, looks healthy, and never gets a job. The token goes in the service
# account's home, never in rc.conf, so rc.conf holds no secret.
if [ -r "$TOKEN_FILE" ]; then
    log "Installing hopper token from $TOKEN_FILE"
    $SUDO install -m0600 -o "$SERVICE_USER" -g "$SERVICE_USER" \
        "$TOKEN_FILE" "$STATE_HOME/.tok/hopper"
elif [ "$MODE" = worker ]; then
    die "no hopper token at $TOKEN_FILE; a worker cannot claim without one"
fi

# Only an operator's explicit pin is passed; unset lets postdoc choose the
# model the endpoint reports.
LLM_MODEL_ENV=""
[ -n "$LLM_MODEL" ] && LLM_MODEL_ENV="SCAN_LLM_MODEL=${LLM_MODEL}"

if [ "$MODE" = worker ]; then
    EXEC_ARGS="worker --url ${URL}"
else
    EXEC_ARGS="serve --token-file ${STATE_HOME}/.tok/hopper"
    [ -n "$URL" ] && EXEC_ARGS="${EXEC_ARGS} --hopper ${URL}"
fi

# --- rc.d script -------------------------------------------------------------

TMP_RC="$(mktemp)"
trap 'rm -f "$TMP_RC"' EXIT

cat > "$TMP_RC" <<RCEOF
#!/bin/sh
#
# PROVIDE: ${SERVICE_NAME}
# REQUIRE: NETWORKING
# KEYWORD: shutdown

. /etc/rc.subr

name="${SERVICE_NAME}"
rcvar="${SERVICE_NAME}_enable"
load_rc_config \\\$name

: \\\${${SERVICE_NAME}_enable:="NO"}
: \\\${${SERVICE_NAME}_nice:="${NICE}"}
: \\\${${SERVICE_NAME}_llm:="${LLM}"}
: \\\${${SERVICE_NAME}_logfile:="${LOGFILE}"}
# Seconds a graceful stop waits for in-flight analyses to drain before the
# whole daemon(8) tree is SIGKILLed. Hopper re-leases anything unfinished.
: \\\${${SERVICE_NAME}_stop_timeout:="20"}
# FreeBSD has no oom_score_adj; protect(1) sets P_PROTECTED, which exempts the
# process from the swap-exhaustion killer. Inherited across fork and survives
# daemon(8)'s setuid.
: \\\${${SERVICE_NAME}_protect:="YES"}

pidfile="/var/run/\\\${name}.pid"
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
command_args="-c -f -r -R 5 -P \\\${pidfile} -o \\\${${SERVICE_NAME}_logfile} -u ${SERVICE_USER} \\
  /usr/bin/env MALLOC_CONF=\\\${malloc_conf} RUST_BACKTRACE=1 SCAN_LLM=\\\${${SERVICE_NAME}_llm} ${LLM_MODEL_ENV} \\
  ${BIN_PATH} ${EXEC_ARGS}"

# daemon(8) -u switches user with setusercontext(3), which applies the login
# class's priority capability — 0 for the default class — *after* rc's nice(1).
# The supervisor keeps the nice value; the child it forks is reset to 0, so the
# priority this service exists to hold is silently lost. Re-apply it to the
# child once it appears; analysis children forked later inherit it.
start_postcmd="${SERVICE_NAME}_postcmd"
${SERVICE_NAME}_postcmd()
{
	_p=0
	while [ \\\$_p -lt 10 ]; do
		_sup=\\\$(cat "\\\${pidfile}" 2>/dev/null)
		case "\\\${_sup}" in
		''|*[!0-9]*) _sup="" ;;
		esac
		if [ -n "\\\${_sup}" ]; then
			_kid=\\\$(pgrep -P "\\\${_sup}" 2>/dev/null | head -1)
			if [ -n "\\\${_kid}" ]; then
				# Absolute priority, not \`renice -n\`, which is an increment
				# on FreeBSD and would compound on every restart.
				renice "\\\${${SERVICE_NAME}_nice}" -p "\\\${_kid}" >/dev/null 2>&1
				checkyesno ${SERVICE_NAME}_protect && \\
					protect -p "\\\${_kid}" >/dev/null 2>&1
				return 0
			fi
		fi
		sleep 1
		_p=\\\$((_p + 1))
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
	_sup=\\\$(cat "\\\${pidfile}" 2>/dev/null)
	case "\\\${_sup}" in
	''|*[!0-9]*) _sup="" ;;
	esac
	if [ -z "\\\${_sup}" ] || ! kill -0 "\\\${_sup}" 2>/dev/null; then
		echo "${SERVICE_NAME} not running."
		rm -f "\\\${pidfile}"
		return 0
	fi
	echo "Stopping ${SERVICE_NAME} (pid \\\${_sup}); up to \\\${${SERVICE_NAME}_stop_timeout}s to drain."
	kill -TERM "\\\${_sup}" 2>/dev/null
	_waited=0
	while kill -0 "\\\${_sup}" 2>/dev/null; do
		[ "\\\${_waited}" -ge "\\\${${SERVICE_NAME}_stop_timeout}" ] && break
		sleep 1
		_waited=\\\$((_waited + 1))
	done
	if kill -0 "\\\${_sup}" 2>/dev/null; then
		echo "${SERVICE_NAME} did not drain in time; forcing SIGKILL."
		kill -KILL "\\\${_sup}" 2>/dev/null
		pkill -9 -x ${BINARY} 2>/dev/null || true
	fi
	rm -f "\\\${pidfile}"
}

run_rc_command "\\\$1"
RCEOF

log "Installing ${RC_FILE}"
$SUDO install -m0755 "$TMP_RC" "$RC_FILE"
$SUDO sysrc "${SERVICE_NAME}_enable=YES" >/dev/null
[ -n "$LLM" ] && $SUDO sysrc "${SERVICE_NAME}_llm=${LLM}" >/dev/null
$SUDO touch "$LOGFILE"
$SUDO chown "${SERVICE_USER}:${SERVICE_USER}" "$LOGFILE"

$SUDO service "$SERVICE_NAME" restart

sleep 3
if $SUDO service "$SERVICE_NAME" status >/dev/null 2>&1; then
    log "${SERVICE_NAME} is running"
else
    $SUDO tail -n 40 "$LOGFILE" 2>/dev/null || true
    die "${SERVICE_NAME} failed to start"
fi
