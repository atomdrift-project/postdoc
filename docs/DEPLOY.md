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
package managers. These tests do not install software or change host services.

## Deployment parity fixes

The 2026-10-18 audit findings are now covered by the deploy paths and tests:

- Linux and FreeBSD bootstrap missing runtime and build tools before Cargo
  runs. Rustup installs a missing Rust toolchain in the invoking user's home;
  older compilers are updated through Rustup, selecting stable for this build
  without changing an existing default toolchain.
- Explicit `BIND`, `ALLOW_CIDR`, `WORKERS`, `DATA_DIR` (workers),
  `MAX_RSS_GB`, and `IDLE` settings reach the service. `IDLE=0` disables
  background claims. Omitted values retain the binary's defaults.
- Credentials are checked for non-whitespace content before stopping services.
  Missing source files reuse valid installed credentials; an explicit blank
  rotation fails. Unix and Windows install LLM and OpenRouter keys from
  `LLM_TOKEN_FILE`, `OPENROUTER_TOKEN_FILE`, or their default `.tok` files.
  `SCAN_LLM_KEY` provisions the OpenRouter key when that endpoint appears in
  the chain, otherwise the LLM endpoint key. Secrets stay out of service argv,
  configuration manifests, and service environment blocks.
- Windows reads exported `LLM`/`LLM_MODEL`; FreeBSD persists an explicit
  `LLM=` so a previous endpoint can be turned off.
- Binary, unit/rc.d script, credential, and saved configuration contents are
  compared before restarting. An unchanged deployment leaves a running
  service alone, while a credential rotation restarts it.
- Direct server deployment waits for authenticated API readiness and, on Unix
  when enabled, a running idle worker. `HEALTH_WAIT` sets the timeout (default
  300 seconds). A running process alone does not count as a successful deploy.
- Existing Unix account homes are honored. Linux resolves relocated state
  storage and binds that home into the service sandbox, including homes
  beneath `/home`. Other users' home directories remain hidden.
- Linux explicitly sets `LimitNOFILE=1048576` and `RUST_BACKTRACE=1`.

Nonsecret deployment settings are stored as `/etc/postdoc/serve.json` or
`worker.json` on Linux, `/usr/local/etc/postdoc/` on FreeBSD, and
`deploy-serve.json` or `deploy-worker.json` in the Windows service state
home. Rollout reads these settings before redeploying. For older installs
without a manifest, it recovers literal flags from the installed service.
A server without `--hopper` is preserved as `HOPPER=none`; an explicit
rollout `URL=` overrides the saved Hopper selection.

Rollout's Hopper phase now connects to `HOPPER_NODE` (default `10.9.8.10`),
updates `$HOME/hopper`, runs `make deploy`, and checks
`HOPPER_HEALTH` (default `http://127.0.0.1:8081/healthz` on that host).
Failure to connect, deploy, or become ready blocks all analysis hosts.
`PHASES="workers servers"` explicitly skips queue deployment. No remote fleet
rollout is performed by the regression tests.

`make test-deploy` runs prerequisite, Unix deployment, and fake-SSH rollout
regressions. When `pwsh` is installed it also runs mocked Windows deployments.
The tests verify unchanged redeploys, token rotation, empty-token rejection,
configuration preservation, paths with spaces, and the failed-Hopper gate.
Native FreeBSD/Windows service-manager validation still requires those hosts.

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

On Linux and FreeBSD, `make deploy-server` and `make deploy-worker` automatically
enable the isotope13 Enriched rules when a subscription token is available:
fresh the hour they clear QA, instead of the four-hourly public roll-up.
Deployment reads `ISOTOPE13_TOKEN`, then `~/.tok/isotope13`
(`ISOTOPE13_TOKEN_FILE` overrides the source path), or reuses the service's
installed token when the source is absent. It adds `--enriched` to the service
command and installs the token as `<state>/.config/atomdrift/isotope13-api.json`,
mode 0600. The resolved `ENRICHED` setting is saved in the deployment manifest,
so a rollout keeps it. With no token, deployment uses public rules;
`ENRICHED=1` requires a token and fails deployment if none is available.
`ENRICHED=0` skips token provisioning. `--enriched` changes only which
rules postdoc analyzes with: postdoc is what isotope13's remote analysis runs
on, so it never asks isotope13 for a verdict, and `--analyze=remote` is refused.
postdoc runs the Enriched rules whenever it finds a token, so `ENRICHED=0` does
not undo an earlier `ENRICHED=1`: remove the installed `isotope13-api.json` to
go back to the public rules. A token refused at startup stops the service from
starting; one refused at a later renewal is logged at error level while the
installed rules go on serving.
Windows deployments do not install the token yet; set `ISOTOPE13_TOKEN` in the
service environment there.

## The three platforms are treated alike

Same flags, same defaults, same behaviour. Scan diverged on three: `--interpret`
was passed everywhere except FreeBSD, `--max-rss-gb` took a different value per
platform, and `--hopper` came from `rc.conf` on FreeBSD but `ExecStart` on
Linux.

postdoc drops the divergence. The LLM is configured through `SCAN_LLM`, which
works identically everywhere, so `--interpret` — already deprecated in favour
of `--llm` — is never passed. Only supervision differs, because systemd, rc.d
and NSSM are different things.

The deployment default is
`LLM=https://llm.isotope13.ai/v1,http://10.9.8.14:8000/v1,openrouter`.
Each request tries isotope13 first, the private endpoint second, and OpenRouter
last. `LLM=` disables the pass; `LLM=<chain>` overrides the endpoints.
The first two endpoints use `~/.tok/llm`; OpenRouter uses its separate
`~/.tok/openrouter` token. Both files are installed for the service when present.
Without an OpenRouter token, that endpoint is dropped from the chain.

`cargo test --locked --test llm_failover` exercises model discovery,
authentication, and ordered failover against local HTTP servers. To test each
real endpoint with the current user's credentials, run
`cargo test --locked --test llm_failover live_ -- --ignored --nocapture`.
These live tests send a small completion request to each endpoint, including
OpenRouter.

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
it reaches it. The Linux server unit sets `MemoryMax=min(80% RAM, 128 GiB)`
as a hard backstop and `MemoryLow` to half that allowance. Explicit
`MEMORY_MAX`/`MEMORY_LOW` overrides are supported. Standalone workers default
to 80% RAM without a reclaim-protected floor. The two are layered on purpose: a soft gate that refuses new work first, a kernel kill
well above it that a `Restart=always` recovers from.

**One consequence worth a decision.** Scan passed `--max-rss-gb -1` under
systemd to turn the soft gate off entirely, reasoning that in-process
throttling "would only turn a leak into a paused worker instead of a restart".
That is real: a genuine leak now parks postdoc at 50% rather than growing to
80% and being restarted, and nothing here notices a parked worker. If that
trade is wrong, `--max-rss-gb -1` restores scan's behaviour — but it should be
set explicitly with `MAX_RSS_GB=-1 make deploy-server`.

## The unit

`scripts/deploy.sh` owns the systemd unit, including `MemoryMax`/`MemoryLow`,
`OOMScoreAdjust=-900` with
`ManagedOOMPreference=avoid`, `Nice=-20` applied before the privilege drop so
analysis children inherit it, `CPUWeight`/`IOWeight` at maximum but
deliberately *not* realtime — a CPU-bound analysis at realtime priority can
starve sshd and lock the box out. Plus the sandbox: an empty capability
bounding set, `SystemCallFilter=@system-service`, `ProtectSystem=strict`.

The Linux companion keeps its kernel freeze/thaw behavior. With the deployed
resource policy, the server moves into a foreground leaf and enables delegated
CPU, IO, and memory controllers. The idle leaf gets CPU/IO weights of 1,
nice 19, idle IO scheduling, no memory protection, and its own `memory.max`.
The idle ceiling defaults to half the unit allowance; `IDLE_MEMORY_MAX` may
lower it. A tighter systemd drop-in also lowers that ceiling. An idle OOM
kills the idle group and the server supervisor restarts it.

Both leaves remain inside the unit's total memory budget. This deliberately
retains Postdoc's supervised companion and immediate freezing rather than
reintroducing Scan's independent idle service. `IDLE` also controls the child
worker's actual concurrency. Delegation is needed for server idle work;
`ProtectControlGroups` allows that delegated subtree while protecting the rest
of the hierarchy.

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

Saved settings preserve each host's Hopper URLs, LLM configuration, and explicit runtime overrides. Older installs recover Hopper URLs from their service rather than from the roster. On
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
