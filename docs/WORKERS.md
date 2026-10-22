# Postdoc workers

`postdoc worker` is the pull-based counterpart to `postdoc serve`. It
exposes no HTTP. It polls a [hopper](https://github.com/atomdrift-project/hopper)
instance for jobs, analyses each file, and posts the result back.

The classifier, thresholds, and response shape are identical to the
server. See [SERVER_API.md](SERVER_API.md) for the server, and
[JSON.md](JSON.md) for the JSON envelope every worker
result carries.

## Running a worker

    postdoc worker --url http://hopper-host:8081

`--url` is required. Everything else has a sensible default.

| Flag             | Default          | Meaning                                                                  |
| ---------------- | ---------------- | ------------------------------------------------------------------------ |
| `--url`          | (required)       | Hopper base URL.                                                         |
| `--name`         | hostname         | Worker identity reported to hopper.                                      |
| `--workers` (`-j`) | auto           | Concurrent analysis slots.                                               |
| `--poll-secs`    | `2`              | Sleep between polls when the queue is empty.                             |
| `--max-rss-gb`   | `0` (auto)       | RSS ceiling. Auto = 85 % of total system RAM. `-1` disables.             |
| `--data-dir`     | none             | Local file root. When set, the worker reads files locally instead of fetching them; SHA-256 is verified before use. |
| `--max-jobs`     | unlimited        | Exit after this many jobs. Useful for cron-style batches.                |
| `--traits-dir`   | none             | Writable cleave traits directory. Cloned on first run if missing.        |
| `--nice`         | `18`             | `nice(2)` value applied at startup. `0` leaves priority unchanged.       |
| `--no-update`    | off              | Skip the startup model/traits `git pull`; run against on-disk rules.    |
| `--no-validate`  | off              | Skip the strict startup trait-validation gate (dev/benchmark runs).      |
| `--exit-if-empty`| off              | Exit cleanly once hopper reports no work and the prefetch queue drains.  |

The same environment variables apply as for the server:
`CLEAVE_TRAITS_DIR`, `CLEAVE_RAYON_THREADS`, `SCAN_MODELS_REPO`.

### Authenticating to hopper

Hopper requires `Authorization: Bearer <token>` on every API route, so a
worker needs the token before it can claim work. The same token authenticates
`scan <path> --hopper` / `--upload` and `serve --hopper`, which renew results
over the same API. It is attached to every call: `/api/next`,
`/api/heartbeat`, `/api/result`, `/api/upload`, `/api/known`, `/api/file`,
`/api/provenance`, and `/data/`.

It is resolved once per process, in this order:

1. `$HOPPER_TOKEN` — the token itself, for callers that inject it some other way.
2. `$HOPPER_TOKEN_FILE` — a path to the token file.
3. `~/.tok/hopper` in the process's own home — the default, and what the
   deploy scripts install.

Loopback is not exempt on hopper's side, so a worker supervised on the hopper
host authenticates exactly like a remote one. The startup log names the source
it resolved (never the secret); with no token it warns that every request will
be rejected with 401.

The supported deploy scripts install the token as `<state>/.tok/hopper` with
mode 0600 on Unix. `HOPPER_TOKEN_FILE=/path/to/token` selects another source;
`TOKEN_FILE` remains a legacy alias on Unix. See [DEPLOY.md](DEPLOY.md) for
the Linux, FreeBSD, and Windows deploy commands. Fleet rollout is handled by
`make rollout`; each host needs its own token source file.

Rotation is an edit plus a worker restart — the token is read once per
process. Re-running the installer with a changed token restarts the service on
its own.

A worker on the hopper host used to pick the token up from
`/usr/local/etc/hopper/env` (as `HOPPER_UPLOAD_TOKEN`, later `HOPPER_TOKEN`).
That path is gone: `~/.tok/hopper` is the only file the worker reads.

## Job lifecycle

1. **Claim.** The worker calls hopper to claim a job. The job carries
   the file path (relative to `--data-dir`, if set) or a download URL,
   plus the expected SHA-256.
2. **Fetch.** If `--data-dir` is set and the file exists there, the
   worker hashes it and uses it directly. Otherwise it downloads.
   Mismatched SHA-256 fails the claim.
3. **Analyse.** Same `classify_file` / `classify_bytes` path used by
   the server.
4. **Report.** The worker posts the [response envelope](JSON.md)
   back to hopper. The shape is the same as the server's `/analyze`
   response.
5. **Sleep.** If the queue was empty on the last claim, the worker
   sleeps `--poll-secs` before trying again.

## Resource discipline

The worker is designed to share a host with other work.

- **Nice.** The default `--nice 18` keeps analysis bursts from
  starving interactive processes. Unprivileged callers can only raise
  the nice value, never lower it; pass `0` when profiling.
- **RSS ceiling.** When current RSS exceeds the limit, the worker
  *pauses claims* — it does not abort jobs in flight. It will resume
  once memory drops. Auto-resolves to 85 % of total system RAM. Set
  `-1` only behind an external supervisor (`MemoryMax=`, jail
  `memoryuse`, cgroup, etc.) that already enforces a hard cap.
- **Concurrency.** `--workers` is the hard limit on simultaneous
  analyses. There is no queue beyond hopper itself.
- **Rayon pool.** The global analysis pool uses Rayon's default sizing and
  stacks. Set `RAYON_NUM_THREADS` before startup to override its width.
  Use the same pool settings in throughput benchmarks and deployment.

## Operations

- **Idempotent deploys.** The worker tolerates repeated launches and
  trait/model directories that already exist; it does not re-clone if
  the working tree is current.
- **Service deploys.** `make deploy-worker URL=...` installs and supervises the
  Postdoc worker on Linux, FreeBSD, or Windows. `make rollout` redeploys the
  fleet in batches.

## Security

The worker is a client of hopper. It does not listen on any port.

- **Outbound only.** The trust boundary is the hopper URL; treat it
  the way you treat the rest of your service mesh.
- **File integrity.** Both download and local-file paths verify
  SHA-256 before analysis. A worker will not analyse a file whose
  hash does not match the job claim.
- **No code execution.** Same static-analysis-only guarantees as the
  server. See [SERVER_API.md#security](SERVER_API.md#security).
- **Worker identity.** `--name` is purely identifying. The bearer token
  is what hopper authenticates; it is shared by the whole fleet and
  says "a worker", not "which worker".
