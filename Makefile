SHELL := /bin/sh
# postdoc — worker and server coordination for the Atomdrift analysis fleet.
# The cargo package, library, and binary all share the name `postdoc`.
BINARY = postdoc

# Scrub GNU make's jobserver from cargo's environment, so build scripts that
# spawn their own `make` (e.g. tikv-jemalloc-sys, pulled in transitively via
# cleave) don't inherit a malformed MAKEFLAGS. Mirrors ../scan and ../isomer.
CARGO = env -u MAKEFLAGS -u MAKELEVEL -u MFLAGS cargo

PACKAGE := $(shell awk -F'"' '/^name = /{print $$2; exit}' Cargo.toml)

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
LLM ?= http://10.9.8.149:8000/v1
# Unset lets postdoc pick whatever the endpoint reports it serves.
LLM_MODEL ?=

.PHONY: all build release quick install lint fix test install-precommit clean help

all: build

build:
	$(CARGO) build

release:
	$(CARGO) build --release

# Optimized but un-LTO'd, for targets that build then immediately run the
# binary. See [profile.quick] in Cargo.toml.
quick:
	$(CARGO) build --profile quick

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
	$(CARGO) clippy --fix --all-targets --allow-dirty --allow-staged
	$(CARGO) fmt

test:
	$(CARGO) test --quiet

# Install the pre-commit gate (no path overrides + make test + make lint).
# Bypass an individual commit with `git commit --no-verify`.
install-precommit:
	cp scripts/pre-commit "$$(git rev-parse --git-dir)/hooks/pre-commit"
	chmod +x "$$(git rev-parse --git-dir)/hooks/pre-commit"
	@echo "✓ Pre-commit hook installed."

# --- Deployment --------------------------------------------------------------
#
# Three platforms, one contract: the deploy passes the two things it cannot
# know — where hopper is and where the token lives — and the binary defaults
# the rest. What differs per platform is supervision (systemd, rc.d, NSSM) and
# the allocator tuning FreeBSD needs in its unit because it uses libc's
# jemalloc rather than the one linked into the binary.

.PHONY: worker deploy-worker deploy-server rollout

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

## deploy-server: install and start a server here (URL=<hopper url>, optional)
deploy-server: release
	@$(MAKE) --no-print-directory _deploy MODE=serve URL="$(URL)"

## rollout: redeploy the fleet — hopper, then workers, then servers
##          DRY_RUN=1 prints the plan; HOSTS="a b" skips discovery
rollout:
	./scripts/rollout.sh

_deploy: export LLM := $(LLM)
_deploy: export LLM_MODEL := $(LLM_MODEL)
_deploy:
	@case "$$(uname -s)" in \
	  Linux)   install -m0755 target/release/$(BINARY) /usr/local/bin/$(BINARY); \
	           ./scripts/deploy.sh "$(MODE)" "$(URL)" ;; \
	  FreeBSD) install -m0755 target/release/$(BINARY) /usr/local/bin/$(BINARY); \
	           ./scripts/deploy-freebsd.sh "$(MODE)" "$(URL)" ;; \
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
	@echo "  install-precommit  gate commits on lint + test"
	@echo "  worker         run a worker in the foreground (URL=<hopper url>)"
	@echo "  deploy-worker  install + start a worker here (URL=<hopper url>)"
	@echo "  deploy-server  install + start a server here"
	@echo "  rollout        redeploy the fleet (DRY_RUN=1 to preview)"
	@echo "  clean     cargo clean"
