#!/bin/sh
# Source in the release recipe so a newly installed Rust toolchain stays on PATH.
set -eu
PATH="$HOME/.cargo/bin:/usr/local/sbin:/usr/local/bin:/usr/sbin:/usr/bin:/sbin:/bin:$PATH"
export PATH
log() { printf '==> %s\n' "$*"; }
die() { printf 'error: %s\n' "$*" >&2; exit 1; }
SUDO=""
if [ "$(id -u)" -ne 0 ]; then
    if command -v doas >/dev/null 2>&1; then SUDO=doas
    elif command -v sudo >/dev/null 2>&1; then SUDO=sudo
    else die "need doas or sudo to bootstrap deployment"; fi
fi
. ./scripts/deploy-deps.sh
case "$(uname -s)" in
    Linux|FreeBSD)
        ensure_runtime_tools "$(uname -s)"
        if [ "$(uname -s)" = FreeBSD ]; then RUNTIME_PKG=pkg
        elif command -v apt-get >/dev/null 2>&1; then RUNTIME_PKG=apt
        elif command -v dnf >/dev/null 2>&1; then RUNTIME_PKG=dnf
        elif command -v yum >/dev/null 2>&1; then RUNTIME_PKG=yum
        elif command -v zypper >/dev/null 2>&1; then RUNTIME_PKG=zypper
        elif command -v pacman >/dev/null 2>&1; then RUNTIME_PKG=pacman
        elif command -v xbps-install >/dev/null 2>&1; then RUNTIME_PKG=xbps
        else RUNTIME_PKG=none; fi
        for build_tool in git curl python3 cc c++ pkg-config; do
            command -v "$build_tool" >/dev/null 2>&1 && continue
            case "$RUNTIME_PKG:$build_tool" in
                pkg:cc|pkg:c++) die "FreeBSD's base compiler is missing" ;;
                pkg:pkg-config) build_package=pkgconf ;;
                apt:cc|apt:c++) build_package=build-essential ;;
                dnf:cc|yum:cc|zypper:cc|xbps:cc) build_package=gcc ;;
                dnf:c++|yum:c++|zypper:c++) build_package=gcc-c++ ;;
                xbps:c++) build_package=gcc ;;
                pacman:cc|pacman:c++) build_package=base-devel ;;
                pacman:python3) build_package=python ;;
                *:pkg-config) build_package=pkgconf ;;
                *) build_package=$build_tool ;;
            esac
            [ "$RUNTIME_PKG" != none ] || die "install $build_tool before deploying"
            install_runtime_package "$build_package" || die "could not install $build_package"
        done
        # Build scripts use TLS both for artifacts and for the Rust bootstrap.
        if [ ! -f /etc/ssl/cert.pem ] && [ ! -f /etc/ssl/certs/ca-certificates.crt ]; then
            ca_package=ca-certificates
            [ "$RUNTIME_PKG" != pkg ] || ca_package=ca-root-nss
            install_runtime_package "$ca_package" || die "install a system CA certificate bundle"
        fi
        required_rust=$(python3 -c 'import re; from pathlib import Path; print(re.search(r"rust-version = \"([0-9.]+)\"", Path("Cargo.toml").read_text())[1])')
        rust_tools_run() {
            rustc --version >/dev/null 2>&1 && cargo --version >/dev/null 2>&1
        }
        rust_ready() {
            cargo --version >/dev/null 2>&1 || return 1
            python3 - "$required_rust" <<'PY'
import subprocess
import sys

def version(value):
    return (tuple(map(int, value.split("-")[0].split("."))) + (0, 0, 0))[:3]

try:
    output = subprocess.check_output(["rustc", "--version"], text=True, stderr=subprocess.DEVNULL)
    supported = version(output.split()[1]) >= version(sys.argv[1])
except (OSError, subprocess.CalledProcessError, ValueError, IndexError):
    supported = False
sys.exit(0 if supported else 1)
PY
        }
        if ! rust_ready; then
            if command -v rustup >/dev/null 2>&1; then
                # A usable stable may already exist even when no default is set.
                # Select it only for this build, preserving the user's default.
                RUSTUP_TOOLCHAIN=stable
                export RUSTUP_TOOLCHAIN
                if ! rust_ready; then
                    rust_install_status=0
                    rustup toolchain install stable --profile minimal || rust_install_status=$?
                    # Interrupted installs can report "unchanged" while lacking
                    # a manifest or compiler. Reinstall only an unusable toolchain;
                    # a failed update must not remove a working older version.
                    if ! rust_tools_run; then
                        log "Stable Rust is unusable; reinstalling the toolchain"
                        rustup toolchain uninstall stable || die "could not remove the incomplete stable Rust toolchain"
                        rustup toolchain install stable --profile minimal || die "could not reinstall stable Rust"
                    elif [ "$rust_install_status" -ne 0 ]; then
                        die "could not update stable Rust; Rust $required_rust or newer is required"
                    fi
                fi
            else
                rust_installer=$(mktemp)
                curl --proto '=https' --tlsv1.2 -fsS https://sh.rustup.rs -o "$rust_installer"
                sh "$rust_installer" -y --profile minimal --no-modify-path
                rm -f "$rust_installer"
            fi
            # Select the updated toolchain for this build without changing an
            # existing rustup default or another project's pinned toolchain.
            RUSTUP_TOOLCHAIN=stable
            export RUSTUP_TOOLCHAIN
        fi
        rust_ready || die "Rust $required_rust or newer and a working Cargo are required; repair the toolchain with rustup"
        if [ "$(uname -s)" = FreeBSD ]; then ulimit -d "$(ulimit -Hd)" 2>/dev/null || true; fi
        ;;
    MINGW*|MSYS*|CYGWIN*)
        for build_tool in cargo rustc; do
            command -v "$build_tool" >/dev/null 2>&1 || die "install Rust using winget install Rustlang.Rustup before deploying"
        done
        ;;
esac
