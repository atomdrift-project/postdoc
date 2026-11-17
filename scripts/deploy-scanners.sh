#!/bin/sh
# The third-party scanners postdoc runs by default: ClamAV (through clamd),
# malcontent (`mal`) and GuardDog.
#
# Sourced by the Linux and FreeBSD deploys after deploy-deps.sh, which call
#     ensure_scanners <Linux|FreeBSD> <service user>
# once the service user exists. Run on its own (`make scanners`) it does the
# same for the host it runs on, macOS included.
#
# SCANNERS picks which (default clamav,malcontent,guarddog; `none` skips all),
# and is the same list postdoc's --scanners takes. A scanner that already
# answers is left alone, so a redeploy changes nothing. postdoc checks each
# one again when it starts, and refuses to start without one it was asked for.
#
#   ClamAV      the platform's package, configured with a local socket, its
#               signature databases fetched once, clamd and freshclam running
#   malcontent  Homebrew on macOS; built from source elsewhere
#               (scripts/build-malcontent.sh; the first build takes ~12 minutes)
#   GuardDog    a virtualenv under $SCANNER_PREFIX/lib/postdoc-guarddog, on
#               Python 3.10-3.13, linked into $SCANNER_PREFIX/bin
#
# Source after defining SUDO, log and die. No work is performed on sourcing.

SCANNERS_ALL=clamav,malcontent,guarddog
SCANNER_PREFIX=${SCANNER_PREFIX:-/usr/local}
# Where each platform's clamd package keeps its configuration, and the socket
# a configuration without one is given.
CLAMD_CONFS="/etc/clamav/clamd.conf /etc/clamd.d/scan.conf /etc/clamd.conf /usr/local/etc/clamd.conf"
FRESHCLAM_CONFS="/etc/clamav/freshclam.conf /etc/freshclam.conf /usr/local/etc/freshclam.conf"
CLAMAV_DB_DIRS="/var/lib/clamav /var/db/clamav"
# How long clamd may take to load its databases and open its socket, and
# freshclam to fetch them.
CLAMD_WAIT=${CLAMD_WAIT:-600}
FRESHCLAM_WAIT=${FRESHCLAM_WAIT:-600}
# The largest artifact clamd scans: postdoc's own body limit (--max-size-mb,
# 1024 by default). Packaged limits are 25-100M, past which clamd refuses
# the stream and the artifact goes unscanned.
CLAMD_MAX=${CLAMD_MAX:-1024M}
CLAMD_LIMITS="StreamMaxLength MaxFileSize MaxScanSize"

scanner_wanted() {
    case ",${SCANNERS:-$SCANNERS_ALL}," in
        *",$1,"*) return 0 ;;
    esac
    return 1
}

# The package manager scanners are installed with: SCANNER_PKG.
scanner_package_manager() {
    case "$1" in
        Darwin)
            command -v brew >/dev/null 2>&1 || die "Homebrew is required to install the scanners on macOS"
            SCANNER_PKG=brew ;;
        FreeBSD)
            command -v pkg >/dev/null 2>&1 || die "pkg is required to install the scanners"
            SCANNER_PKG=pkg ;;
        *)
            if command -v apt-get >/dev/null 2>&1; then SCANNER_PKG=apt
            elif command -v dnf >/dev/null 2>&1; then SCANNER_PKG=dnf
            elif command -v yum >/dev/null 2>&1; then SCANNER_PKG=yum
            elif command -v zypper >/dev/null 2>&1; then SCANNER_PKG=zypper
            elif command -v pacman >/dev/null 2>&1; then SCANNER_PKG=pacman
            else die "no supported package manager to install the scanners with; install them by hand, or deploy with SCANNERS=none"
            fi ;;
    esac
}

scanner_install() {
    case "$SCANNER_PKG" in
        brew) brew install "$@" ;;
        apt)
            if [ -z "${SCANNER_APT_UPDATED:-}" ]; then
                $SUDO env DEBIAN_FRONTEND=noninteractive apt-get update || die "could not refresh apt package metadata"
                SCANNER_APT_UPDATED=1
            fi
            $SUDO env DEBIAN_FRONTEND=noninteractive apt-get install -y --no-install-recommends "$@" ;;
        dnf)    $SUDO dnf install -y --setopt=install_weak_deps=False "$@" ;;
        yum)    $SUDO yum install -y "$@" ;;
        zypper) $SUDO zypper --non-interactive install --no-recommends "$@" ;;
        pacman) $SUDO pacman -S --needed --noconfirm "$@" ;;
        pkg)    $SUDO pkg install -y "$@" ;;
    esac
}

# A program the service will find: on its PATH, or under the scanner prefix.
scanner_program() {
    for dir in $(printf '%s' "${SERVICE_PATH:-$PATH}" | tr ':' ' ') "$SCANNER_PREFIX/bin" "$SCANNER_PREFIX/sbin"; do
        if [ -x "$dir/$1" ]; then
            printf '%s\n' "$dir/$1"
            return 0
        fi
    done
    return 1
}

# Comment out a configuration's `Example` line, which keeps clamd and
# freshclam from starting, and give it `$2 $3` when it sets no `$2`.
clamav_configure() {
    conf=$1
    $SUDO sed -i.postdoc-bak -e 's/^Example/#Example/' "$conf" && $SUDO rm -f "$conf.postdoc-bak"
    if [ -n "${2:-}" ] && ! grep -q "^[[:space:]]*$2[[:space:]]" "$conf"; then
        printf '%s %s\n' "$2" "$3" | $SUDO tee -a "$conf" >/dev/null
    fi
}

# Give a configuration `$2 $3`, replacing whatever value it had. Succeeds
# only when that changed the file.
clamav_set() {
    current=$(sed -n "s/^[[:space:]]*$2[[:space:]]\{1,\}\([^[:space:]]*\).*/\1/p" "$1" | tail -n 1)
    [ "$current" != "$3" ] || return 1
    $SUDO sed -i.postdoc-bak -e "/^[[:space:]]*$2[[:space:]]/d" "$1" && $SUDO rm -f "$1.postdoc-bak"
    printf '%s %s\n' "$2" "$3" | $SUDO tee -a "$1" >/dev/null
}

# The socket a clamd configuration names.
clamav_socket_of() {
    sed -n 's/^[[:space:]]*LocalSocket[[:space:]]\{1,\}\([^[:space:]]*\).*/\1/p' "$1" | head -n 1
}

# Whether clamd answers PING on socket $1. The socket existing is not
# enough: systemd can hold it open while a restarted clamd is still loading
# its databases, which takes minutes.
clamd_answers() {
    [ -S "$1" ] || return 1
    $SUDO python3 -c '
import socket, sys
s = socket.socket(socket.AF_UNIX)
s.settimeout(5)
s.connect(sys.argv[1])
s.sendall(b"zPING\0")
sys.exit(s.recv(8) != b"PONG\0")
' "$1" 2>/dev/null
}

clamav_has_databases() {
    for dir in $CLAMAV_DB_DIRS; do
        for db in "$dir"/daily.cvd "$dir"/daily.cld; do
            [ -e "$db" ] && return 0
        done
    done
    return 1
}

# Wait for freshclam's first download. A freshclam scheduled by a timer
# rather than run as a daemon may not start one, so after half a minute it
# is run directly; a run that collides with the service's is harmless.
clamav_wait_databases() {
    waited=0
    while ! clamav_has_databases; do
        [ "$waited" -ne 0 ] || log "Waiting for freshclam to download the ClamAV signature databases (~110 MiB)"
        if [ "$waited" -eq 30 ]; then $SUDO freshclam >/dev/null 2>&1 || true; fi
        [ "$waited" -lt "$FRESHCLAM_WAIT" ] || die "no ClamAV signature databases in $CLAMAV_DB_DIRS after ${FRESHCLAM_WAIT}s"
        sleep 5
        waited=$((waited + 5))
    done
}

# Enable and start the first of the given systemd units that exists; with
# --restart, restart it, so a running one reads a changed configuration.
clamav_systemd_start() {
    restart=""
    if [ "$1" = --restart ]; then restart=1; shift; fi
    for unit in "$@"; do
        if $SUDO systemctl cat "$unit" >/dev/null 2>&1; then
            if [ -n "$restart" ]; then
                { $SUDO systemctl enable "$unit" && $SUDO systemctl restart "$unit"; } || die "could not restart $unit"
            else
                $SUDO systemctl enable --now "$unit" || die "could not start $unit"
            fi
            return 0
        fi
    done
    die "none of $* is installed; start clamd and freshclam by hand, or deploy without clamav in SCANNERS"
}

ensure_clamav() {
    platform=$1
    user=${2:-}
    if [ "$platform" = Darwin ]; then
        brew_prefix=$(brew --prefix)
        CLAMD_CONFS="$brew_prefix/etc/clamav/clamd.conf"
        FRESHCLAM_CONFS="$brew_prefix/etc/clamav/freshclam.conf"
        CLAMAV_DB_DIRS="$brew_prefix/var/lib/clamav"
        default_socket="$brew_prefix/var/run/clamav/clamd.sock"
    elif [ "$platform" = FreeBSD ]; then
        default_socket=/var/run/clamav/clamd.sock
    elif [ "$SCANNER_PKG" = dnf ] || [ "$SCANNER_PKG" = yum ]; then
        default_socket=/run/clamd.scan/clamd.sock
    else
        default_socket=/run/clamav/clamd.ctl
    fi

    if ! scanner_program clamd >/dev/null; then
        case "$SCANNER_PKG" in
            apt)        packages="clamav-daemon clamav-freshclam" ;;
            dnf|yum)    packages="clamd clamav-update" ;;
            *)          packages="clamav" ;;
        esac
        log "Installing ClamAV ($packages via $SCANNER_PKG)"
        # shellcheck disable=SC2086 # a package list
        scanner_install $packages || die "could not install ClamAV ($packages); on RHEL-family hosts it is in EPEL"
    fi

    # A package that ships only samples (FreeBSD, Homebrew) gets configured
    # copies; one that ships a disabled configuration gets it enabled.
    for conf in $CLAMD_CONFS $FRESHCLAM_CONFS; do
        if [ ! -e "$conf" ] && [ -e "$conf.sample" ]; then
            $SUDO cp "$conf.sample" "$conf"
        fi
    done
    clamd_conf=""
    for conf in $CLAMD_CONFS; do
        if [ -e "$conf" ]; then clamd_conf=$conf; break; fi
    done
    [ -n "$clamd_conf" ] || die "ClamAV is installed but no clamd configuration was found in: $CLAMD_CONFS"
    clamav_configure "$clamd_conf" LocalSocket "$default_socket"
    # Packages that leave the mode unset inherit clamd's umask, which keeps
    # the postdoc user out; a stream scan is all the socket offers.
    clamav_configure "$clamd_conf" LocalSocketMode 666
    # Raised on every deploy, and clamd restarted when that changed them: a
    # running clamd keeps the limits it started with.
    restart_clamd=""
    for limit in $CLAMD_LIMITS; do
        if clamav_set "$clamd_conf" "$limit" "$CLAMD_MAX"; then restart_clamd=--restart; fi
    done
    [ -z "$restart_clamd" ] || log "clamd limits set to $CLAMD_MAX in $clamd_conf"
    for conf in $FRESHCLAM_CONFS; do
        if [ -e "$conf" ]; then clamav_configure "$conf"; fi
    done
    socket=$(clamav_socket_of "$clamd_conf")
    [ -n "$socket" ] || die "$clamd_conf sets no LocalSocket"

    # freshclam first: clamd will not start without signature databases, and
    # where freshclam runs as a service it fetches them itself.
    case "$platform" in
        Darwin)
            # Homebrew ships no freshclam service.
            if ! clamav_has_databases; then
                log "Downloading ClamAV signature databases (first run, ~110 MiB)"
                freshclam || die "freshclam could not download the signature databases"
            fi
            install -d "$(dirname "$socket")"
            # Homebrew's clamav service runs clamd as root, so only it is
            # started with sudo.
            if [ -n "$restart_clamd" ]; then
                sudo brew services restart clamav || die "could not restart clamd (brew services restart clamav)"
            else
                sudo brew services start clamav || die "could not start clamd (brew services start clamav)"
            fi
            log "Run freshclam from time to time: Homebrew schedules no signature updates"
            ;;
        FreeBSD)
            $SUDO sysrc clamav_freshclam_enable=YES clamav_clamd_enable=YES >/dev/null
            $SUDO service clamav-freshclam status >/dev/null 2>&1 || $SUDO service clamav-freshclam start ||
                die "could not start clamav-freshclam"
            clamav_wait_databases
            if [ -n "$restart_clamd" ]; then
                $SUDO service clamav-clamd restart || die "could not restart clamav-clamd"
            else
                $SUDO service clamav-clamd status >/dev/null 2>&1 || $SUDO service clamav-clamd start ||
                    die "could not start clamav-clamd"
            fi
            ;;
        *)
            clamav_systemd_start clamav-freshclam.service freshclam.service freshclam.timer
            clamav_wait_databases
            # shellcheck disable=SC2086 # empty or one flag
            clamav_systemd_start $restart_clamd clamav-daemon.service clamd@scan.service clamd.service
            ;;
    esac

    waited=0
    while ! clamd_answers "$socket"; do
        [ "$waited" -lt "$CLAMD_WAIT" ] || die "clamd did not answer on $socket within ${CLAMD_WAIT}s"
        sleep 2
        waited=$((waited + 2))
    done
    # The service reaches clamd through its socket, which a package may keep
    # to its own group.
    if [ -n "$user" ]; then
        group=$(stat -c %G "$socket" 2>/dev/null || stat -f %Sg "$socket")
        case "$group" in
            root|wheel|"") ;;
            *)
                if ! id -Gn "$user" 2>/dev/null | tr ' ' '\n' | grep -qx "$group"; then
                    log "Adding $user to $group, which owns $socket"
                    if [ "$platform" = FreeBSD ]; then
                        $SUDO pw groupmod "$group" -m "$user" || die "could not add $user to $group for $socket"
                    else
                        $SUDO usermod -a -G "$group" "$user" || die "could not add $user to $group for $socket"
                    fi
                    # A running service keeps the groups it started with.
                    DEPLOY_CHANGED=1
                fi
                ;;
        esac
    fi
    log "ClamAV ready: clamd on $socket"
}

ensure_malcontent() {
    platform=$1
    if mal=$(scanner_program mal) && "$mal" --version >/dev/null 2>&1; then
        log "malcontent ready: $("$mal" --version)"
        return 0
    fi
    if [ "$platform" = Darwin ]; then
        log "Installing malcontent (Homebrew)"
        brew install malcontent || die "could not install malcontent"
    else
        # malcontent publishes no packages here: build it, against the
        # yara-x C API it links, with Go, Rust and cargo-c.
        for tool in git go pkg-config; do
            command -v "$tool" >/dev/null 2>&1 && continue
            case "$tool:$SCANNER_PKG" in
                go:apt) package=golang-go ;;
                go:*) package=go ;;
                pkg-config:apt) package=pkg-config ;;
                pkg-config:dnf|pkg-config:yum) package=pkgconf-pkg-config ;;
                pkg-config:*) package=pkgconf ;;
                *) package=$tool ;;
            esac
            log "Installing $package to build malcontent"
            scanner_install "$package" || die "could not install $package"
        done
        if ! command -v cargo-cbuild >/dev/null 2>&1; then
            if [ "$platform" = FreeBSD ]; then
                scanner_install cargo-c || die "could not install cargo-c"
            else
                log "Installing cargo-c to build malcontent"
                cargo install --locked cargo-c || die "could not install cargo-c"
            fi
        fi
        build=${MALCONTENT_BUILD_DIR:-$HOME/.cache/postdoc/malcontent}
        mkdir -p "$build"
        log "Building malcontent (the first build compiles yara-x, ~12 minutes)"
        MALCONTENT_SRC="$build/src" MAL_BIN="$build/mal" "$DEPLOY_DIR/build-malcontent.sh" ||
            die "malcontent did not build"
        $SUDO install -d "$SCANNER_PREFIX/bin"
        $SUDO install -m 0755 "$build/mal" "$SCANNER_PREFIX/bin/mal"
    fi
    mal=$(scanner_program mal) || die "malcontent is installed but no mal is on the service's PATH"
    "$mal" --version >/dev/null 2>&1 || die "$mal does not run"
    log "malcontent ready: $("$mal" --version)"
}

# The first of the given Pythons GuardDog installs on: 3.10 or newer (its
# own floor), and no newer than 3.13: the pygit2 it pins has no wheels for
# later versions, and building it there from source fails.
guarddog_python() {
    for candidate in "$@"; do
        command -v "$candidate" >/dev/null 2>&1 || continue
        case "$("$candidate" -c 'import sys; print("%d.%d" % sys.version_info[:2])' 2>/dev/null)" in
            3.10|3.11|3.12|3.13) command -v "$candidate"; return 0 ;;
        esac
    done
    return 1
}

ensure_guarddog() {
    platform=$1
    if dog=$(scanner_program guarddog) && "$dog" --version >/dev/null 2>&1; then
        log "GuardDog ready: $("$dog" --version)"
        return 0
    fi
    venv="$SCANNER_PREFIX/lib/postdoc-guarddog"
    if [ "$platform" = Darwin ]; then
        bin="$(brew --prefix)/bin"
        set -- "$bin/python3" "$bin/python3.13" "$bin/python3.12" "$bin/python3.11" "$bin/python3.10"
    else
        set -- python3 python3.13 python3.12 python3.11 python3.10
    fi
    if ! python=$(guarddog_python "$@"); then
        log "No Python 3.10-3.13 for GuardDog; installing Python 3.13"
        if [ "$platform" = Darwin ]; then scanner_install python@3.13; else scanner_install python3.13; fi
        python=$(guarddog_python "$@") ||
            die "GuardDog needs Python 3.10-3.13 (its pygit2 has no wheels for newer); install one, or leave guarddog out of SCANNERS"
    fi
    # --clear: a virtualenv left by a failed install, or built on another
    # Python, is replaced rather than reused.
    if ! $SUDO "$python" -m venv --clear "$venv"; then
        # Debian splits venv (and the pip it bootstraps) out of each python3.x.
        [ "$SCANNER_PKG" = apt ] || die "could not create a Python virtualenv for GuardDog"
        scanner_install "$(basename -- "$python")-venv" || die "could not install $(basename -- "$python")-venv"
        $SUDO "$python" -m venv --clear "$venv" || die "could not create a Python virtualenv for GuardDog"
    fi
    if [ "$platform" = FreeBSD ]; then
        # No wheels are built for FreeBSD: yara-python builds against libyara
        # and pygit2 against libgit2. GuardDog's sandbox dependency has no
        # FreeBSD build at all, so a stub stands in for it (postdoc passes
        # --no-sandbox here; see scripts/nono-stub-wheel.py).
        scanner_install yara libgit2 || die "could not install yara and libgit2 for GuardDog"
        wheels="${DEPLOY_TMP:-$(mktemp -d)}/guarddog-wheels"
        python3 "$DEPLOY_DIR/nono-stub-wheel.py" "$wheels" >/dev/null || die "could not build the nono-py stub"
        set -- --find-links "$wheels" -c "$wheels/overrides.txt"
    else
        set --
    fi
    log "Installing GuardDog into $venv"
    $SUDO "$venv/bin/pip" install --quiet --upgrade "$@" 'guarddog>=3' || die "could not install GuardDog"
    $SUDO install -d "$SCANNER_PREFIX/bin"
    $SUDO ln -sf "$venv/bin/guarddog" "$SCANNER_PREFIX/bin/guarddog"
    dog=$(scanner_program guarddog) || die "GuardDog is installed but not on the service's PATH"
    "$dog" --version >/dev/null 2>&1 || die "$dog does not run"
    log "GuardDog ready: $("$dog" --version)"
}

ensure_scanners() {
    platform=$1
    user=${2:-}
    list=${SCANNERS:-$SCANNERS_ALL}
    for name in $(printf '%s' "$list" | tr ',' ' '); do
        case "$name" in
            clamav|malcontent|guarddog|none) ;;
            *) die "SCANNERS names unknown scanner $name; known: $SCANNERS_ALL, none" ;;
        esac
    done
    if ! scanner_wanted clamav && ! scanner_wanted malcontent && ! scanner_wanted guarddog; then
        log "Third-party scanners: none"
        return 0
    fi
    scanner_package_manager "$platform"
    if scanner_wanted clamav; then ensure_clamav "$platform" "$user"; fi
    if scanner_wanted malcontent; then ensure_malcontent "$platform"; fi
    if scanner_wanted guarddog; then ensure_guarddog "$platform"; fi
}

# Run on its own: this host, with the privileges it takes to install.
if [ "$(basename -- "$0")" = deploy-scanners.sh ]; then
    set -eu
    die() { echo "error: $*" >&2; exit 1; }
    log() { printf '==> %s\n' "$*"; }
    DEPLOY_DIR=$(CDPATH='' cd -- "$(dirname -- "$0")" && pwd)
    platform=$(uname -s)
    SUDO=""
    if [ "$platform" = Darwin ]; then
        # Homebrew refuses root, so only the clamd service is started with
        # sudo; everything else lands under Homebrew's own prefix.
        SCANNER_PREFIX=$(brew --prefix)
        SERVICE_PATH="$SCANNER_PREFIX/bin:/usr/bin:/bin"
        ensure_scanners Darwin
        exit 0
    fi
    if [ "$(id -u)" -ne 0 ]; then
        if command -v doas >/dev/null 2>&1; then SUDO=doas
        elif command -v sudo >/dev/null 2>&1; then SUDO=sudo
        else die "need doas or sudo to install the scanners"
        fi
    fi
    . "$DEPLOY_DIR/deploy-deps.sh"
    ensure_scanners "$platform" "${SERVICE_USER:-}"
fi
