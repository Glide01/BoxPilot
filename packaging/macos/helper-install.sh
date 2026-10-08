#!/bin/sh
# Install BoxPilot's privileged helper on macOS (ADR 0006 rule 7), and make
# an account its owner (rule 4):
#
#   helper-install.sh <BoxPilot.app/Contents> <owner uid>
#
# It runs once, as root: under the administrator prompt BoxPilot shows
# (`osascript ... with administrator privileges`, whose AppleScript passes
# both values as positional arguments, each as its `quoted form`), or with
# sudo. It takes nothing but those two arguments: no environment, no
# configuration. Running it again replaces the binaries and makes the given
# account the owner; the last account an administrator authorized holds it.
#
# From BoxPilot.app it copies, all root:wheel, nothing writable by anyone
# else, into fixed paths (boxpilot_protocol::endpoint::macos):
#
#   Contents/MacOS/boxpilot-helper
#       -> /Library/PrivilegedHelperTools/io.github.glide01.boxpilot.helper  0755
#   Contents/MacOS/sing-box (the app's own sing-box, byte for byte)
#       -> /Library/Application Support/BoxPilot Helper/bin/sing-box         0755
#   Contents/Resources/Helper/manifest.json (its hash)
#       -> /Library/Application Support/BoxPilot Helper/bin/manifest.json    0644
#   Contents/Resources/Helper/io.github.glide01.boxpilot.helper.plist
#       -> /Library/LaunchDaemons/io.github.glide01.boxpilot.helper.plist    0644
#
# and creates the state directory, .../BoxPilot Helper/state, 0700, with the
# owner record in it, `owner`, 0600: the uid, in decimal. Then it replaces
# whatever daemon was loaded (launchctl bootout) with this one (launchctl
# bootstrap system). launchd creates the helper's socket and starts the
# helper when a client connects.
#
# The payload is read from the app bundle, which the user can write: a
# process of theirs could swap it during this one prompt. That is the same
# exposure as any installer's (ADR 0006, "Install-time trust"). After this,
# nothing in the helper's path is writable by anyone but root and admins.
#
# helper-uninstall.sh, beside this script, removes it again.

set -eu

LABEL='io.github.glide01.boxpilot.helper'
HELPER_PATH='/Library/PrivilegedHelperTools/io.github.glide01.boxpilot.helper'
HELPER_TOOLS_DIR='/Library/PrivilegedHelperTools'
SUPPORT_DIR='/Library/Application Support/BoxPilot Helper'
BIN_DIR='/Library/Application Support/BoxPilot Helper/bin'
SING_BOX_PATH='/Library/Application Support/BoxPilot Helper/bin/sing-box'
MANIFEST_PATH='/Library/Application Support/BoxPilot Helper/bin/manifest.json'
STATE_DIR='/Library/Application Support/BoxPilot Helper/state'
OWNER_FILE='/Library/Application Support/BoxPilot Helper/state/owner'
PLIST_PATH='/Library/LaunchDaemons/io.github.glide01.boxpilot.helper.plist'
SOCKET_PATH='/var/run/io.github.glide01.boxpilot.helper.sock'
BUNDLE_HELPER='MacOS/boxpilot-helper'
BUNDLE_SING_BOX='MacOS/sing-box'
BUNDLE_PAYLOAD_DIR='Resources/Helper'
PLIST_FILE='io.github.glide01.boxpilot.helper.plist'

PATH='/usr/bin:/bin:/usr/sbin:/sbin'
export PATH
umask 022

die() {
    printf 'helper-install: %s\n' "$*" >&2
    exit 1
}

[ "$#" -eq 2 ] || die "usage: helper-install.sh <BoxPilot.app/Contents> <owner uid>"
contents=$1
owner=$2

[ "$(/usr/bin/id -u)" -eq 0 ] || die "it must run as root"

case $contents in
    /*) ;;
    *) die "the Contents directory must be an absolute path" ;;
esac

# A uid as the helper reads one (paths::is_uid): decimal, no leading zero,
# below (uid_t)-1. Not 0: root may start anyway, and isn't an owner.
case $owner in
    '' | *[!0-9]*) die "the owner uid must be a decimal number" ;;
    0*) die "the owner uid must be an account's other than root's, without a leading zero" ;;
esac
[ "${#owner}" -le 10 ] && [ "$owner" -lt 4294967295 ] || die "the owner uid is out of range"
/usr/bin/id -un "$owner" >/dev/null 2>&1 || die "no account has uid $owner"

helper=$contents/$BUNDLE_HELPER
sing_box=$contents/$BUNDLE_SING_BOX
manifest=$contents/$BUNDLE_PAYLOAD_DIR/manifest.json
plist=$contents/$BUNDLE_PAYLOAD_DIR/$PLIST_FILE
for file in "$helper" "$sing_box" "$manifest" "$plist"; do
    [ -f "$file" ] && [ ! -L "$file" ] || die "not a file in the payload: $file"
done
/usr/bin/plutil -lint -s "$plist" || die "the payload's plist is not valid: $plist"

# None of the helper's own paths may be a link: install would write through
# it. Only root can have made one.
for path in "$HELPER_TOOLS_DIR" "$HELPER_PATH" "$SUPPORT_DIR" "$BIN_DIR" "$SING_BOX_PATH" \
    "$MANIFEST_PATH" "$STATE_DIR" "$OWNER_FILE" "$PLIST_PATH"; do
    [ ! -L "$path" ] || die "$path is a symbolic link; remove it first"
done

# The daemon that is loaded, if any, goes first: launchd sends it SIGTERM,
# and it stops its sing-box before it exits.
if /bin/launchctl print "system/$LABEL" >/dev/null 2>&1; then
    /bin/launchctl bootout "system/$LABEL" 2>/dev/null || :
    tries=0
    while /bin/launchctl print "system/$LABEL" >/dev/null 2>&1; do
        tries=$((tries + 1))
        [ "$tries" -le 40 ] || die "the old helper is still loaded after launchctl bootout"
        /bin/sleep 1
    done
fi
/bin/rm -f "$SOCKET_PATH"

# The directories. install -d sets the owner and mode of one that exists
# too, so a reinstall repairs them. /Library/PrivilegedHelperTools is the
# system's: created if missing, never changed.
if [ ! -d "$HELPER_TOOLS_DIR" ]; then
    /usr/bin/install -d -o root -g wheel -m 0755 "$HELPER_TOOLS_DIR"
fi
/usr/bin/install -d -o root -g wheel -m 0755 "$SUPPORT_DIR"
/usr/bin/install -d -o root -g wheel -m 0755 "$BIN_DIR"
/usr/bin/install -d -o root -g wheel -m 0700 "$STATE_DIR"

# The files. install replaces a file with a new one rather than writing
# into it, so a running helper keeps the binary it started from.
/usr/bin/install -o root -g wheel -m 0755 "$helper" "$HELPER_PATH"
/usr/bin/install -o root -g wheel -m 0755 "$sing_box" "$SING_BOX_PATH"
/usr/bin/install -o root -g wheel -m 0644 "$manifest" "$MANIFEST_PATH"
# The administrator vetted this payload with the prompt: a quarantine flag
# copied from the download must not stop launchd from running it.
for file in "$HELPER_PATH" "$SING_BOX_PATH"; do
    /usr/bin/xattr -d com.apple.quarantine "$file" 2>/dev/null || :
done

# The owner, written beside and renamed: the helper, which reads it for
# each connection, sees the old record or the new one, never half.
owner_new=$STATE_DIR/owner.new
/bin/rm -f "$owner_new"
(
    umask 077
    printf '%s\n' "$owner" >"$owner_new"
)
/usr/sbin/chown root:wheel "$owner_new"
/bin/chmod 0600 "$owner_new"
/bin/mv -f "$owner_new" "$OWNER_FILE"

/usr/bin/install -o root -g wheel -m 0644 "$plist" "$PLIST_PATH"
# An administrator may have disabled it before (launchctl disable persists).
/bin/launchctl enable "system/$LABEL"
tries=0
until /bin/launchctl bootstrap system "$PLIST_PATH"; do
    tries=$((tries + 1))
    [ "$tries" -le 5 ] || die "launchctl bootstrap system $PLIST_PATH failed"
    /bin/sleep 1
done
/bin/launchctl print "system/$LABEL" >/dev/null 2>&1 || die "the helper is not loaded"
printf 'helper-install: the helper is installed, and uid %s owns it\n' "$owner"
