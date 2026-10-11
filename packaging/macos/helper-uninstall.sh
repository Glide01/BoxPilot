#!/bin/sh
# Remove BoxPilot's privileged helper from macOS (ADR 0006 rule 7):
#
#   helper-uninstall.sh [--keep-state | --remove-state]
#
# It runs as root: under BoxPilot's administrator prompt ("Remove helper"),
# or with sudo; it works without the app. It unloads the daemon (launchctl
# bootout: launchd sends it SIGTERM, and it stops its sing-box first), then
# removes the helper, its plist, its sing-box and manifest, and the socket.
#
# The state directory (/Library/Application Support/BoxPilot Helper/state:
# each account's sing-box cache and Tailscale logins, the helper's log and
# the owner record) is kept, as on Windows, unless --remove-state is given;
# then the whole /Library/Application Support/BoxPilot Helper goes.
#
# The same, by hand:
#
#   sudo launchctl bootout system/io.github.glide01.boxpilot.helper
#   sudo rm -f /Library/LaunchDaemons/io.github.glide01.boxpilot.helper.plist \
#       /Library/PrivilegedHelperTools/io.github.glide01.boxpilot.helper
#   sudo rm -rf "/Library/Application Support/BoxPilot Helper"

set -eu

LABEL='io.github.glide01.boxpilot.helper'
HELPER_PATH='/Library/PrivilegedHelperTools/io.github.glide01.boxpilot.helper'
SUPPORT_DIR='/Library/Application Support/BoxPilot Helper'
BIN_DIR='/Library/Application Support/BoxPilot Helper/bin'
STATE_DIR='/Library/Application Support/BoxPilot Helper/state'
PLIST_PATH='/Library/LaunchDaemons/io.github.glide01.boxpilot.helper.plist'
SOCKET_PATH='/var/run/io.github.glide01.boxpilot.helper.sock'

PATH='/usr/bin:/bin:/usr/sbin:/sbin'
export PATH

die() {
    printf 'helper-uninstall: %s\n' "$*" >&2
    exit 1
}

usage="usage: helper-uninstall.sh [--keep-state | --remove-state]"
remove_state=no
case $# in
    0) ;;
    1)
        case $1 in
            --keep-state) ;;
            --remove-state) remove_state=yes ;;
            *) die "$usage" ;;
        esac
        ;;
    *) die "$usage" ;;
esac

[ "$(/usr/bin/id -u)" -eq 0 ] || die "it must run as root"

if /bin/launchctl print "system/$LABEL" >/dev/null 2>&1; then
    /bin/launchctl bootout "system/$LABEL" 2>/dev/null || :
    tries=0
    while /bin/launchctl print "system/$LABEL" >/dev/null 2>&1; do
        tries=$((tries + 1))
        [ "$tries" -le 40 ] || die "the helper is still loaded after launchctl bootout"
        /bin/sleep 1
    done
fi

/bin/rm -f "$PLIST_PATH" "$HELPER_PATH" "$SOCKET_PATH"
/bin/rm -rf "$BIN_DIR"
if [ "$remove_state" = yes ]; then
    /bin/rm -rf "$SUPPORT_DIR"
    printf 'helper-uninstall: the helper is removed, and its state directory too\n'
else
    printf 'helper-uninstall: the helper is removed; its state directory stays: %s\n' "$STATE_DIR"
    printf 'helper-uninstall: (each account'\''s cache and Tailscale logins, and the log; --remove-state removes it)\n'
fi
