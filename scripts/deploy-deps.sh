#!/bin/sh
# Shared runtime prerequisites for the Linux and FreeBSD deploy scripts.
# Source after defining SUDO, log, and die. No work is performed on sourcing.

# Match the service, not the operator's private PATH (ProtectHome hides it).
SERVICE_PATH=/usr/local/sbin:/usr/local/bin:/usr/sbin:/usr/bin:/sbin:/bin

runtime_tool_available() (
    PATH="$SERVICE_PATH"
    export PATH
    case "$1" in
        7zip)
            for runtime_bin in 7z 7za 7zz 7zr; do
                command -v "$runtime_bin" >/dev/null 2>&1 && exit 0
            done
            exit 1
            ;;
        *) command -v "$1" >/dev/null 2>&1 ;;
    esac
)

install_runtime_package() {
    case "$RUNTIME_PKG" in
        apt)    $SUDO env DEBIAN_FRONTEND=noninteractive apt-get install -y --no-install-recommends "$1" ;;
        dnf)    $SUDO dnf install -y --setopt=install_weak_deps=False "$1" ;;
        yum)    $SUDO yum install -y "$1" ;;
        zypper) $SUDO zypper --non-interactive install --no-recommends "$1" ;;
        # Do not refresh Arch's package database without a full system upgrade.
        pacman) $SUDO pacman -S --needed --noconfirm "$1" ;;
        xbps)   $SUDO xbps-install -Sy "$1" ;;
        pkg)    $SUDO pkg install -y "$1" ;;
    esac
}

ensure_runtime_tools() {
    runtime_missing=""
    for runtime_tool in rizin upx innoextract 7zip; do
        runtime_tool_available "$runtime_tool" || runtime_missing="$runtime_missing $runtime_tool"
    done
    if [ -z "$runtime_missing" ]; then
        log "Runtime prerequisites available: rizin, upx, innoextract, 7-Zip"
        return 0
    fi

    if [ "$1" = FreeBSD ]; then
        command -v pkg >/dev/null 2>&1 || die "pkg is required to install:$runtime_missing"
        RUNTIME_PKG=pkg
    elif command -v apt-get >/dev/null 2>&1; then RUNTIME_PKG=apt
    elif command -v dnf >/dev/null 2>&1; then RUNTIME_PKG=dnf
    elif command -v yum >/dev/null 2>&1; then RUNTIME_PKG=yum
    elif command -v zypper >/dev/null 2>&1; then RUNTIME_PKG=zypper
    elif command -v pacman >/dev/null 2>&1; then RUNTIME_PKG=pacman
    elif command -v xbps-install >/dev/null 2>&1; then RUNTIME_PKG=xbps
    else
        die "missing runtime tools:$runtime_missing; install them on $SERVICE_PATH before deploying"
    fi

    if [ "$RUNTIME_PKG" = apt ]; then
        $SUDO env DEBIAN_FRONTEND=noninteractive apt-get update || die "could not refresh apt package metadata"
    fi

    for runtime_tool in $runtime_missing; do
        # Prefer current names, retaining older distro names as fallbacks.
        case "$runtime_tool:$RUNTIME_PKG" in
            7zip:pkg) runtime_candidates="7-zip p7zip" ;;
            7zip:apt) runtime_candidates="7zip p7zip-full" ;;
            7zip:*)   runtime_candidates="7zip p7zip" ;;
            upx:apt)  runtime_candidates="upx-ucl upx" ;;
            *)        runtime_candidates="$runtime_tool" ;;
        esac
        for runtime_package in $runtime_candidates; do
            log "Installing $runtime_tool prerequisite ($runtime_package via $RUNTIME_PKG)"
            if install_runtime_package "$runtime_package"; then
                runtime_tool_available "$runtime_tool" && break
            fi
        done
        runtime_tool_available "$runtime_tool" ||
            die "$runtime_tool is still unavailable on $SERVICE_PATH; tried packages: $runtime_candidates; install it before deploying"
    done
    log "Runtime prerequisites available: rizin, upx, innoextract, 7-Zip"
}
