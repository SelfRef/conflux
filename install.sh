#!/usr/bin/env bash
#
# install.sh — build conflux from source and install (or remove) the binary,
# a starter config, and the systemd unit files.
#
# Usage:
#   ./install.sh install   [--system] [--no-build]
#   ./install.sh uninstall [--system] [--purge]
#   ./install.sh --help
#
# Modes (standard local-from-source locations):
#   user (default) — binary in ~/.local/bin, units in ~/.config/systemd/user,
#                    config in ~/.config/conflux/config.toml. Manage with
#                    `systemctl --user`.
#   --system       — binary in /usr/local/bin, units in /etc/systemd/system,
#                    config in /etc/conflux/config.toml. Runs as the `conflux`
#                    system user. Requires root (run with sudo).
#
# Both the plain and the profile-templated units are installed, so you can
# `enable conflux` (implicit "default" profile) or `enable conflux@<profile>`.

set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"

info() { printf '\033[1;32m==>\033[0m %s\n' "$*"; }
warn() { printf '\033[1;33mwarning:\033[0m %s\n' "$*" >&2; }
die()  { printf '\033[1;31merror:\033[0m %s\n' "$*" >&2; exit 1; }

usage() {
    cat <<'EOF'
install.sh — build conflux from source and install (or remove) the binary,
a starter config, and the systemd unit files.

Usage:
  ./install.sh install   [--system] [--no-build]
  ./install.sh uninstall [--system] [--purge]
  ./install.sh --help

Modes (standard local-from-source locations):
  user (default) — binary in ~/.local/bin, units in ~/.config/systemd/user,
                   config in ~/.config/conflux/config.toml. Manage with
                   `systemctl --user`.
  --system       — binary in /usr/local/bin, units in /etc/systemd/system,
                   config in /etc/conflux/config.toml. Runs as the `conflux`
                   system user. Requires root (run with sudo).

Options:
  --no-build   install the existing target/release/conflux without rebuilding.
  --purge      on uninstall, also remove the config, state, and system user.

Both the plain and the profile-templated units are installed, so you can
`enable conflux` (implicit "default" profile) or `enable conflux@<profile>`.
EOF
    exit "${1:-0}"
}

# ---------------------------------------------------------------------------
# Argument parsing
# ---------------------------------------------------------------------------

COMMAND=""
SYSTEM=0
NO_BUILD=0
PURGE=0

for arg in "$@"; do
    case "$arg" in
        install|uninstall) COMMAND="$arg" ;;
        --system)          SYSTEM=1 ;;
        --no-build)        NO_BUILD=1 ;;
        --purge)           PURGE=1 ;;
        -h|--help)         usage 0 ;;
        *)                 die "unknown argument: $arg (see --help)" ;;
    esac
done

[ -n "$COMMAND" ] || usage 1

# ---------------------------------------------------------------------------
# Mode-dependent locations
# ---------------------------------------------------------------------------

if [ "$SYSTEM" -eq 1 ]; then
    [ "$(id -u)" -eq 0 ] || die "--system requires root; re-run with sudo"
    BINDIR="/usr/local/bin"
    UNITDIR="/etc/systemd/system"
    CONFIG_DIR="/etc/conflux"
    STATE_DIR="/var/lib/conflux"
    UNIT_PLAIN="conflux.system.service"
    UNIT_TEMPLATE="conflux@.system.service"
    SYSTEMCTL=(systemctl)
    SERVICE_USER="conflux"
else
    BINDIR="$HOME/.local/bin"
    UNITDIR="$HOME/.config/systemd/user"
    CONFIG_DIR="${XDG_CONFIG_HOME:-$HOME/.config}/conflux"
    STATE_DIR="${XDG_STATE_HOME:-$HOME/.local/state}/conflux"
    UNIT_PLAIN="conflux.user.service"
    UNIT_TEMPLATE="conflux@.user.service"
    SYSTEMCTL=(systemctl --user)
    SERVICE_USER=""
fi

CONFIG_FILE="$CONFIG_DIR/config.toml"
BIN="$BINDIR/conflux"

# systemctl is optional: install the files even where it is unavailable.
have_systemctl() { command -v systemctl >/dev/null 2>&1; }
reload_daemon() {
    if have_systemctl; then
        "${SYSTEMCTL[@]}" daemon-reload || warn "systemctl daemon-reload failed"
    else
        warn "systemctl not found; skipping daemon-reload"
    fi
}

# ---------------------------------------------------------------------------
# install
# ---------------------------------------------------------------------------

do_install() {
    local src="$SCRIPT_DIR/target/release/conflux"

    if [ "$NO_BUILD" -eq 0 ]; then
        command -v cargo >/dev/null 2>&1 || die "cargo not found; install Rust or pass --no-build"
        info "Building conflux (release)"
        ( cd "$SCRIPT_DIR" && cargo build --release --bin conflux )
    fi
    [ -f "$src" ] || die "binary not found at $src (build it, or drop --no-build)"

    info "Installing binary → $BIN"
    install -Dm755 "$src" "$BIN"

    # Create the service user before installing system config/units.
    if [ -n "$SERVICE_USER" ] && ! id "$SERVICE_USER" >/dev/null 2>&1; then
        info "Creating system user '$SERVICE_USER'"
        useradd --system --create-home --home-dir "$STATE_DIR" "$SERVICE_USER"
    fi

    # Starter config — never clobber an existing one.
    if [ -e "$CONFIG_FILE" ]; then
        info "Config already present, leaving it untouched → $CONFIG_FILE"
    else
        info "Installing starter config → $CONFIG_FILE"
        install -Dm644 "$SCRIPT_DIR/config.example.toml" "$CONFIG_FILE"
        if [ -n "$SERVICE_USER" ]; then
            chown -R "$SERVICE_USER":"$SERVICE_USER" "$CONFIG_DIR"
        fi
    fi

    info "Installing systemd units → $UNITDIR"
    install_unit "$UNIT_PLAIN"    "$UNITDIR/conflux.service"
    install_unit "$UNIT_TEMPLATE" "$UNITDIR/conflux@.service"
    reload_daemon

    print_next_steps
}

# install_unit <src-basename> <dest-path>
# Copies a unit file, rewriting its ExecStart from the packaged /usr/bin/conflux
# to the binary we just installed (~/.local/bin or /usr/local/bin) so this
# from-source install works wherever the binary landed.
install_unit() {
    local src="$SCRIPT_DIR/systemd/$1" dest="$2"
    [ -f "$src" ] || die "unit file missing: $src"
    install -d "$(dirname "$dest")"
    sed -E "s#^ExecStart=(/[^ ]*/)?conflux#ExecStart=${BIN}#" "$src" >"$dest"
    chmod 644 "$dest"
}

print_next_steps() {
    local sc="systemctl --user" status="conflux status"
    if [ "$SYSTEM" -eq 1 ]; then
        sc="sudo systemctl"
        status="conflux --system status"
    fi
    cat <<EOF

conflux installed. Next steps:
  1. Edit your config:   \$EDITOR $CONFIG_FILE
  2. Enable the daemon:  $sc enable --now conflux
     (or a profile:      $sc enable --now conflux@desktop)
  3. Check it:           $status
EOF
    if [ "$SYSTEM" -eq 0 ] && ! printf '%s' ":$PATH:" | grep -q ":$BINDIR:"; then
        warn "$BINDIR is not on your PATH; add it so you can run 'conflux'"
    fi
}

# ---------------------------------------------------------------------------
# uninstall
# ---------------------------------------------------------------------------

do_uninstall() {
    if have_systemctl; then
        info "Stopping and disabling conflux.service (if enabled)"
        "${SYSTEMCTL[@]}" disable --now conflux.service >/dev/null 2>&1 || true
    fi

    info "Removing systemd units"
    rm -f "$UNITDIR/conflux.service" "$UNITDIR/conflux@.service"
    reload_daemon

    info "Removing binary → $BIN"
    rm -f "$BIN"

    if [ "$PURGE" -eq 1 ]; then
        info "Purging config → $CONFIG_DIR"
        rm -rf "$CONFIG_DIR"
        info "Purging state → $STATE_DIR"
        rm -rf "$STATE_DIR"
        if [ -n "$SERVICE_USER" ] && id "$SERVICE_USER" >/dev/null 2>&1; then
            info "Removing system user '$SERVICE_USER'"
            userdel "$SERVICE_USER" >/dev/null 2>&1 || warn "could not remove user '$SERVICE_USER'"
        fi
    else
        info "Kept config ($CONFIG_DIR) and state ($STATE_DIR); pass --purge to remove them"
    fi

    cat <<EOF

conflux uninstalled.
EOF
    if have_systemctl; then
        printf '  If any profile instances were enabled, disable them too, e.g.:\n    %s disable --now conflux@desktop\n' "${SYSTEMCTL[*]}"
    fi
}

# ---------------------------------------------------------------------------

case "$COMMAND" in
    install)   do_install ;;
    uninstall) do_uninstall ;;
esac
