#!/bin/bash
# Runs exclusively as the console user. Never perform user-home writes as root.
set -euo pipefail
if [ "$(/usr/bin/id -u)" -eq 0 ]; then
    echo "Refusing to prepare user files as root" >&2
    exit 1
fi

TEMPLATE_DIR="$1"
AGENTS_DIR="$2"
LOG_DIR="$3"
ROLLOVER_BYTES="${4:-10485760}"
umask 077

reject_symlink() {
    if [ -L "$1" ]; then
        echo "Refusing symbolic link: $1" >&2
        exit 1
    fi
}

reject_symlink "$AGENTS_DIR"
reject_symlink "$LOG_DIR"
/bin/mkdir -p "$AGENTS_DIR" "$LOG_DIR"
/bin/chmod 700 "$LOG_DIR"

prepare_log() {
    local path="$1"
    reject_symlink "$path"
    reject_symlink "$path.1"
    if [ -e "$path" ] && [ ! -f "$path" ]; then
        echo "Refusing non-regular log: $path" >&2
        exit 1
    fi
    if [ -f "$path" ] && [ "$(/usr/bin/stat -f '%z' "$path")" -ge "$ROLLOVER_BYTES" ]; then
        /bin/mv -f "$path" "$path.1"
        /bin/chmod 600 "$path.1"
    fi
    /usr/bin/touch "$path"
    /bin/chmod 600 "$path"
}

install_agent() (
    local label="$1"
    local stdout_path="$2"
    local stderr_path="$3"
    local temporary
    temporary="$(/usr/bin/mktemp "$AGENTS_DIR/.sotf-agent.XXXXXX")"
    trap '/bin/rm -f "$temporary"' EXIT
    /bin/cp "$TEMPLATE_DIR/$label.plist" "$temporary"
    /usr/libexec/PlistBuddy -c "Set :StandardOutPath $stdout_path" "$temporary"
    /usr/libexec/PlistBuddy -c "Set :StandardErrorPath $stderr_path" "$temporary"
    if /usr/bin/grep -q '__SOTF_' "$temporary"; then
        echo "Unexpanded agent placeholder" >&2
        exit 1
    fi
    /bin/chmod 644 "$temporary"
    /bin/mv -f "$temporary" "$AGENTS_DIR/$label.plist"
)

prepare_log "$LOG_DIR/sotf-daemon.log"
prepare_log "$LOG_DIR/sotf-systemwide.log"
prepare_log "$LOG_DIR/sotf-systemwide.error.log"
install_agent org.spinorama.sotf-daemon "$LOG_DIR/sotf-daemon.log" "$LOG_DIR/sotf-daemon.log"
install_agent org.spinorama.sotf-systemwide "$LOG_DIR/sotf-systemwide.log" "$LOG_DIR/sotf-systemwide.error.log"
