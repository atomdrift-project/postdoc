#!/bin/sh
# build-malcontent.sh — build malcontent's `mal` from source, together with the
# yara-x C API it cgo-links against.
#
# Copied from gauntlet, which builds `mal` this way where it cannot run the
# chainguard container image. postdoc's deploy builds it this way on Linux and
# FreeBSD (scripts/deploy-scanners.sh): malcontent publishes no packages for
# either, and a container would need a runtime inside a hardened service.
#
# Two marker-guarded stages, because the first is expensive:
#   1. yara-x's C API (Rust + LTO, ~11min) — rebuilt only when malcontent moves
#      its pinned YARA_X_COMMIT, which happens a few times a year.
#   2. cmd/mal (cgo, ~1min) — rebuilt on every new malcontent release.
# A cycle that finds neither changed costs one `git fetch`.
#
# Installation is atomic and verified: the fresh binary must answer --version
# before it replaces the one in use, so a broken upstream leaves the
# last good `mal` in place rather than knocking the scanner out.
#
# Usage: build-malcontent.sh
# Env:
#   MALCONTENT_SRC     source checkout       (default $HOME/malcontent-src)
#   MALCONTENT_REMOTE  git URL               (default chainguard-dev/malcontent)
#   MALCONTENT_REF     tag/branch to build   (default: newest v* tag, matching
#                                             what the :latest image tracks)
#   MAL_BIN            install path          (default $HOME/.local/bin/mal)
set -eu

SRC="${MALCONTENT_SRC:-$HOME/malcontent-src}"
REMOTE="${MALCONTENT_REMOTE:-https://github.com/chainguard-dev/malcontent.git}"
MAL_BIN="${MAL_BIN:-$HOME/.local/bin/mal}"
OUT="$SRC/out"

log() { printf '[build-malcontent] %s\n' "$*"; }
die() { printf '[build-malcontent] error: %s\n' "$*" >&2; exit 1; }

for tool in git go cargo cargo-cbuild; do
	command -v "$tool" >/dev/null 2>&1 || die "missing ${tool} (need git, go, rust and cargo-c)"
done

# --- Source ------------------------------------------------------------------

if [ -d "$SRC/.git" ]; then
	log "fetching ${REMOTE}"
	git -C "$SRC" remote set-url origin "$REMOTE"
	git -C "$SRC" fetch --prune --tags origin
else
	log "cloning ${REMOTE} -> ${SRC}"
	git clone "$REMOTE" "$SRC"
fi

# The chainguard :latest image is built from releases, not from main, so track
# the newest release tag by default to keep the two deployments comparable.
REF="${MALCONTENT_REF:-}"
if [ -z "$REF" ]; then
	REF=$(git -C "$SRC" tag -l 'v[0-9]*' --sort=-v:refname | head -1)
	[ -n "$REF" ] || die "no v* release tag found in ${SRC}"
fi
log "building ref ${REF}"
git -C "$SRC" checkout --quiet --force "$REF"
MALCONTENT_COMMIT=$(git -C "$SRC" rev-parse HEAD)

# --- yara-x C API ------------------------------------------------------------

# malcontent pins the yara-x revision its bindings expect in its own Makefile;
# read it from there rather than duplicating the pin here, so an upstream bump
# is picked up by the fetch above.
YARA_X_REPO=$(sed -n 's/^YARA_X_REPO[[:space:]]*?*=[[:space:]]*//p' "$SRC/Makefile" | head -1)
YARA_X_COMMIT=$(sed -n 's/^YARA_X_COMMIT[[:space:]]*?*=[[:space:]]*//p' "$SRC/Makefile" | head -1)
[ -n "$YARA_X_REPO" ] && [ -n "$YARA_X_COMMIT" ] \
	|| die "could not read YARA_X_REPO/YARA_X_COMMIT from ${SRC}/Makefile"

YARA_X_SRC="$OUT/$YARA_X_REPO"
YARA_X_MARKER="$OUT/.yara-x-commit"

if [ -r "$YARA_X_MARKER" ] && [ "$(cat "$YARA_X_MARKER")" = "$YARA_X_COMMIT" ] \
	&& [ -f "$OUT/include/yara_x.h" ]; then
	log "yara-x C API already current (${YARA_X_COMMIT})"
else
	log "building yara-x C API (${YARA_X_COMMIT}) — this takes ~11 minutes"
	mkdir -p "$YARA_X_SRC"
	if [ -d "$YARA_X_SRC/.git" ]; then
		git -C "$YARA_X_SRC" fetch --prune origin
	else
		git clone "https://github.com/${YARA_X_REPO}.git" "$YARA_X_SRC"
	fi
	git -C "$YARA_X_SRC" checkout --quiet --force "$YARA_X_COMMIT"
	rm -f "$YARA_X_MARKER"
	# Flags mirror malcontent's own `install-yara-x` target: the capi crate, its
	# native-code-serialization feature (malcontent caches compiled rulesets), and
	# a static library so the resulting `mal` carries yara-x with it and doesn't
	# depend on this tree at runtime.
	( cd "$YARA_X_SRC" && cargo cinstall -p yara-x-capi \
		--features=native-code-serialization \
		--profile release-lto \
		--prefix="$OUT" --libdir="$OUT/lib" --library-type=staticlib )
	printf '%s\n' "$YARA_X_COMMIT" > "$YARA_X_MARKER"
fi

# cargo-c follows the platform's convention for where .pc files belong:
# $prefix/libdata/pkgconfig on FreeBSD, $prefix/lib/pkgconfig elsewhere. Find it
# rather than assuming, which is what malcontent's own Makefile gets wrong here.
PKGCONF_DIR=$(dirname "$(find "$OUT" -name yara_x_capi.pc -print -quit 2>/dev/null)" 2>/dev/null || true)
[ -n "$PKGCONF_DIR" ] && [ -d "$PKGCONF_DIR" ] \
	|| die "no yara_x_capi.pc under ${OUT} — the cargo cinstall above did not complete"

# --- mal ---------------------------------------------------------------------

REV_MARKER="${MAL_BIN}.revision"
if [ -x "$MAL_BIN" ] && [ -r "$REV_MARKER" ] \
	&& [ "$(sed -n '1p' "$REV_MARKER")" = "$MALCONTENT_COMMIT" ]; then
	log "mal already current (${REF}, $(echo "$MALCONTENT_COMMIT" | cut -c1-12))"
	exit 0
fi

log "building mal (${REF})"
mkdir -p "$OUT" "$(dirname "$MAL_BIN")"
# The .pc carries -L/-l and every transitive system library, so no CGO_LDFLAGS of
# our own are needed; CGO_CPPFLAGS finds yara_x.h before the compiler's defaults.
# -s -w strip the symbol table and DWARF, which is most of a Go binary's size;
# -trimpath keeps build-host paths out of it. CGO stays ON here:
# mal links yara-x's C API.
( cd "$SRC" && CGO_CPPFLAGS="-I${OUT}/include" PKG_CONFIG_PATH="$PKGCONF_DIR" \
	go build -trimpath -ldflags="-s -w" -o "$OUT/mal.new" ./cmd/mal )

# Prove the fresh binary loads its rules before it replaces a working one.
"$OUT/mal.new" --version >/dev/null 2>&1 || {
	rm -f "$OUT/mal.new"
	die "freshly built mal does not run; keeping the installed binary"
}

install -m 0755 "$OUT/mal.new" "${MAL_BIN}.new"
mv -f -- "${MAL_BIN}.new" "$MAL_BIN"
rm -f "$OUT/mal.new"
printf '%s\n' "$MALCONTENT_COMMIT" > "${REV_MARKER}.new"
mv -f -- "${REV_MARKER}.new" "$REV_MARKER"

log "installed $("$MAL_BIN" --version) -> ${MAL_BIN}"
