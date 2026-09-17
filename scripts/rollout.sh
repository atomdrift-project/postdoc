#!/bin/sh
# Redeploy postdoc across the fleet: hopper first, then workers, then servers.
#
#   ./scripts/rollout.sh                  # everything hopper has seen lately
#   HOSTS="a b c" ./scripts/rollout.sh    # just these
#   DRY_RUN=1 ./scripts/rollout.sh        # print the plan and stop
#
# Ordering is the point. Hopper is a hard gate: if the queue does not come back
# no worker or server is touched, because a fleet redeployed against a dead
# queue is a fleet that claims nothing and looks fine doing it. Workers go in
# batches, since losing some of them costs throughput and nothing else. Servers
# go one at a time, each proved healthy before the next is touched, because
# they answer live traffic.
#
# Env:
#   DAYS          how far back a check-in still counts        (default 7)
#   BATCH         workers deployed concurrently, 0 for all    (default 0)
#   SERVER_BATCH  servers one at a time (1) or all at once (0) (default 1)
#   HOSTS         space-separated hosts, skipping discovery
#   SKIP          space-separated hosts to leave alone
#   PHASES        which of `hopper workers servers` to run    (default all)
#   URL           hopper endpoint for a worker whose unit names none
#   DB            hopper database to read the roster from
#   TIMEOUT       seconds before a wedged deploy is cut loose (default 1800)
#   HEALTH_ADDR   where a server answers /_/health   (default 127.0.0.1:49999)
#   HEALTH_WAIT   seconds a server may take to come back      (default 300)
#   HEALTH_UPTIME largest uptime_secs still counted as a restart (default 600)
#   DRY_RUN       set to anything to print the plan and stop
#   TRACE         set to anything to run under `set -x`

set -eu
[ -n "${TRACE:-}" ] && set -x

DAYS="${DAYS:-7}"
BATCH="${BATCH:-0}"
SERVER_BATCH="${SERVER_BATCH:-1}"
HOSTS="${HOSTS:-}"
SKIP="${SKIP:-}"
PHASES="${PHASES:-hopper workers servers}"
URL="${URL:-}"
DB="${DB:-}"
TIMEOUT="${TIMEOUT:-1800}"
HEALTH_ADDR="${HEALTH_ADDR:-127.0.0.1:49999}"
HEALTH_WAIT="${HEALTH_WAIT:-300}"
HEALTH_UPTIME="${HEALTH_UPTIME:-600}"

log()  { printf '==> %s\n' "$*"; }
note() { printf '    %s\n' "$*"; }
warn() { printf '!!! %s\n' "$*" >&2; }
die()  { warn "$*"; exit 1; }

work="$(mktemp -d)"
ctl="$work/ssh"
mkdir -p "$ctl"
# Close every multiplexed master on the way out, whatever the exit path.
cleanup() {
    for sock in "$ctl"/*; do
        [ -S "$sock" ] && ssh -o ControlPath="$sock" -O exit x >/dev/null 2>&1
    done
    rm -rf "$work"
}
trap cleanup EXIT INT TERM

# One authentication per host, reused by every later command — which is what
# makes a parallel batch cost one touch of a hardware key rather than N.
SSH_OPTS="-o ControlMaster=auto -o ControlPath=$ctl/%C -o ControlPersist=900 \
  -o ConnectTimeout=15 -o ServerAliveInterval=30 -o StrictHostKeyChecking=accept-new"

# --- Roster ------------------------------------------------------------------

if [ -z "$HOSTS" ]; then
    [ -n "$DB" ] || die "set DB=<hopper postgres url>, or pass HOSTS=\"a b c\""
    command -v psql >/dev/null 2>&1 || die "psql not found; pass HOSTS instead"
    log "Asking hopper which hosts checked in over the last $DAYS days"
    # A server's embedded idle worker checks in as `<host>-idle`, so the suffix
    # is what separates a server from a worker. Newest check-in per host wins.
    roster=$(psql "$DB" -Atq -c "
        SELECT DISTINCT ON (host) host || CASE WHEN idle THEN '-idle' ELSE '' END
        FROM (
            SELECT regexp_replace(split_part(name, ':', 1), '-idle\$', '') AS host,
                   split_part(name, ':', 1) LIKE '%-idle'                  AS idle,
                   last_seen
            FROM workers
            WHERE last_seen > now() - interval '$DAYS days'
        ) t
        ORDER BY host, last_seen DESC") ||
        die "could not read the worker roster from $DB"
    [ -n "$roster" ] || die "hopper has seen no workers in the last $DAYS days"
else
    roster="$HOSTS"
fi

self="$(hostname -s 2>/dev/null || hostname)"
workers=""
servers=""
for entry in $roster; do
    host="${entry%-idle}"
    case " $SKIP " in *" $host "*) note "skipping $host"; continue ;; esac
    # Never redeploy the host running the rollout: it would cut the connection
    # driving it partway through.
    [ "$host" = "$self" ] && { note "skipping $host (this host)"; continue; }
    if [ "$entry" != "$host" ]; then
        servers="$servers $host"
    else
        workers="$workers $host"
    fi
done

log "Plan: $(echo "$workers" | wc -w | tr -d ' ') workers, $(echo "$servers" | wc -w | tr -d ' ') servers"
[ -n "$workers" ] && note "workers:$workers"
[ -n "$servers" ] && note "servers:$servers"
if [ -n "${DRY_RUN:-}" ]; then
    log "DRY_RUN set; stopping here"
    exit 0
fi

# --- Remote ------------------------------------------------------------------

# What runs on a host is decided by what is *installed* there, not by the
# roster: a host can be reimaged or repurposed between check-ins, and the unit
# on disk is the only thing that knows what it is now.
#
# Exit codes: 90 no repo, 91 no postdoc service.
remote_script() {
    cat <<'REMOTE'
set -eu
repo="$HOME/postdoc"
[ -d "$repo" ] || exit 90
cd "$repo"

unit_live() {
    case "$1" in
    *.service)
        systemctl is-enabled "$(basename "$1")" >/dev/null 2>&1 ||
            systemctl is-active "$(basename "$1")" >/dev/null 2>&1
        ;;
    *) [ -x "$1" ] && grep -q '_enable' "$1" 2>/dev/null ;;
    esac
}

worker_unit=""
for f in /etc/systemd/system/postdoc-worker.service \
         /usr/local/etc/rc.d/postdoc_worker; do
    if unit_live "$f"; then worker_unit="$f"; break; fi
done
server_unit=""
for f in /etc/systemd/system/postdoc.service \
         /usr/local/etc/rc.d/postdoc; do
    if unit_live "$f"; then server_unit="$f"; break; fi
done

git pull --ff-only

if [ -n "$server_unit" ]; then
    make deploy-server URL="${ROLLOUT_URL:-}"
elif [ -n "$worker_unit" ]; then
    # Re-read the hopper URL out of the unit rather than assuming the roster's:
    # a worker pointed at a different queue must stay pointed at it.
    url=$(grep -oE -- '--url[= ][^ "]+' "$worker_unit" | head -1 | sed 's/^--url[= ]//')
    [ -n "$url" ] || url="${ROLLOUT_URL:-}"
    [ -n "$url" ] || { echo "no --url in $worker_unit and no URL given" >&2; exit 91; }
    make deploy-worker URL="$url"
else
    exit 91
fi
REMOTE
}

status_of() { cut -d' ' -f2 "$work/$1.status" 2>/dev/null || echo "not-reached"; }

connect() {
    host="$1"
    for _ in 1 2 3; do
        ssh $SSH_OPTS -o BatchMode=no "$host" true >/dev/null 2>&1 && return 0
    done
    echo "$host UNREACHABLE" > "$work/$host.status"
    warn "$host: unreachable"
    return 1
}

deploy_host() {
    host="$1"
    logfile="$work/$host.log"
    started=$(date +%s)

    ROLLOUT_URL="$URL" remote_script |
        ssh $SSH_OPTS "$host" "ROLLOUT_URL='$URL' sh -s" >"$logfile" 2>&1 &
    ssh_pid=$!
    # A wedged deploy must not hold the whole rollout. The killer races the
    # deploy and loses in the normal case.
    ( sleep "$TIMEOUT"; kill "$ssh_pid" 2>/dev/null ) >/dev/null 2>&1 &
    killer=$!

    rc=0
    wait "$ssh_pid" || rc=$?
    kill "$killer" 2>/dev/null || true
    elapsed=$(( $(date +%s) - started ))

    case "$rc" in
    0)  echo "$host ok" > "$work/$host.status"; log "$host: ok (${elapsed}s)" ;;
    90) echo "$host no-repo" > "$work/$host.status"; warn "$host: no postdoc checkout" ;;
    91) echo "$host no-service" > "$work/$host.status"; warn "$host: no postdoc service installed" ;;
    *)
        if [ "$elapsed" -ge "$TIMEOUT" ]; then
            echo "$host TIMEOUT" > "$work/$host.status"
            warn "$host: cut loose after ${elapsed}s"
        else
            echo "$host FAILED" > "$work/$host.status"
            warn "$host: failed (rc=$rc)"
        fi
        tail -n 15 "$logfile" >&2 2>/dev/null || true
        ;;
    esac
}

# Two questions, not one. Answering `/_/health` proves the port is bound; a
# small uptime proves it is the *new* process answering. A server that failed
# to restart answers happily with the old binary, and that is the failure this
# gate exists to catch.
await_health() {
    host="$1"
    waited=0
    while [ "$waited" -lt "$HEALTH_WAIT" ]; do
        body=$(ssh $SSH_OPTS "$host" \
            "curl -sf --max-time 10 http://$HEALTH_ADDR/_/health" 2>/dev/null || true)
        status=$(printf '%s' "$body" | sed -n 's/.*"status":"\([a-z]*\)".*/\1/p')
        uptime=$(printf '%s' "$body" | sed -n 's/.*"uptime_secs":\([0-9]*\).*/\1/p')
        if [ "$status" = "ok" ]; then
            if [ -n "$uptime" ] && [ "$uptime" -gt "$HEALTH_UPTIME" ]; then
                if grep -q 'No changes;' "$work/$host.log" 2>/dev/null; then
                    note "$host: healthy — deploy changed nothing, so ${uptime}s uptime is expected"
                    return 0
                fi
                warn "$host: healthy but uptime is ${uptime}s — it did not restart"
                return 1
            fi
            note "$host: healthy after ${uptime:-?}s of uptime"
            return 0
        fi
        sleep 5
        waited=$(( waited + 5 ))
    done
    warn "$host: no healthy answer within ${HEALTH_WAIT}s"
    return 1
}

# --- Phase 0: hopper ---------------------------------------------------------

case " $PHASES " in *" hopper "*)
    hopper=""
    [ -n "$URL" ] && hopper=$(printf '%s' "$URL" | sed -E 's#^[a-z]+://##; s#[:/].*$##')
    if [ -n "$hopper" ] && [ "$hopper" != "$self" ]; then
        log "Hopper $hopper"
        if connect "$hopper"; then
            deploy_host "$hopper"
            if [ "$(status_of "$hopper")" != "ok" ]; then
                warn "the queue did not come back — halting before any worker or server"
                workers=""
                servers=""
            fi
        fi
    fi
    ;;
esac

# --- Phase 1: workers --------------------------------------------------------

case " $PHASES " in *" workers "*)
    if [ -n "$workers" ]; then
        set -- $workers
        while [ $# -gt 0 ]; do
            batch=""
            n=0
            while [ $# -gt 0 ] && { [ "$BATCH" -eq 0 ] || [ "$n" -lt "$BATCH" ]; }; do
                batch="$batch $1"; shift; n=$(( n + 1 ))
            done
            log "Opening connections — touch your key when prompted"
            for host in $batch; do connect "$host" || true; done
            log "Deploying $(echo "$batch" | wc -w | tr -d ' ') workers in parallel"
            for host in $batch; do
                [ -f "$work/$host.status" ] || deploy_host "$host" &
            done
            wait
        done
    fi
    ;;
esac

# --- Phase 2: servers --------------------------------------------------------

case " $PHASES " in *" servers "*)
    for host in $servers; do
        log "Server $host"
        connect "$host" || continue
        deploy_host "$host"
        [ "$(status_of "$host")" = "ok" ] || {
            warn "$host: deploy failed; not gating, and not moving on"
            break
        }
        if [ "$SERVER_BATCH" -eq 1 ]; then
            await_health "$host" || {
                echo "$host UNHEALTHY" > "$work/$host.status"
                warn "stopping: $host did not come back healthy, and the next one is not worth risking"
                break
            }
        fi
    done
    ;;
esac

# --- Summary -----------------------------------------------------------------

rc=0
log "Summary"
for f in "$work"/*.status; do
    [ -e "$f" ] || continue
    host=$(cut -d' ' -f1 "$f")
    state=$(cut -d' ' -f2 "$f")
    printf '    %-28s %s\n' "$host" "$state"
    case "$state" in ok) ;; *) rc=1 ;; esac
done
exit "$rc"
