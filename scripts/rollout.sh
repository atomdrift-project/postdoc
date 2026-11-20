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
# Each host's deploy disables and stops `scan-worker` and `scan` before it
# starts postdoc: the two cannot share a machine, since each sizes itself
# against the whole of it. That lives in the deploy scripts rather than here,
# so a deploy run by hand stands scan down too. A scan service that will not
# stop fails the host's deploy, and the summary below shows it as FAILED.
#
# Env:
#   DAYS          how far back a check-in still counts        (default 7)
#   BATCH         workers deployed concurrently, 0 for all    (default 0)
#   SERVER_BATCH  servers one at a time (1) or all at once (0) (default 1)
#   HOSTS         space-separated hosts, skipping discovery
#   SKIP          space-separated hosts to leave alone
#   PHASES        which of `hopper workers servers` to run    (default all)
#   HOPPER_NODE   SSH host for Hopper (default 10.9.8.10)
#   HOPPER_HEALTH Hopper health URL on that host (default http://127.0.0.1:8081/healthz)
#   URL           explicit Hopper override; otherwise preserve installed settings
#   HOPPER_DASH   hopper's dashboard, whose /workers.json is the roster
#                 (default http://$HOPPER_NODE:8082)
#   MIN_FREE_GB   free disk a host must have before it is built on  (default 20)
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
HOPPER_NODE="${HOPPER_NODE:-10.9.8.10}"
HOPPER_HEALTH="${HOPPER_HEALTH:-http://127.0.0.1:8081/healthz}"
HOPPER_DASH="${HOPPER_DASH:-http://$HOPPER_NODE:8082}"
TIMEOUT="${TIMEOUT:-1800}"
HEALTH_ADDR="${HEALTH_ADDR:-}"
HEALTH_WAIT="${HEALTH_WAIT:-300}"
HEALTH_UPTIME="${HEALTH_UPTIME:-600}"
MIN_FREE_GB="${MIN_FREE_GB:-20}"

log()  { printf '==> %s\n' "$*"; }
note() { printf '    %s\n' "$*"; }
warn() { printf '!!! %s\n' "$*" >&2; }
die()  { warn "$*"; exit 1; }

# Under /tmp, not $TMPDIR: the ssh control sockets live in here, and a unix
# socket path is limited to 104 bytes. macOS's $TMPDIR alone is ~50, which
# with the 40-byte %C hash overflows it, and ssh then fails every connection.
work="$(mktemp -d /tmp/rollout.XXXXXX)"
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

# Reaches a remote shell's arithmetic, so it is digits or nothing.
case "$MIN_FREE_GB" in ''|*[!0-9]*) die "MIN_FREE_GB must be a whole number of gigabytes, not '$MIN_FREE_GB'" ;; esac

# --- Roster ------------------------------------------------------------------

if [ -z "$HOSTS" ]; then
    command -v curl >/dev/null 2>&1 || die "curl not found; pass HOSTS instead"
    command -v python3 >/dev/null 2>&1 || die "python3 not found; pass HOSTS instead"
    log "Asking hopper which hosts checked in over the last $DAYS days"
    # The roster is what hopper's dashboard shows: its in-memory worker tracker.
    # The workers table in its database is not that. Hopper stops writing it
    # while the verdict store is on, so a roster read from it is frozen at the
    # day that changed and drops hosts as they age out of the window.
    curl -fsS --max-time 15 -o "$work/workers.json" "$HOPPER_DASH/workers.json" ||
        die "could not reach $HOPPER_DASH/workers.json; check HOPPER_DASH, or pass HOSTS"
    # A hopper that predates the route answers every path with the dashboard
    # page, which is a 200 and not JSON.
    # A server's embedded idle worker checks in as `<host>-idle`, so the suffix
    # is what separates a server from a worker. Newest check-in per host wins.
    # `local` is hopper's own built-in worker: it is redeployed with hopper, and
    # is not a host that can be reached.
    roster=$(python3 -I -c '
import datetime, json, re, sys
cutoff = datetime.datetime.now(datetime.timezone.utc) - datetime.timedelta(days=float(sys.argv[1]))
newest = {}
try:
    workers = json.load(open(sys.argv[2]))
except ValueError:
    sys.exit(1)
for w in workers:
    name = w["name"].split(":")[0]
    if name == "local":
        continue
    seen = re.sub(r"(\.\d{6})\d+", r"\1", w["last_seen"]).replace("Z", "+00:00")
    seen = datetime.datetime.fromisoformat(seen)
    host = name[: -len("-idle")] if name.endswith("-idle") else name
    if seen >= cutoff and (host not in newest or seen > newest[host][0]):
        newest[host] = (seen, name)
print("\n".join(name for _, name in sorted(newest.values(), key=lambda v: v[1])))
' "$DAYS" "$work/workers.json") ||
        die "$HOPPER_DASH/workers.json is not a worker list: is hopper older than that route? run PHASES=hopper first, or pass HOSTS"
    [ -n "$roster" ] || die "hopper has seen no workers in the last $DAYS days"
else
    roster="$HOSTS"
fi

self="$(hostname -s 2>/dev/null || hostname)"
workers=""
servers=""
for entry in $roster; do
    host="${entry%-idle}"
    # Worker names are self-chosen and reach ssh + local filenames below:
    # a name like `-oProxyCommand=...` would be parsed as an ssh option
    # despite quoting, and one with `/` would escape the status dir.
    case "$host" in
        *[!A-Za-z0-9._-]*|'') note "skipping unsafe worker name: $entry"; continue ;;
    esac
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

note "Hopper SSH host: $HOPPER_NODE (phases: $PHASES)"
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
# on disk is the only thing that knows what it is now. Windows has no unit file,
# so its service manager is asked instead; it carries workers only, the way
# scan's rollout has it, and needs an ssh server and a POSIX `sh` to be driven.
#
# Exit codes: 90 no repo, 91 no postdoc service.
remote_script() {
    if [ "${1:-postdoc}" = hopper ]; then
        cat <<'HOPPER_REMOTE'
set -eu
cd "$HOME/hopper" || exit 90
git pull --ff-only
make deploy
# A fresh TCP request must work before any analysis host is redeployed.
waited=0
while [ "$waited" -lt 300 ]; do
    if curl -fsS --max-time 5 "$ROLLOUT_HOPPER_HEALTH" >/dev/null; then exit 0; fi
    sleep 2
    waited=$((waited + 2))
done
echo "Hopper did not become ready" >&2
exit 1
HOPPER_REMOTE
        return
    fi
    cat <<'REMOTE'
set -eu
# Where the checkout lives is the host's choice. Windows hosts keep source
# under src\ -- in the profile or at the drive root -- rather than in $HOME.
set -- "$HOME/postdoc"
case "$(uname -s)" in
MSYS*|MINGW*|CYGWIN*) set -- "$@" "$HOME/src/postdoc" /c/src/postdoc ;;
esac
repo=""
for d in "$@"; do
    if [ -d "$d" ]; then repo="$d"; break; fi
done
[ -n "$repo" ] || exit 90
cd "$repo"

# Cargo's built-in git client authenticates through ssh-agent only, and a
# rollout session has none, so a git dependency it has not cached fails with
# "failed to authenticate". The git CLI uses the host's own ssh keys instead.
CARGO_NET_GIT_FETCH_WITH_CLI=true
export CARGO_NET_GIT_FETCH_WITH_CLI

# A release build needs room for its dependencies and for the previous build's
# leftovers. Without this a full disk surfaces as "os error 112" halfway through
# a compile, after the checkout has already been moved. Check before touching
# anything. It does not go looking for what is using the space: walking a full
# disk takes minutes, and the failure should be immediate.
min_free_gb=${ROLLOUT_MIN_FREE_GB:-20}
free_kb=$(df -Pk . | awk 'NR==2 {print $4}')
if [ "${free_kb:-0}" -lt $(( min_free_gb * 1024 * 1024 )) ]; then
    echo "not enough free disk for a build: $(( ${free_kb:-0} / 1024 / 1024 )) GB free under $repo, need $min_free_gb GB" >&2
    exit 1
fi

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

# Windows keeps no unit file at all: deploy-windows.ps1 registers an NSSM
# service, so the service manager is the only thing that can be asked. Spelled
# `sc.exe` because a bare `sc` is a builtin in some shells. Worker only, the
# way scan's rollout has it — there is no Windows server to find.
if [ -z "$worker_unit" ] && sc.exe query postdoc-worker >/dev/null 2>&1; then
    worker_unit="service:postdoc-worker"
fi
server_unit=""
for f in /etc/systemd/system/postdoc.service \
         /usr/local/etc/rc.d/postdoc; do
    if unit_live "$f"; then server_unit="$f"; break; fi
done
if [ -z "$server_unit" ] && sc.exe query postdoc >/dev/null 2>&1; then
    server_unit="service:postdoc"
fi

# Bring the checkout up to date. Deploy hosts are checkouts, not workspaces, so
# what has to hold is "the tree is the upstream branch" -- and `git pull`
# cannot promise that: when main has been force-pushed it aborts with "Not
# possible to fast-forward", failing the host for a reason that has nothing to
# do with it. Fetching and resetting reaches the same commit either way, and
# discards exactly the local commits and edits a deploy host is not supposed to
# have. Untracked files -- build artifacts, caches, token files -- are left
# alone.
upstream=$(git rev-parse --abbrev-ref --symbolic-full-name '@{u}' 2>/dev/null) ||
    upstream=origin/main
git fetch --prune
git reset --hard "$upstream"

# The Makefile is GNU make's. FreeBSD's `make` is BSD make, which cannot parse
# it, and GNU make is installed there as `gmake`.
mk=make
[ "$(uname -s)" != FreeBSD ] || mk=gmake

if [ -n "$server_unit" ]; then
    config=/etc/postdoc/serve.json
    [ "$(uname -s)" != FreeBSD ] || config=/usr/local/etc/postdoc/serve.json
    saved=$(mktemp)
    if [ "$server_unit" = service:postdoc ]; then
        state_home=$(nssm get postdoc AppDirectory | tr -d '\000\r')
        config="$state_home/deploy-serve.json"
        fallback=$(mktemp)
        nssm get postdoc AppParameters | tr -d '\000\r' > "$fallback"
        python3 scripts/deploy-config.py export "$config" "$fallback" > "$saved"
        rm -f "$fallback"
    else
        python3 scripts/deploy-config.py export "$config" "$server_unit" > "$saved"
    fi
    . "$saved"
    rm -f "$saved"
    if [ -n "${ROLLOUT_URL:-}" ]; then HOPPER=$ROLLOUT_URL; export HOPPER; fi
    $mk deploy-server
elif [ -n "$worker_unit" ]; then
    config=/etc/postdoc/worker.json
    [ "$(uname -s)" != FreeBSD ] || config=/usr/local/etc/postdoc/worker.json
    if [ "$worker_unit" = service:postdoc-worker ]; then
        state_home=$(nssm get postdoc-worker AppDirectory | tr -d '\000\r')
        config="$state_home/deploy-worker.json"
        saved=$(mktemp)
        fallback=$(mktemp)
        nssm get postdoc-worker AppParameters | tr -d '\000\r' > "$fallback"
        python3 scripts/deploy-config.py export "$config" "$fallback" > "$saved"
        . "$saved"
        rm -f "$saved" "$fallback"
        URL=${ROLLOUT_URL:-${URL:-}}
    else
        saved=$(mktemp)
        python3 scripts/deploy-config.py export "$config" "$worker_unit" > "$saved"
        . "$saved"
        rm -f "$saved"
        URL=${ROLLOUT_URL:-${URL:-}}
    fi
    [ -n "${URL:-}" ] || { echo "no worker URL configured" >&2; exit 91; }
    $mk deploy-worker URL="$URL"
else
    exit 91
fi
REMOTE
}

status_of() { cut -d' ' -f2 "$work/$1.status" 2>/dev/null || echo "not-reached"; }

connect() {
    host="$1"
    for _ in 1 2 3; do
        # `exit 0`, not `true`: it is the one command sh, cmd.exe and PowerShell
        # all have, and a Windows host's ssh login shell is cmd.exe.
        ssh $SSH_OPTS -o BatchMode=no -- "$host" 'exit 0' >/dev/null 2>&1 && return 0
    done
    echo "$host UNREACHABLE" > "$work/$host.status"
    warn "$host: unreachable"
    return 1
}

deploy_host() {
    host="$1"
    logfile="$work/$host.log"
    started=$(date +%s)

    role=${2:-postdoc}
    # The deploy script is POSIX sh fed over stdin. A Windows host's ssh login
    # shell is cmd.exe: it has no `sh` on PATH, so Git for Windows' is run by
    # its 8.3 path (no quoting needed), and that sh starts with only the Windows
    # PATH, which lacks the Git coreutils (uname, mktemp, ...) the script uses.
    if ssh $SSH_OPTS -- "$host" 'sh -c "exit 0"' >/dev/null 2>&1; then
        remote_sh='sh -s'
        remote_path=''
    else
        remote_sh='C:\Progra~1\Git\usr\bin\sh.exe -s'
        remote_path='PATH=/usr/bin:$PATH; export PATH'
    fi
    # Quote values as shell data in stdin rather than interpolating an ssh command.
    {
        [ -z "$remote_path" ] || printf '%s\n' "$remote_path"
        printf "ROLLOUT_MIN_FREE_GB='%s'\n" "$MIN_FREE_GB"
        printf "ROLLOUT_URL='%s'\n" "$(printf '%s' "$URL" | sed "s/'/'\\\\''/g")"
        printf "ROLLOUT_HOPPER_HEALTH='%s'\n" "$(printf '%s' "$HOPPER_HEALTH" | sed "s/'/'\\\\''/g")"
        remote_script "$role"
    } | ssh $SSH_OPTS -- "$host" $remote_sh >"$logfile" 2>&1 &
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
        health_arg=$(printf '%s' "$HEALTH_ADDR" | sed "s/'/'\\\\''/g")
        body=$(ssh $SSH_OPTS -- "$host" \
            "cd postdoc && python3 scripts/deploy-config.py probe '$health_arg'" 2>/dev/null || true)
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
    hopper=$HOPPER_NODE
    case "$hopper" in ''|-*|*[!A-Za-z0-9._-]*) die "unsafe HOPPER_NODE: $hopper";; esac
    if [ "$hopper" != "$self" ]; then
        log "Hopper $hopper"
        if connect "$hopper"; then deploy_host "$hopper" hopper; fi
        if [ "$(status_of "$hopper")" != ok ]; then
            warn "Hopper deployment/readiness failed; halting the rollout"
            workers=""
            servers=""
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
