# Deploying postdoc

```sh
make deploy-worker URL=https://hopper.example   # this host
make deploy-server URL=https://hopper.example   # this host, URL optional
make rollout                                    # the fleet
```

`make` picks the platform:

| Platform | Supervision | Script |
| --- | --- | --- |
| Linux | systemd | `scripts/deploy.sh` |
| FreeBSD | rc.d + daemon(8) | `scripts/deploy-freebsd.sh` |
| Windows | NSSM | `scripts/deploy-windows.ps1` |

Nothing else is supported, and the Makefile says so rather than guessing.

## The binary owns its defaults

Deployment passes exactly two things postdoc cannot work out for itself:
where hopper is, and where the bearer token lives.

```
worker   worker --url <hopper>
serve    serve --token-file <state>/.tok/hopper [--hopper <hopper>]
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
thing that knows what it is now. A worker's hopper URL is re-read out of its
own unit rather than taken from the roster, so a worker pointed at a different
queue stays pointed at it.

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

Both binaries can run on one host: the service names differ (`postdoc` and
`postdoc-worker` against scan's `scan` and `scan-worker`), which is how you
compare them on real traffic before retiring the old one.
