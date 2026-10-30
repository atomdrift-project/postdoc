#!/bin/sh
# Sourced by the Unix installers after MODE, URL, STATE_HOME and SUDO are set.

DEPLOY_DIR=$(CDPATH= cd -- "$(dirname -- "$0")" && pwd)
. "$DEPLOY_DIR/deploy-deps.sh"
DEPLOY_CHANGED=0
DEPLOY_TMP=$(mktemp -d)
TMP_BIN=""
cleanup() {
    rm -rf "$DEPLOY_TMP"
    [ -z "$TMP_BIN" ] || $SUDO rm -f "$TMP_BIN"
}
trap cleanup EXIT

token_present() { [ -r "$1" ] && grep -q '[^[:space:]]' "$1"; }
check_token() {
    # A supplied blank credential is an error, never a request to remove auth.
    if [ -e "$1" ]; then
        token_present "$1" || die "credential $1 is empty, whitespace-only, or unreadable"
    elif [ "$3" = required ]; then
        $SUDO grep -q '[^[:space:]]' "$2" 2>/dev/null || die "no usable credential at $1 or $2"
    fi
}
install_changed() {
    if ! $SUDO cmp -s "$1" "$2"; then
        $SUDO install -m "$3" -o "$4" -g "$5" "$1" "$2"
        DEPLOY_CHANGED=1
    fi
}
install_token() {
    if token_present "$1"; then
        install_changed "$1" "$STATE_HOME/.tok/$2" 0600 "$SERVICE_USER" "$SERVICE_USER"
    fi
}

# Reuse the retired service's credentials only when neither the operator nor
# postdoc has one. Explicit source overrides and installed tokens take priority.
legacy_token_source() {
    if [ "$1" = "$HOME/.tok/$2" ] && [ ! -e "$1" ] &&
       ! $SUDO test -e "$STATE_HOME/.tok/$2"; then
        legacy_home=$(getent passwd scan | cut -d: -f6 || true)
        if [ -n "$legacy_home" ] && $SUDO test -f "$legacy_home/.tok/$2"; then
            (umask 077; $SUDO cat "$legacy_home/.tok/$2" > "$DEPLOY_TMP/legacy-$2") ||
                die "could not migrate Scan's $2 credential"
            printf '%s\n' "$DEPLOY_TMP/legacy-$2"
            return
        fi
    fi
    printf '%s\n' "$1"
}

prepare_credentials() {
    HOPPER_TOKEN_FILE=${HOPPER_TOKEN_FILE:-${TOKEN_FILE:-$HOME/.tok/hopper}}
    SCAN_TOKEN_FILE=${SCAN_TOKEN_FILE:-$HOME/.tok/scan}
    LLM_TOKEN_FILE=${LLM_TOKEN_FILE:-$HOME/.tok/llm}
    OPENROUTER_TOKEN_FILE=${OPENROUTER_TOKEN_FILE:-$HOME/.tok/openrouter}
    HOPPER_TOKEN_FILE=$(legacy_token_source "$HOPPER_TOKEN_FILE" hopper)
    if [ "$MODE" = serve ]; then
        SCAN_TOKEN_FILE=$(legacy_token_source "$SCAN_TOKEN_FILE" scan)
    fi
    LLM_TOKEN_FILE=$(legacy_token_source "$LLM_TOKEN_FILE" llm)
    OPENROUTER_TOKEN_FILE=$(legacy_token_source "$OPENROUTER_TOKEN_FILE" openrouter)
    requirement=optional
    if [ "$MODE" = worker ] || [ -n "$URL" ]; then requirement=required; fi
    check_token "$HOPPER_TOKEN_FILE" "$STATE_HOME/.tok/hopper" "$requirement"
    if [ "$MODE" = serve ]; then check_token "$SCAN_TOKEN_FILE" "$STATE_HOME/.tok/scan" required; fi
    or_count=0
    endpoint_count=0
    saved_ifs=$IFS
    IFS=,
    for endpoint in ${LLM:-}; do
        endpoint=$(printf '%s' "$endpoint" | tr -d '[:space:]')
        [ -n "$endpoint" ] || continue
        endpoint_count=$((endpoint_count + 1))
        case "$endpoint" in openrouter|https://openrouter.ai/*|http://openrouter.ai/*) or_count=$((or_count + 1));; esac
    done
    IFS=$saved_ifs
    if [ -n "${SCAN_LLM_KEY:-}" ]; then
        (umask 077; printf '%s\n' "$SCAN_LLM_KEY" > "$DEPLOY_TMP/llm-key")
        if [ "$or_count" -gt 0 ]; then OPENROUTER_TOKEN_FILE=$DEPLOY_TMP/llm-key
        else LLM_TOKEN_FILE=$DEPLOY_TMP/llm-key; fi
    fi
    check_token "$LLM_TOKEN_FILE" "$STATE_HOME/.tok/llm" optional
    requirement=optional
    if [ "$or_count" -gt 0 ] && [ "$or_count" -eq "$endpoint_count" ]; then requirement=required; fi
    check_token "$OPENROUTER_TOKEN_FILE" "$STATE_HOME/.tok/openrouter" "$requirement"
    # A mixed failover chain used to make both key files optional, even when
    # neither endpoint could authenticate. Require a key for at least one
    # configured endpoint before stopping or replacing any service.
    if [ "$endpoint_count" -gt 0 ]; then
        llm_key_available=0
        if [ "$endpoint_count" -gt "$or_count" ] &&
           { token_present "$LLM_TOKEN_FILE" || $SUDO grep -q '[^[:space:]]' "$STATE_HOME/.tok/llm" 2>/dev/null; }; then
            llm_key_available=1
        fi
        if [ "$or_count" -gt 0 ] &&
           { token_present "$OPENROUTER_TOKEN_FILE" || $SUDO grep -q '[^[:space:]]' "$STATE_HOME/.tok/openrouter" 2>/dev/null; }; then
            llm_key_available=1
        fi
        [ "$llm_key_available" -eq 1 ] ||
            die "no usable LLM credential for the configured endpoints; provide LLM_TOKEN_FILE, OPENROUTER_TOKEN_FILE, or SCAN_LLM_KEY (LLM= explicitly disables LLM analysis)"
    fi
    # Provision the operator's subscription for the service automatically.
    # Keep an installed token usable on redeploy when its source is absent.
    # An explicit ENRICHED setting still controls whether we provision it.
    ISOTOPE13_TOKEN_FILE=${ISOTOPE13_TOKEN_FILE:-$HOME/.tok/isotope13}
    if [ -n "${ISOTOPE13_TOKEN:-}" ]; then
        (umask 077; printf '%s\n' "$ISOTOPE13_TOKEN" > "$DEPLOY_TMP/isotope13-token")
        ISOTOPE13_TOKEN_FILE=$DEPLOY_TMP/isotope13-token
    fi
    if [ -z "${ENRICHED+x}" ]; then
        if [ -e "$ISOTOPE13_TOKEN_FILE" ] || $SUDO grep -q '[^[:space:]]' "$STATE_HOME/.config/atomdrift/isotope13-api.json" 2>/dev/null; then
            ENRICHED=1
            export ENRICHED
        fi
    fi
    case "${ENRICHED:-0}" in
        0) ;;
        1) check_token "$ISOTOPE13_TOKEN_FILE" "$STATE_HOME/.config/atomdrift/isotope13-api.json" required ;;
        *) die "ENRICHED must be 0 or 1" ;;
    esac
}
install_credentials() {
    install_token "$HOPPER_TOKEN_FILE" hopper
    if [ "$MODE" = serve ]; then install_token "$SCAN_TOKEN_FILE" scan; fi
    install_token "$LLM_TOKEN_FILE" llm
    install_token "$OPENROUTER_TOKEN_FILE" openrouter
    if [ "${ENRICHED:-0}" = 1 ] && token_present "$ISOTOPE13_TOKEN_FILE"; then
        # Rendered here, not passed on a command line: the token never reaches argv.
        (umask 077; python3 -c 'import json, sys
token = next(line.strip() for line in open(sys.argv[1]) if line.strip())
print(json.dumps({"token": token}))' "$ISOTOPE13_TOKEN_FILE" > "$DEPLOY_TMP/isotope13-api.json") \
            || die "could not read $ISOTOPE13_TOKEN_FILE"
        $SUDO install -d -m0700 -o "$SERVICE_USER" -g "$SERVICE_USER" "$STATE_HOME/.config" "$STATE_HOME/.config/atomdrift"
        install_changed "$DEPLOY_TMP/isotope13-api.json" "$STATE_HOME/.config/atomdrift/isotope13-api.json" 0600 "$SERVICE_USER" "$SERVICE_USER"
    fi
}
prepare_arguments() {
    # Keep absent settings absent; only explicit deployment overrides become flags.
    EXEC_ARGS=$(python3 "$DEPLOY_DIR/deploy-config.py" args "$MODE" "$URL" "$STATE_HOME") || die "invalid deployment arguments"
}
install_binary() {
    $SUDO install -d -m0755 -o root -g "$ROOT_GROUP" "$STATE_HOME/bin"
    if ! $SUDO cmp -s "$BUILD_BIN" "$BIN_PATH"; then
        TMP_BIN=$($SUDO mktemp "$STATE_HOME/bin/.postdoc.XXXXXX")
        $SUDO install -m0755 -o root -g "$ROOT_GROUP" "$BUILD_BIN" "$TMP_BIN"
        $SUDO mv -f "$TMP_BIN" "$BIN_PATH"
        TMP_BIN=""
        DEPLOY_CHANGED=1
    fi

    # SELinux's default label for executables under /var/lib is var_lib_t,
    # which prevents systemd services from executing the binary (203/EXEC).
    # Persist the executable label and reapply it after every atomic install.
    if command -v getenforce >/dev/null 2>&1 && [ "$(getenforce)" != Disabled ]; then
        command -v semanage >/dev/null 2>&1 || die "semanage is required to label the service binary under SELinux"
        if ! $SUDO semanage fcontext -a -t bin_t "$BIN_PATH" 2>/dev/null; then
            $SUDO semanage fcontext -m -t bin_t "$BIN_PATH" || die "could not configure SELinux executable label for $BIN_PATH"
        fi
        $SUDO restorecon -F "$BIN_PATH" || die "could not apply SELinux executable label to $BIN_PATH"
    fi
}
save_configuration() {
    python3 "$DEPLOY_DIR/deploy-config.py" save "$MODE" "$URL" > "$DEPLOY_TMP/config.json"
    $SUDO install -d -m0755 -o root -g "$ROOT_GROUP" "$CONFIG_DIR"
    install_changed "$DEPLOY_TMP/config.json" "$CONFIG_DIR/$MODE.json" 0644 root "$ROOT_GROUP"
}
wait_ready() {
    if [ "$MODE" = serve ]; then
        expect_idle=0
        if [ -n "$URL" ] && [ "${IDLE:-1}" != 0 ] && [ "${WORKERS:-2}" -ge 2 ]; then expect_idle=1; fi
        $SUDO python3 "$DEPLOY_DIR/deploy-config.py" health "$STATE_HOME/.tok/scan" "${BIND:-127.0.0.1:49999}" "${HEALTH_WAIT:-300}" "$expect_idle"
    fi
}
