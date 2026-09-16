SHELL := /bin/sh
# postdoc — worker and server coordination for the Atomdrift analysis fleet.
# The cargo package, library, and binary all share the name `postdoc`.
BINARY = postdoc

# Scrub GNU make's jobserver from cargo's environment, so build scripts that
# spawn their own `make` (e.g. tikv-jemalloc-sys, pulled in transitively via
# cleave) don't inherit a malformed MAKEFLAGS. Mirrors ../scan and ../isomer.
CARGO = env -u MAKEFLAGS -u MAKELEVEL -u MFLAGS cargo

PACKAGE := $(shell awk -F'"' '/^name = /{print $$2; exit}' Cargo.toml)

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
	@echo "  clean     cargo clean"
