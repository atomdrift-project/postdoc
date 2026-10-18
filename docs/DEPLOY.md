# Deploying postdoc

```sh
make deploy-worker URL=https://hopper.example   # this host
make deploy-server                              # this host, default Hopper pair
make deploy-server URL=https://hopper.example   # override the Hopper pair
make deploy-server HOPPER=none                  # explicitly disable Hopper
make rollout                                    # the fleet
```

`make` picks the platform:

| Platform | Supervision | Script |
| --- | --- | --- |
| Linux | systemd | `scripts/deploy.sh` |
| FreeBSD | rc.d + daemon(8) | `scripts/deploy-freebsd.sh` |
| Windows | NSSM | `scripts/deploy-windows.ps1` |

Nothing else is supported, and the Makefile says so rather than guessing.

Server deployment preserves Scan's default Hopper pair:
`https://hops-ro.isotope13.ai,https://hops.isotope13.ai`. Reads prefer the
replica; the companion idle worker claims from the primary. `HOPPER=` is an
error; `HOPPER=none` explicitly disables Hopper uploads, corpus lookups, and
idle work. A nonempty `URL=` overrides `HOPPER` for server deployments.
Without a Hopper URL, a server cannot poll for idle work even when its token
is installed. Standalone worker deployments still require `URL=`.

On Linux and FreeBSD, run `make deploy-worker` or `make deploy-server` as your
usual user. The build runs as you; installation and service management prefer
`doas`, falling back to `sudo` when `doas` is unavailable. No wrapper is needed
when already root. Deployment creates the `postdoc` service account and
installs the binary in that user's `~/bin/postdoc`. The binary and its `bin`
directory remain root-owned (`root:root` on Linux, `root:wheel` on FreeBSD).
Redeploys replace the binary by rename so a running service can be upgraded.

| Platform | Service home and writable state | Enabled/restarted service |
| --- | --- | --- |
| Linux | `/var/lib/atomdrift/postdoc` | `postdoc-worker.service` or `postdoc.service` |
| FreeBSD | `/var/db/atomdrift/postdoc` | `postdoc_worker` or `postdoc` (rc.d) |

## Runtime prerequisites

Both worker and server deployments require `rizin`, `upx`, `innoextract`, and
one of `7z`, `7za`, `7zz`, or `7zr`. These checks run before retiring Scan or
replacing the service binary. An unavailable tool fails deployment.

Linux and FreeBSD automatically install missing tools using the host package
manager through `doas` or `sudo`. Already available tools need no package
operation. Linux supports apt, dnf/yum, zypper, pacman, and xbps; FreeBSD uses
pkg. The installer tries current package names before older names and checks
again after installation. It does not enable additional repositories or
perform a full system upgrade; install unavailable packages manually before
retrying. Windows validates tools on the service's inherited PATH; install
missing tools using Scoop, winget, or another system installer first.

Unix services use the explicit PATH
`/usr/local/sbin:/usr/local/bin:/usr/sbin:/usr/bin:/sbin:/bin`, which is also
used for prerequisite checks. Tools installed only in the deploying user's
home do not satisfy this check. Restart after installing tools on a running
host: tool availability is cached by the analysis engines.

Run `make test-deploy` to exercise the Unix prerequisite installer with mock
package managers. These tests do not install software or change services.

## Deployment parity audit (2026-10-18)

Compared the current worker and server deployment scripts with the sibling
Scan checkout's Linux, FreeBSD, and Windows deployment paths. Runtime tool
installation/validation is addressed above. The following differences remain;
they are findings, not changes made as part of the prerequisite fix.

| Priority | Finding | Effect |
| --- | --- | --- |
| High | Rollout's Hopper phase calls the Postdoc deployment routine. | With `URL` set, it looks for `$HOME/postdoc` and a Postdoc service on the queue host instead of deploying Hopper from `$HOME/hopper`. Without `URL`, the phase is skipped. It does not provide the documented Hopper-first deployment. |
| High | Several Scan deployment overrides are not forwarded. | `IDLE=0`, `BIND`, `ALLOW_CIDR`, `WORKERS`, `DATA_DIR`, and `MAX_RSS_GB` do not configure the generated Postdoc services. In particular, `IDLE=0` does not disable the idle worker. Explicit command-line or environment support in the binary does not make these make-time settings reach the service. |
| High | LLM settings and credential handling are incomplete. | Windows defaults its script parameters to empty strings instead of reading the Makefile's exported `LLM` and `LLM_MODEL`, and does not copy the LLM token. Unix copies `.tok/llm` but does not carry over Scan's OpenRouter key installation or `SCAN_LLM_KEY` provisioning. FreeBSD only writes a nonempty LLM setting to rc.conf, so `LLM=` cannot clear a previously configured endpoint. |
| High | Server and idle-worker resource policies changed. | Scan caps the server's default memory allowance at 128 GiB and runs Linux idle work in its own unit with a smaller allowance and low CPU/IO weights. Postdoc uses an uncapped 80% host allowance and a 50% host reclaim-protected floor; its companion shares the server's cgroup and CPU/IO weights. Freezing on interactive requests is retained, but memory and competition with other services differ. These need an explicit policy decision. |
| Medium | Blank token files pass deployment preflight. | Readability alone does not establish a usable credential. A blank Hopper token can produce a running worker whose requests are rejected; a blank API token fails after deployment has already replaced files. Scan checked Hopper tokens for non-whitespace content. |
| Medium | Redeployment always replaces files and restarts. | Scan compares binary, unit, and credential contents and leaves an unchanged service running. Postdoc interrupts in-flight work even on a no-op deployment. |
| Medium | Fresh-host build setup is absent. | `make deploy-*` invokes Cargo before its deployment script. It cannot bootstrap Rust, the compiler/linker, and required development libraries as Scan's host scripts did. The runtime installer assumes the release binary can already be built. |
| Medium | Storage and existing-home migration handling is reduced. | Linux does not resolve a symlinked state directory before placing it in `ReadWritePaths`, as Scan did. FreeBSD hard-codes `/var/db/atomdrift/postdoc` instead of consulting an existing account's home; a differently configured account may resolve credentials and model directories elsewhere. |
| Medium | Rollout does not preserve a server's installed Hopper selection. | Workers recover `--url` from their service; servers receive `ROLLOUT_URL` or the Makefile's site default instead of their existing `--hopper` list. An ordinary rollout can replace a custom Hopper pair or an intentional opt-out. |

The Linux unit also omits Scan's explicit `LimitNOFILE=1048576` and worker
`RUST_BACKTRACE=1`. The shared `scan::runtime::install()` still raises the
process's soft open-file limit to its inherited hard limit, so the missing
unit setting does not by itself leave the running process at 1024. The live
unit's configured hard limit was 524288 during this audit.

One retained limitation also matters: direct deployment checks process
liveness rather than API readiness. Three seconds of `systemctl is-active`
or rc.d `status` can succeed while resources are still loading. Scan's direct
scripts also used service status; this is not a new parity regression. The
fleet rollout has a separate API health check.

Source locations: `scripts/deploy.sh`, `scripts/deploy-freebsd.sh`,
`scripts/deploy-windows.ps1`, and `scripts/rollout.sh`; compared with Scan's
`scripts/server/server-linux.sh`, `scripts/server/server-freebsd.sh`,
`scripts/worker/worker-linux.sh`, `scripts/worker/worker-freebsd.sh`, and
`scripts/worker/worker-windows.ps1`. Removed platforms, separate tunnel
deployment, and CLI-owned behavioral defaults are documented design choices.

## The binary owns its defaults

Deployment passes the hopper URL and installs credentials as files. The API
credential used by Beamline and the hopper credential are separate secrets
and must remain separate during the Scan/Postdoc fleet transition.

```
worker   worker --url <hopper>              # reads <state>/.tok/hopper
serve    serve --token-file <state>/.tok/scan [--hopper <hopper>]
```

Everything else — slot count, memory ceiling, nice value, bind address, body
limit, analysis timeout, follow policy, operating point — is postdoc's own
default, stated once in the binary and visible from `postdoc <mode> --help`.

This is deliberate, and it is the thing scan's deployment got wrong. There,
the same setting was written in the Makefile, in a shell variable, in a
systemd unit and in the binary, with different values on different platforms.
`--max-rss-gb` alone was `-1` under systemd, `0` on FreeBSD, and unset on
macOS and Windows, each for a defensible reason that nobody could see from any
one of those places. A default stated in four places is a default that
disagrees with itself, and the disagreement surfaces as one host behaving
unlike its neighbour.

The same rule applies inside the binary: `--workers`, `--max-rss-gb` and
`--traits-dir` apply to both modes, so they are declared once and shared, not
written twice.

For deployment, `~/.tok/scan` is installed as `<state>/.tok/scan` and passed
to `serve --token-file`; it stays the API token Beamline supplies as
`SCAN_TOKEN`. `SCAN_TOKEN_FILE` overrides its source path. The separate
`~/.tok/hopper` is installed as `<state>/.tok/hopper` for worker/server hopper
calls; `HOPPER_TOKEN_FILE` overrides its source path (`TOKEN_FILE` remains a
legacy alias on Unix). A server deploy requires the API token; a hopper token
is required only when that service will call hopper.

Unix deployments also copy `~/.tok/llm`, when present, into the service's home
with mode 0600. `LLM_TOKEN_FILE` overrides its source path. The server and its
idle child use this credential when the configured LLM endpoint requires one.

## The three platforms are treated alike

Same flags, same defaults, same behaviour. Scan diverged on three: `--interpret`
was passed everywhere except FreeBSD, `--max-rss-gb` took a different value per
platform, and `--hopper` came from `rc.conf` on FreeBSD but `ExecStart` on
Linux.

postdoc drops the divergence. The LLM is configured through `SCAN_LLM`, which
works identically everywhere, so `--interpret` — already deprecated in favour
of `--llm` — is never passed. Only supervision differs, because systemd, rc.d
and NSSM are different things.

### What each platform needs that the others do not

**FreeBSD** runs on libc's jemalloc rather than the one linked into the
binary, so the allocator is tuned in the unit instead:
`MALLOC_CONF=dirty_decay_ms:1000,muzzy_decay_ms:0,abort_conf:true,junk:false`,
injected through `/usr/bin/env` so it survives `daemon(8)`'s user switch and
`login.conf` filtering. Two of those values carry measurements. Adding
`background_thread:true` wedges the process: libc's jemalloc is built without
`JEMALLOC_BACKGROUND_THREAD`, init fails late enough to leave
`malloc_initialized()` false forever, and every allocation then serializes on
the global init lock — measured as a 128-core host at 14-28 cores busy with
zero completions for 14h27m. `junk:false` turns off fill-on-malloc and
fill-on-free, worth 20% and 2 GiB of peak RSS.

FreeBSD also needs the nice value applied twice. `daemon(8) -u` switches user
with `setusercontext(3)`, which applies the login class's priority — 0 — after
rc's `nice(1)` has run, so the supervisor keeps the value and the child it
forks does not. The rc.d script re-applies it to the child with an absolute
`renice`, not `renice -n`, which on FreeBSD is an increment and would compound
on every restart. And it uses `protect(1)` for `P_PROTECTED` where Linux uses
`OOMScoreAdjust`.

**Windows** has no jemalloc here; postdoc links mimalloc for the same reason,
and routes tree-sitter's C allocator through it. The CRT heap measured 12%
exclusive with rayon workers convoying on it, which is the contention jemalloc
is chosen to avoid elsewhere.

## Deploying stands atomscan down

Every deploy, on all three platforms, disables and stops scan's services before
it installs postdoc. `make rollout` inherits this because it runs the same
`make deploy-worker` / `make deploy-server` on each host, and so does a deploy
run by hand.

| Platform | Stood down | How |
| --- | --- | --- |
| Linux | `scan-worker.service`, `scan-server.service`, `scan.service`, `scan-idle.service`, `ascan-worker.service` | `systemctl disable`, `stop`, then remove local unit files |
| FreeBSD | `scan-worker`, `scan-server`, `scan`, `scan-idle`, `ascan-worker` | `sysrc *_enable=NO`, `service onestop`, then remove rc.d scripts |
| Windows | `scan-worker` | `Set-Service -StartupType Disabled` then `nssm stop` |

Windows has one service because scan has no Windows server. Unix deployment
also handles `ascan-worker`, the worker's pre-rename name, and server aliases
and companions that may remain from older installs.

They cannot share a host. Each sizes its memory ceiling against the whole
machine and each runs at nice -20, so a box running both carries two analysis
daemons that each believe they own it. They claim from the same queue and reach
the same memory ceiling together, which is how a host that survived either one
alone gets OOM-killed running the pair.

Three details carry the weight:

**Scan stops before postdoc starts.** The overlap is the dangerous window. The
gap is not, because hopper re-leases anything left unfinished.

**It is a hard gate.** If a scan service will not stop, the deploy dies rather
than starting postdoc beside it, which would produce exactly the collision the
step exists to prevent.

On FreeBSD and Windows that check reaches past the service, because on both a
stop can leave an `atomscan` process behind. FreeBSD's `daemon(8)` pidfile names
the supervisor, not the child it forked, so a stop that times out leaves the
analysis process running and still holding leases. On Windows nssm kills the
tree it supervises, but a worker started by hand or orphaned by an earlier crash
sits outside it. Both reap the straggler the way scan's own uninstaller does —
FreeBSD by process name, Windows by command line so an interactive `atomscan
scan` is untouched — and only on a host where a scan service was actually stood
down. An `atomscan` someone is running by hand elsewhere is left alone.

**Legacy services stay retired.** Linux deployment also removes their unit
files or symlinks from `/etc/systemd/system`, including disabled or static
units. Vendor unit files stay installed but disabled. Scan binaries and data
are retained. FreeBSD removes the corresponding scripts from
`/usr/local/etc/rc.d` after confirming their supervisors and leftover atomscan
processes stopped. Windows keeps the service registered with startup disabled.

This happens on every deploy path, including the one the rollout drives, so
there is no way to install postdoc on a host and leave scan running beside it.

## Memory

postdoc resolves its own ceiling, `min(50% RAM, 32 GiB)`, and sheds work when
it reaches it. The unit sets `MemoryMax=80%` as a hard backstop. The two are
layered on purpose: a soft gate that refuses new work first, a kernel kill
well above it that a `Restart=always` recovers from.

**One consequence worth a decision.** Scan passed `--max-rss-gb -1` under
systemd to turn the soft gate off entirely, reasoning that in-process
throttling "would only turn a leak into a paused worker instead of a restart".
That is real: a genuine leak now parks postdoc at 50% rather than growing to
80% and being restarted, and nothing here notices a parked worker. If that
trade is wrong, `--max-rss-gb -1` restores scan's behaviour — but it should be
set in `scripts/deploy.sh`, in one place, not per platform.

## The unit

`scripts/deploy.sh` owns the systemd unit, and that is where the measured
settings live: `MemoryMax`/`MemoryLow`, `OOMScoreAdjust=-900` with
`ManagedOOMPreference=avoid`, `Nice=-20` applied before the privilege drop so
analysis children inherit it, `CPUWeight`/`IOWeight` at maximum but
deliberately *not* realtime — a CPU-bound analysis at realtime priority can
starve sshd and lock the box out. Plus the sandbox: an empty capability
bounding set, `SystemCallFilter=@system-service`, `ProtectSystem=strict`.

`Delegate=yes` and the `ProtectControlGroups` dance are for `serve` only,
which needs a freezable cgroup subtree for its companion idle worker.

## What the process itself carries

Not deployment configuration, but the reason a deployment behaves. postdoc
declares the same jemalloc allocator as atomscan, configured from the same
`cleave::JEMALLOC_CONF` string, and routes tree-sitter's C allocator through
it. It calls `scan::runtime::install()` first thing in `main` for the signal
mask, the ptrace permission and the environment the analysis crates read.

Running this workload on the system allocator is not a small difference.
Rayon workers convoy on the process heap and freed pages are retained rather
than returned. A worker whose memory behaviour diverges from the one the fleet
was sized against is a worker that gets OOM-killed on a host the old one
survived.

## Rollout

`make rollout` redeploys the fleet in three phases. `DRY_RUN=1` prints the
plan and stops; `HOSTS="a b c"` skips discovery.

Ordering is the point:

1. **Hopper, and it is a hard gate.** If the queue does not come back, no
   worker or server is touched. A fleet redeployed against a dead queue is a
   fleet that claims nothing and looks fine doing it.
2. **Workers, in batches.** Losing some of them costs throughput and nothing
   else. `BATCH=0` does all at once. Authentication is serialized inside a
   batch and the deploys are not, which is what SSH multiplexing buys: one
   touch of a hardware key per host, not per command.
3. **Servers, one at a time, each gated before the next.** They answer live
   traffic. `SERVER_BATCH=0` deploys them together and skips the gate.

**Discovery** reads hopper's `workers` table for anything that checked in
within `DAYS`. A server is told from a worker by the `-idle` suffix its
embedded companion worker registers under — the server spawns it via
`current_exe`, so a postdoc server's idle worker is a postdoc worker and the
convention carries. The host running the rollout is always skipped; it would
be cutting the connection driving it.

**What runs on a host is decided by what is installed there**, not by the
roster: `/etc/systemd/system/postdoc-worker.service`,
`/usr/local/etc/rc.d/postdoc_worker`, and the server equivalents. A host can
be reimaged or repurposed between check-ins, and the unit on disk is the only
thing that knows what it is now.

Windows keeps no unit file, so the service manager is asked instead:
`sc.exe query postdoc-worker`. Worker only, the way scan's rollout has it,
since scan has no Windows server to have modelled one on. A Windows host needs
an ssh server and a POSIX `sh` for the rollout to drive it, which is the same
thing scan's rollout assumes.

It also needs that ssh session to be elevated, because registering a service
and writing under `Program Files` both require it. `deploy-windows.ps1` refuses
with `run this from an elevated prompt` rather than half-installing, so a host
whose session is not elevated shows up as FAILED with that line in the summary's
log tail. It does not self-elevate: `Start-Process -Verb RunAs` needs an
interactive desktop and would hang an ssh session waiting for a consent dialog
nobody can see.

A worker's hopper URL is re-read out of its own unit rather than taken from the
roster, so a worker pointed at a different queue stays pointed at it. On
Windows that means `nssm get postdoc-worker AppParameters`, whose answer is
UTF-16 and has its NULs stripped before anything tries to tokenize it.

**The health gate asks two questions**, not one. Answering `/_/health` proves
the port is bound; a small `uptime_secs` proves it is the *new* process
answering. A server that failed to restart answers happily with the old
binary, and that is exactly the failure worth catching before touching the
next one.

Each host gets a timeout with a killer subshell, so one wedged deploy cannot
hold the rollout. The summary table exits non-zero if any host is not `ok`.

**Not carried over** from scan's version: the Steam Deck binary push, the
ad-hoc macOS launch, and the beamline pinned-query check that asks a known
coordinate for a known verdict after a rollout. That last one is a genuinely
good end-to-end proof and is worth adding back if you want it.

## Other platforms

**Dropped.** Bastille jails, macOS launchd, Alpine and OpenBSD cron,
OmniOS SMF and Debian-over-SSH are all dropped. Scan carries deploy paths for
them; postdoc supports Linux, FreeBSD and Windows and nothing else.

The service names still differ — `postdoc` and `postdoc-worker` against scan's
`scan` and `scan-worker` — but they no longer coexist. See below.
