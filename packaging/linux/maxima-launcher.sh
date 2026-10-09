#!/bin/sh
# Entry point shared by the Linux packages: installed as AppRun in the
# AppImage and as /app/bin/maxima in the Flatpak.
#
# Dispatch rules (first match wins):
#   1. invoked through a symlink / ARGV0 named maxima-{cli,server,bootstrap,tui}
#      -> that binary
#   2. first argument is a link2ea:// origin2:// or qrc:// URL
#      -> maxima-bootstrap (this is how the desktop environment hands over
#         protocol URLs, see MimeType= in the .desktop file)
#   3. first argument is cli|server|bootstrap|tui (or the full maxima-* name)
#      -> that binary, remaining arguments passed through
#   4. --install-handlers / --uninstall-handlers
#      -> (un)register this AppImage as the URL scheme handler for the
#         current user (for setups without AppImageLauncher / appimaged)
#   5. otherwise -> maxima-cli (inside a terminal emulator when started from
#      a desktop menu without a tty)
set -eu

SELF="$(readlink -f "$0")"
HERE="${APPDIR:-$(dirname "$SELF")}"
# AppImage: $APPDIR/usr/bin. Flatpak: this script is /app/bin/maxima, next to
# the binaries. MAXIMA_BIN_DIR overrides both.
if [ -n "${MAXIMA_BIN_DIR:-}" ]; then
    BIN="$MAXIMA_BIN_DIR"
elif [ -x "$HERE/usr/bin/maxima-cli" ]; then
    BIN="$HERE/usr/bin"
else
    BIN="$(dirname "$SELF")"
fi

export PATH="$BIN:$PATH"
# The .desktop file shipped in the AppImage already declares the URL scheme
# handlers. Keep the binaries from writing their own maxima-<scheme>.desktop
# files (they would point at a temporary /tmp/.mount_* path) and from warning
# on every start that the differently named handler is missing.
export MAXIMA_PACKAGED=1
export MAXIMA_DISABLE_QRC=1

APP_ID="com.ArmchairDevelopers.Maxima"
SCHEMES="x-scheme-handler/link2ea x-scheme-handler/origin2 x-scheme-handler/qrc"

exec_bin() {
    bin="$1"
    shift
    exec "$BIN/$bin" "$@"
}

refresh_desktop_db() {
    if command -v update-desktop-database >/dev/null 2>&1; then
        update-desktop-database "$1" 2>/dev/null || true
    fi
}

install_handlers() {
    target="${XDG_DATA_HOME:-$HOME/.local/share}/applications"
    appimage="${APPIMAGE:-}"
    if [ -z "$appimage" ]; then
        echo "error: \$APPIMAGE is not set; run this from the .AppImage file itself" >&2
        exit 1
    fi
    mkdir -p "$target"
    sed -e "s|^Exec=.*|Exec=\"$appimage\" %u|" \
        "$HERE/$APP_ID.desktop" > "$target/$APP_ID.desktop"
    refresh_desktop_db "$target"
    if command -v xdg-mime >/dev/null 2>&1; then
        for s in $SCHEMES; do
            xdg-mime default "$APP_ID.desktop" "$s"
        done
    else
        echo "warning: xdg-mime not found; handlers not set as default" >&2
    fi
    echo "Registered $appimage for link2ea://, origin2:// and qrc://"
}

uninstall_handlers() {
    target="${XDG_DATA_HOME:-$HOME/.local/share}/applications"
    rm -f "$target/$APP_ID.desktop"
    refresh_desktop_db "$target"
    echo "Removed $target/$APP_ID.desktop"
}

name="$(basename "${ARGV0:-$0}")"
case "$name" in
    maxima-cli|maxima-server|maxima-bootstrap|maxima-tui)
        exec_bin "$name" "$@"
        ;;
esac

if [ "$#" -gt 0 ]; then
    case "$1" in
        link2ea:*|origin2:*|qrc:*)
            exec_bin maxima-bootstrap "$@"
            ;;
        cli|server|bootstrap|tui)
            sub="$1"
            shift
            exec_bin "maxima-$sub" "$@"
            ;;
        maxima-cli|maxima-server|maxima-bootstrap|maxima-tui)
            sub="$1"
            shift
            exec_bin "$sub" "$@"
            ;;
        --install-handlers)
            install_handlers
            exit 0
            ;;
        --uninstall-handlers)
            uninstall_handlers
            exit 0
            ;;
    esac
    exec_bin maxima-cli "$@"
fi

# No arguments. From a shell this is the interactive CLI menu; from a desktop
# launcher there is no tty, so open a terminal for it.
if [ -t 0 ] && [ -t 1 ]; then
    exec_bin maxima-cli
fi
for term in x-terminal-emulator gnome-terminal konsole xfce4-terminal kitty alacritty xterm; do
    if command -v "$term" >/dev/null 2>&1; then
        case "$term" in
            gnome-terminal|xfce4-terminal) exec "$term" -- "$BIN/maxima-cli" ;;
            *) exec "$term" -e "$BIN/maxima-cli" ;;
        esac
    fi
done
exec_bin maxima-cli
