SHELL := /bin/sh
# postdoc — worker and server coordination for the Atomdrift analysis fleet.
# The cargo package, library, and binary all share the name `postdoc`.
BINARY = postdoc

# Scrub GNU make's jobserver from cargo's environment, so build scripts that
# spawn their own `make` (e.g. tikv-jemalloc-sys, pulled in transitively via
# cleave) don't inherit a malformed MAKEFLAGS. Mirrors ../scan and ../isomer.
CARGO = env -u MAKEFLAGS -u MAKELEVEL -u MFLAGS cargo

# The site's LLM endpoint, passed as SCAN_LLM to every mode that runs one.
#
# This is a coordinate, not a default: postdoc cannot know where the vLLM
# lives, the same way it cannot know where hopper lives. Behaviour defaults
# belong in the binary; addresses belong here, once.
#
# The scheme is required — anything that is not `local` or `openrouter` is used
# as a base URL verbatim, and a bare host:port fails in the HTTP client.
# A comma-separated list is a failover chain tried in order, so put the
# endpoint you would rather pay for last.
LLM ?= https://llm.isotope13.ai/v1,http://10.9.8.14:8000/v1,openrouter
# Unset lets postdoc pick whatever the endpoint reports it serves.
LLM_MODEL ?=

# Match Scan's server deployment: reads prefer the replica, while the idle
# worker claims from the final (primary) address. URL= remains an override for
# deploy-server as well as the required address for standalone workers.
HOPPER ?= https://hops-ro.isotope13.ai,https://hops.isotope13.ai
SERVER_HOPPER = $(if $(strip $(URL)),$(URL),$(HOPPER))

.PHONY: all build release quick install lint fix test test-bash-scanner install-precommit clean help

all: build

build:
	$(CARGO) build --locked

release:
	@if [ "$${POSTDOC_DEPLOY_BUILD:-}" = 1 ]; then . ./scripts/deploy-bootstrap.sh; fi; \
	  $(CARGO) build --locked --release

# Optimized but un-LTO'd, for targets that build then immediately run the
# binary. See [profile.quick] in Cargo.toml.
quick:
	$(CARGO) build --locked --profile quick

# Same cache policy and verdict checks as scan's server benchmark. Profiling
# and timing are separate runs: allocator sampling has a measurable cost.
SERVER_BENCH_PATH ?= $(HOME)/data/benchmark/realworld-small
SERVER_BENCH_OUT ?= out/server-bench
SERVER_BENCH_PORT ?= 49997
SERVER_BENCH_WORKERS ?= 20
BENCH_BINARY ?= target/release/postdoc
BENCH_FETCH ?= all

.PHONY: server-benchmark server-heap-benchmark
server-benchmark:
	SCAN_NO_ANALYSIS_CACHE=1 SCAN_FETCH="$(BENCH_FETCH)" SCAN_NO_UPDATE_CHECK=1 \
	  sh scripts/bench-serve.sh "$(BENCH_BINARY)" "$(SERVER_BENCH_PATH)" \
	  "$(SERVER_BENCH_OUT)" "$(SERVER_BENCH_PORT)" "$(SERVER_BENCH_WORKERS)"

server-heap-benchmark:
	BENCH_HEAP=1 $(MAKE) --no-print-directory server-benchmark

# The mock hopper exercises downloads too; --data-dir would bypass the buffers
# whose residency matters to production. Results include a hashed manifest,
# verdict fingerprint, RSS samples, and per-size-class latency/throughput.
WORKER_BENCH_PATH ?= $(SERVER_BENCH_PATH)
WORKER_BENCH_OUT ?= out/worker-bench
WORKER_BENCH_WORKERS ?= 16
WORKER_BENCH_FETCH ?= none
WORKER_BENCH_MAX_RSS_GB ?= -1

.PHONY: bench-hopper worker-benchmark worker-heap-benchmark
bench-hopper:
	$(CARGO) build --locked --release --features bench-hopper --bin postdoc-bench-hopper

worker-benchmark: bench-hopper
	python3 scripts/bench-worker.py "$(WORKER_BENCH_PATH)" "$(WORKER_BENCH_OUT)" \
	  --binary "$(BENCH_BINARY)" --workers "$(WORKER_BENCH_WORKERS)" --fetch "$(WORKER_BENCH_FETCH)" \
	  --max-rss-gb "$(WORKER_BENCH_MAX_RSS_GB)"

worker-heap-benchmark: bench-hopper
	python3 scripts/bench-worker.py "$(WORKER_BENCH_PATH)" "$(WORKER_BENCH_OUT)" \
	  --binary "$(BENCH_BINARY)" --workers "$(WORKER_BENCH_WORKERS)" --fetch "$(WORKER_BENCH_FETCH)" \
	  --max-rss-gb "$(WORKER_BENCH_MAX_RSS_GB)" --heap

install: release
	$(CARGO) install --path .

# Two gates, cheapest first. `--check` (not `--all`) keeps rustfmt inside this
# package: `cargo fmt --all` also reformats local path dependencies, which would
# drag ../scan and ../isomer into postdoc's lint run. `--all-targets` puts tests
# under the same lints as the library — clippy.toml relaxes only the panic lints
# there. `--locked` fails on a stale Cargo.lock instead of quietly rewriting it,
# so a lint run can't move a pinned git dep.
lint:
	$(CARGO) fmt --check
	$(CARGO) clippy --locked --all-targets -- -D warnings

# Auto-fix what clippy and rustfmt can fix on their own; fmt last so it tidies
# any code clippy rewrote. Same target set as `lint`.
fix:
	$(CARGO) clippy --locked --fix --all-targets --allow-dirty --allow-staged
	$(CARGO) fmt

test: test-bash-scanner
	$(CARGO) test --locked --quiet

# Exercise the scanner Cargo actually resolves, including its Unicode fix.
test-bash-scanner:
	@set -e; tmp=$$(mktemp); trap 'rm -f "$$tmp"' EXIT; \
	  $(CARGO) metadata --locked --format-version 1 > "$$tmp"; \
	  scanner_src=$$(python3 -c 'import json, pathlib, sys; p = next(p for p in json.load(sys.stdin)["packages"] if p["name"] == "tree-sitter-bash"); print(pathlib.Path(p["manifest_path"]).parent / "src")' < "$$tmp"); \
	  $${CC:-cc} -O2 -I"$$scanner_src" \
	    tests/fixtures/bash-scanner-repro.c -o "$$tmp"; "$$tmp"

# Install the pre-commit gate (no path overrides + make test + make lint).
# Bypass an individual commit with `git commit --no-verify`.
install-precommit:
	cp scripts/pre-commit "$$(git rev-parse --git-dir)/hooks/pre-commit"
	chmod +x "$$(git rev-parse --git-dir)/hooks/pre-commit"
	@echo "✓ Pre-commit hook installed."

# --- Deployment --------------------------------------------------------------
#
# Three platforms, one contract: the deploy passes the hopper URL and installs
# the distinct API and hopper credentials; the binary defaults the rest. What
# differs per platform is supervision (systemd, rc.d, NSSM) and
# the allocator tuning FreeBSD needs in its unit because it uses libc's
# jemalloc rather than the one linked into the binary.

.PHONY: worker deploy-worker deploy-server check-server-hopper test-deploy rollout

deploy-worker deploy-server: export POSTDOC_DEPLOY_BUILD := 1

test-deploy:
	python3 scripts/test-deploy-deps.py
	python3 scripts/test-deploy.py
	python3 scripts/test-rollout.py
	@if command -v pwsh >/dev/null 2>&1; then pwsh -NoLogo -NoProfile -File scripts/test-deploy-windows.ps1; fi

## worker: run a worker in the foreground, as you, until Ctrl-C
##         No service, no state directory — it reads ~/.tok/hopper directly.
##         LLM= turns the interpreter off; LLM=<endpoint> points it elsewhere.
worker: release
	@[ -n "$(URL)" ] || { echo "usage: make worker URL=<hopper url>"; exit 1; }
	@[ -r "$$HOME/.tok/hopper" ] || { \
	  echo "error: no hopper token at ~/.tok/hopper; every claim would 401" >&2; exit 1; }
	SCAN_LLM="$(LLM)" $(if $(LLM_MODEL),SCAN_LLM_MODEL="$(LLM_MODEL)",) \
	  ./target/release/$(BINARY) worker --url "$(URL)"

## deploy-worker: install and start a worker here (URL=<hopper url>)
deploy-worker: release
	@[ -n "$(URL)" ] || { echo "usage: make deploy-worker URL=<hopper url>"; exit 1; }
	@$(MAKE) --no-print-directory _deploy MODE=worker URL="$(URL)"

## deploy-server: install and start a server here (HOPPER=<read,primary> or URL=)
##                HOPPER=none explicitly disables uploads and idle work.
deploy-server: check-server-hopper release
	@$(MAKE) --no-print-directory _deploy MODE=serve URL="$(if $(filter none,$(SERVER_HOPPER)),,$(SERVER_HOPPER))"

check-server-hopper:
	@[ -n "$(strip $(SERVER_HOPPER))" ] || { \
	  echo "error: no server Hopper URL; set HOPPER=<url> or HOPPER=none to disable uploads and idle work" >&2; \
	  exit 1; }

## rollout: redeploy the fleet — hopper, then workers, then servers
##          DRY_RUN=1 prints the plan; HOSTS="a b" skips discovery
rollout:
	./scripts/rollout.sh

_deploy: export LLM := $(LLM)
_deploy: export LLM_MODEL := $(LLM_MODEL)
_deploy:
	@case "$$(uname -s)" in \
	  Linux)   ./scripts/deploy.sh "$(MODE)" "$(URL)" "target/release/$(BINARY)" ;; \
	  FreeBSD) ./scripts/deploy-freebsd.sh "$(MODE)" "$(URL)" "target/release/$(BINARY)" ;; \
	  MINGW*|MSYS*|CYGWIN*) \
	           if command -v pwsh >/dev/null 2>&1; then ps=pwsh; else ps=powershell; fi; \
	           "$$ps" -NoProfile -ExecutionPolicy Bypass \
	             -File scripts/deploy-windows.ps1 -Mode "$(MODE)" -Url "$(URL)" ;; \
	  *) echo "error: no deploy path for $$(uname -s); supported: Linux, FreeBSD, Windows" >&2; \
	     exit 1 ;; \
	esac

clean:
	$(CARGO) clean

help:
	@echo "postdoc targets:"
	@echo "  build     debug build (optimized dependencies)"
	@echo "  release   optimized build"
	@echo "  quick     optimized build, no LTO — fast to link"
	@echo "  install   cargo install to ~/.cargo/bin"
	@echo "  lint      rustfmt --check + clippy with warnings denied"
	@echo "  fix       auto-fix clippy + rustfmt"
	@echo "  test      run the test suite"
	@echo "  test-deploy  check runtime prerequisite installation with mock package managers"
	@echo "  install-precommit  gate commits on lint + test"
	@echo "  worker         run a worker in the foreground (URL=<hopper url>)"
	@echo "  server-benchmark / server-heap-benchmark   HTTP corpus benchmark (SERVER_BENCH_PATH=)"
	@echo "  worker-benchmark / worker-heap-benchmark   local hopper benchmark (WORKER_BENCH_PATH=)"
	@echo "  deploy-worker  install + start a worker here (URL=<hopper url>)"
	@echo "  deploy-server  install + start a server here"
	@echo "  rollout        redeploy the fleet (DRY_RUN=1 to preview)"
	@echo "  clean     cargo clean"
