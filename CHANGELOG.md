# Changelog

All notable changes to postdoc. Versions follow [semantic versioning](https://semver.org/spec/v2.0.0.html).

## 0.9.5 (unreleased)

### Features

- Speak scan's version 8 result format, rather than a private one nobody else could read.
- Run ClamAV, malcontent and GuardDog on request with `--scanners`; they report beside the verdict and never change it.
- Install requested scanners on Linux and FreeBSD deploys with `SCANNERS`, and fail loudly if one will not answer.
- Return the full version 8 result instead of the decision when asked with `full=1`.
- Breaking: talk only to hopper's `/v1` routes; older hoppers need not apply.
- Breaking: analyses no longer time out by default; `--analysis-timeout` brings the limit back.
- Breaking: `serve` requires `--token-file`; there is no unauthenticated mode, and every route but `/_/health` needs the token.
- Breaking: `/_/health` answers one word (`ok`, `starting`, `failed`, `degraded`) as plain text; load and uptime are on `/_/stats`.
- Breaking: remove `/_/info`, `/_/memory`, `/_/requests`, `/_/threads` and `/_/metrik`, which no caller used; metrics are still pushed over OTLP.
- Breaking: `GET /status` is now `GET /v1/status`, beside the other per-artifact routes.

### Fixes

- Name ourselves to hopper with a user agent, since it refuses anonymous clients.
- Read a hopper record holding unanalyzed bytes as nothing known, not as a failure.

## 0.9.0 (unreleased)

First release. postdoc replaces `atomscan serve` and `atomscan worker`.

### Security

- Grant no route loopback trust; behind a tunnel, atomscan serve let remote callers read server paths and reload rules.
- Stop following hopper redirects, which let a hostile hopper probe internal addresses and read the replies back.
- Cap every hopper response body, so a hostile peer can no longer exhaust a worker's memory.
- Charge spooled downloads to the disk budget as bytes arrive, so an understated size cannot fill the disk.
- Delay wrong bearer tokens on `/_/health` too, which had given token guessers a fast oracle.
- Close connections that send no request header within 30 seconds; idle peers could hold sockets forever.
- Answer unavailable when hopper returns something other than a record, rather than failing open as unanalyzed.
- Stop echoing error chains to callers; they exposed temporary paths and internal addresses.
- Escape hopper-supplied names and Unicode line breaks in logs, which could forge log records.

### Features

- Judge each artifact with cleave, the azoth model, scan's LLM interpreter and isomer, reporting every engine's verdict.
- Diff a package release against an earlier one hopper holds, catching what a compromised release added.
- Keep the worse of scan's and isomer's outcomes, and record which engine decided it.
- Claim and judge hopper artifacts with `postdoc worker`, a drop-in for atomscan worker.
- Serve scan's existing v1 HTTP API with `postdoc serve`, a drop-in for atomscan serve.
- Accept uploads up to 1 GiB by default, so promoter's larger archives and images need no flag.
- Pick the rule feed (zero-day, supporter, oss-delayed) with `--feed`, the saved choice, or a held token, as atomscan does.
- Fail over LLM requests across an ordered chain of endpoints.
- Push server metrics over OTLP, configured by the standard OpenTelemetry environment variables.
- Reap temporary files and download spools a crashed process abandoned on the next startup.
- Deploy and roll out workers and servers on Linux, FreeBSD and Windows, including SELinux hosts.
- Stop requiring rizin on worker and server hosts.
- Answer `?url=` lookups and repeat analyses from the digest that URL last served.
- Answer lookups for known-good artifacts from the local bloom and NSRL filters, without asking hopper.
- Breaking: drop atomscan serve's legacy analyze, lookup, reload and update routes; Beamline uses only v1.
- Breaking: read the traits directory from `CLEAVE_TRAITS_DIR` instead of a flag.
- Breaking: report a failing artifact source as a retryable 503, not a 422 that marks the package gone.
- Breaking: size the analysis pool like atomscan does; `CLEAVE_RAYON_THREADS` replaces RAYON_NUM_THREADS.

### Fixes

- Patch the vendored bash scanner's Unicode lookahead, which segfaulted the production worker.
- Give analysis threads 256 MB stacks like atomscan; the 2 MiB default could crash on nested archives.
- Finish answers for 30 seconds on shutdown, then cancel the rest, instead of outlasting the supervisor.
- Classify analysis failures by what went wrong, never by the caller's filename.
- Reuse hopper's verdict on refresh only when this server's traits actually classified it.
- Never signal a reaped idle worker's pid on FreeBSD or macOS, where it may name another process.

### Optimizations

- Build releases with profile-guided optimization, which cut scan's CPU 7–10% with identical findings.
- Analyze uploads in memory on whale lanes, cutting small-file latency threefold under concurrent load.
- Hash with hardware SHA-256 on ARM, making a 600 MiB upload 3.6 times faster.
- Size upload buffers from Content-Length, cutting a 600 MiB upload's peak memory from 1 GiB.
- Skip followed dependencies that the bloom filters vouch for, instead of fetching and analyzing them.
