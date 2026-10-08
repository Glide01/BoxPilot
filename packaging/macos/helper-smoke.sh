#!/bin/sh
# Smoke-test BoxPilot's privileged helper on a real Mac (ADR 0006,
# docs/helper-macos-checklist.md): install it from the DMG CI just built,
# check what it installed, drive the daemon with the smoke client
# (crates/boxpilot-helper/examples/mac_smoke.rs) as the owner, as root and
# as another account, break the install on purpose, kill it, and uninstall.
#
#   packaging/macos/helper-smoke.sh <step> [<BoxPilot.dmg> <mac_smoke>]
#
# e.g. packaging/macos/helper-smoke.sh install \
#        release/BoxPilot-1.13.5-macos-arm64.dmg \
#        target/aarch64-apple-darwin/release/examples/mac_smoke
#
# Run it as an administrator with passwordless sudo, on a Mac you can throw
# away: it installs a launch daemon, creates and deletes a local account,
# brings TUN up for a few seconds at a time (this machine's own traffic goes
# through it then), sets and clears the system proxy, and tampers with the
# installed files (restoring each). CI runs it on GitHub's Apple-silicon
# runner, one step per CI step:
#
#   install          copy BoxPilot.app out of the DMG and run its
#                    helper-install.sh with sudo, standing in for the
#                    administrator prompt; the runner's account is the owner
#   inspect          the installed paths' owners and modes, the files against
#                    the app's, the manifest's hash, the launchd-created
#                    socket, launchctl print
#   protocol         the smoke client as the owner and as root: hello, a
#                    refused start, connection slots, the write deadline, and
#                    three TUN runs (one with probes, and checks from outside
#                    while it runs: utun, the routes, sing-box as the helper's
#                    child in its process group, its listeners)
#   idle-exit        the daemon exits a minute after the last connection, with
#                    code 0, and the next connection starts it again
#   other-user       a fresh standard account: read-only, refused as
#                    unauthorized, capped, kept out of the state directory; and
#                    a reinstall naming it makes it the owner instead
#   broken-install   a tampered sing-box, a bin directory writable by its
#                    group or by others, a group-writable sing-box, a readable
#                    state directory: each stops the helper with its exit code;
#                    a malformed owner record: nobody may start
#   kill-helper      SIGKILL to the helper during TUN: sing-box goes with it
#   system-proxy     TUN with the system proxy: sing-box sets it and unsets it
#                    on stop; killed outright, the next helper start resets it
#   uninstall        helper-uninstall.sh: the daemon and its paths are gone,
#                    the state directory stays; --remove-state removes it
#   logs             the helper's log, launchctl print, the system log for the
#                    label, and the smoke client's output (CI runs it always)
#
# Each check fails the step with what it saw. Everything goes in
# /private/tmp/boxpilot-helper-smoke, which the other account can reach.

set -eu

# boxpilot_protocol::endpoint (tests/macos_packaging.rs holds these to it).
LABEL='io.github.glide01.boxpilot.helper'
HELPER_PATH='/Library/PrivilegedHelperTools/io.github.glide01.boxpilot.helper'
SUPPORT_DIR='/Library/Application Support/BoxPilot Helper'
BIN_DIR='/Library/Application Support/BoxPilot Helper/bin'
SING_BOX_PATH='/Library/Application Support/BoxPilot Helper/bin/sing-box'
MANIFEST_PATH='/Library/Application Support/BoxPilot Helper/bin/manifest.json'
STATE_DIR='/Library/Application Support/BoxPilot Helper/state'
OWNER_FILE='/Library/Application Support/BoxPilot Helper/state/owner'
LOG_FILE='/Library/Application Support/BoxPilot Helper/state/helper.log'
PLIST_PATH='/Library/LaunchDaemons/io.github.glide01.boxpilot.helper.plist'
SOCKET_PATH='/var/run/io.github.glide01.boxpilot.helper.sock'
BUNDLE_PAYLOAD_DIR='Resources/Helper'
INSTALL_SCRIPT='helper-install.sh'
UNINSTALL_SCRIPT='helper-uninstall.sh'
EXIT_OK='0'
EXIT_HELPER_DIR_REFUSED='10'
EXIT_STATE_DIR_REFUSED='11'
EXIT_MANIFEST_REFUSED='12'

PATH='/usr/bin:/bin:/usr/sbin:/sbin'
export PATH

WORK=/private/tmp/boxpilot-helper-smoke
LOGS=$WORK/logs
APP=$WORK/BoxPilot.app
CONTENTS=$APP/Contents
SMOKE=$WORK/mac_smoke
OTHER_USER=bpsmoke
OTHER_DIR=$WORK/other

step=${1:-}
dmg=${2:-}
smoke_source=${3:-}

# ---- Saying what happened ----

fail() {
    if [ "${GITHUB_ACTIONS:-}" = true ]; then
        printf '::error title=helper smoke (%s)::%s\n' "$step" "$1"
    fi
    printf 'helper smoke %s: FAILED: %s\n' "$step" "$1" >&2
    exit 1
}

note() {
    if [ "${GITHUB_ACTIONS:-}" = true ]; then
        printf '::warning title=helper smoke (%s)::%s\n' "$step" "$1"
    fi
    printf 'note: %s\n' "$1"
}

ok() {
    printf 'ok: %s\n' "$1"
}

# ---- The smoke client ----

smoke() {
    printf -- '---- mac_smoke %s (as %s)\n' "$*" "$(id -un)"
    "$SMOKE" "$@" || fail "mac_smoke $1 failed (above)"
}

smoke_as_root() {
    printf -- '---- mac_smoke %s (as root)\n' "$*"
    sudo "$SMOKE" "$@" || fail "mac_smoke $1 as root failed (above)"
}

smoke_as_other() {
    printf -- '---- mac_smoke %s (as %s)\n' "$*" "$OTHER_USER"
    (cd / && sudo -u "$OTHER_USER" "$SMOKE" "$@") || fail "mac_smoke $1 as $OTHER_USER failed (above)"
}

# A smoke command in the background, its output in $LOGS/<name>.txt; its PID
# in $bg_pid.
smoke_background() {
    name=$1
    shift
    printf -- '---- mac_smoke %s, in the background (%s.txt)\n' "$*" "$name"
    "$SMOKE" "$@" >"$LOGS/$name.txt" 2>&1 &
    bg_pid=$!
}

# Wait for the ready file $1 of the background smoke $2, at most $3 s.
wait_ready() {
    waited=0
    while [ ! -f "$1" ]; do
        if ! kill -0 "$2" 2>/dev/null; then
            cat "$LOGS/$name.txt" || :
            fail "the smoke client ended before it was ready (above)"
        fi
        waited=$((waited + 1))
        [ "$waited" -le "$3" ] || fail "the smoke client wasn't ready within $3 s"
        sleep 1
    done
    printf 'ready: %s\n' "$(tr '\n' ' ' <"$1")"
}

# The value of key $2 in ready file $1.
ready_value() {
    sed -n "s/^$2=//p" "$1"
}

# Wait for background smoke $1 (output $LOGS/$2.txt) and show its output; it
# must have succeeded.
finish_background() {
    if wait "$1"; then
        cat "$LOGS/$2.txt"
    else
        cat "$LOGS/$2.txt" || :
        fail "the background smoke client failed (above)"
    fi
}

# ---- The daemon ----

launchd_print() {
    sudo launchctl print "system/$LABEL" 2>/dev/null
}

helper_pid() {
    launchd_print | awk '$1 == "pid" && $2 == "=" { print $3; exit }'
}

last_exit_code() {
    launchd_print | sed -n 's/^[[:space:]]*last exit code = \([0-9-]*\).*/\1/p' | head -n 1
}

# Wait at most $1 s for the helper to stop running.
wait_helper_stopped() {
    waited=0
    while [ -n "$(helper_pid)" ]; do
        waited=$((waited + 1))
        [ "$waited" -le "$1" ] || fail "the helper still runs after $1 s"
        sleep 1
    done
}

# Stop a running helper (launchd's SIGTERM, as at bootout), and wait.
stop_helper() {
    if [ -n "$(helper_pid)" ]; then
        sudo launchctl kill SIGTERM "system/$LABEL" || :
        wait_helper_stopped 40
    fi
}

helper_log_tail() {
    sudo tail -n "${1:-40}" "$LOG_FILE" 2>/dev/null || :
}

# ---- Files ----

# "owner:group mode type", e.g. "root:wheel 755 Directory".
describe() {
    sudo stat -f '%Su:%Sg %Lp %HT' "$1"
}

expect_stat() {
    got=$(describe "$1") || fail "$1 is missing"
    [ "$got" = "$2" ] || fail "$1 is $got, not $2"
    ok "$1: $got"
}

# Every directory from / to $1, with its ACL entries if any (the helper
# judges owners and mode bits; this shows what else is there), for the
# record.
show_chain() {
    path=$1
    while :; do
        sudo ls -lde "$path"
        [ "$path" != / ] || break
        path=$(dirname "$path")
    done
}

# ---- TUN, from outside ----

TUN_ADDRESS=172.18.0.1

# The utun interface holding the TUN address, if any.
tun_interface() {
    ifconfig | awk -v address="$TUN_ADDRESS" '
        /^[a-z]/ { name = $1; sub(":$", "", name) }
        $1 == "inet" && $2 == address { print name; exit }'
}

route_interface() {
    route -n get "$1" 2>/dev/null | awk '$1 == "interface:" { print $2; exit }'
}

# The helper's (pid $1) one child, sing-box, in $sing_box; fails the step
# unless there is exactly one.
find_sing_box() {
    children=$(pgrep -P "$1" || :)
    count=$(printf '%s\n' "$children" | grep -c . || :)
    [ "$count" -eq 1 ] || fail "the helper (pid $1) has $count children, not 1: $children"
    sing_box=$children
}

# sing-box (pid $1) is the helper's (pid $2) child, in its process group, as
# root, running the installed binary from a run directory.
assert_sing_box_under_helper() {
    info=$(ps -o ppid= -o pgid= -o uid= -p "$1") || fail "sing-box (pid $1) is gone"
    read -r ppid pgid uid <<EOF
$info
EOF
    [ "$ppid" = "$2" ] || fail "sing-box's parent is $ppid, not the helper ($2)"
    [ "$pgid" = "$2" ] || fail "sing-box's process group is $pgid, not the helper's ($2)"
    [ "$uid" = 0 ] || fail "sing-box runs as uid $uid, not root"
    exe=$(sudo lsof -a -p "$1" -d txt -Fn 2>/dev/null | sed -n 's/^n//p' | head -n 1)
    [ "$exe" = "$SING_BOX_PATH" ] || fail "sing-box runs $exe, not $SING_BOX_PATH"
    cwd=$(sudo lsof -a -p "$1" -d cwd -Fn 2>/dev/null | sed -n 's/^n//p' | head -n 1)
    case $cwd in
        "$STATE_DIR"/runs/*) ;;
        *) fail "sing-box's working directory is $cwd, not a run directory" ;;
    esac
    ok "sing-box (pid $1) is the helper's child in its process group, as root, running $exe in $cwd"
}

# The helper has no network socket at all; sing-box listens on TCP only on
# loopback and the TUN interface's own /30.
assert_listeners() {
    if sudo lsof -nP -a -p "$1" -i >/dev/null 2>&1; then
        sudo lsof -nP -a -p "$1" -i || :
        fail "the helper (pid $1) has network sockets (above)"
    fi
    listening=$(sudo lsof -nP -a -p "$2" -iTCP -sTCP:LISTEN -Fn 2>/dev/null | sed -n 's/^n//p')
    for address in $listening; do
        case $address in
            127.0.0.1:* | '[::1]:'* | 172.18.0.1:* | 172.18.0.2:*) ;;
            *) fail "sing-box listens on $address, beyond loopback and the TUN interface" ;;
        esac
    done
    ok "the helper has no network socket; sing-box listens on $(printf '%s' "$listening" | tr '\n' ' ')"
}

assert_tun_up() {
    interface=$(tun_interface)
    [ -n "$interface" ] || fail "no interface holds $TUN_ADDRESS while TUN is up"
    routed=$(route_interface 1.1.1.1)
    [ "$routed" = "$interface" ] || fail "1.1.1.1 is routed through $routed, not $interface"
    ok "$interface holds $TUN_ADDRESS, and 1.1.1.1 is routed through it"
}

# TUN is down: no interface with its address, no route through one, no
# sing-box under the helper, no run directory (unless $1 is allow-runs).
assert_tun_down() {
    waited=0
    while [ -n "$(tun_interface)" ]; do
        waited=$((waited + 1))
        [ "$waited" -le 15 ] || fail "$(tun_interface) still holds $TUN_ADDRESS after TUN went down"
        sleep 1
    done
    routed=$(route_interface 1.1.1.1)
    case $routed in
        utun*) fail "1.1.1.1 is still routed through $routed" ;;
    esac
    if pgrep -f "$SING_BOX_PATH" >/dev/null; then
        pgrep -lf "$SING_BOX_PATH" || :
        fail "a sing-box of the helper's still runs"
    fi
    runs=$(sudo ls -A "$STATE_DIR/runs" 2>/dev/null || :)
    if [ -n "$runs" ] && [ "${1:-}" != allow-runs ]; then
        fail "run directories are left: $runs"
    fi
    ok "TUN is down: no $TUN_ADDRESS, 1.1.1.1 through ${routed:-?}, no sing-box"
}

# The network services' names, one per line.
network_services() {
    networksetup -listallnetworkservices | sed '1d; s/^\*//'
}

# Whether any service has a proxy enabled on 127.0.0.1:$1.
proxy_set_on() {
    found=no
    services=$(network_services)
    old_ifs=$IFS
    IFS='
'
    for service in $services; do
        for getter in -getwebproxy -getsecurewebproxy -getsocksfirewallproxy; do
            state=$(networksetup "$getter" "$service" 2>/dev/null || :)
            if printf '%s\n' "$state" | grep -qx 'Enabled: Yes' &&
                printf '%s\n' "$state" | grep -qx 'Server: 127.0.0.1' &&
                printf '%s\n' "$state" | grep -qx "Port: $1"; then
                printf '  %s %s: 127.0.0.1:%s\n' "$service" "$getter" "$1"
                found=yes
            fi
        done
    done
    IFS=$old_ifs
    [ "$found" = yes ]
}

flush_dns() {
    sudo dscacheutil -flushcache || :
    sudo killall -HUP mDNSResponder || :
}

# Turn off every proxy still on 127.0.0.1:$1: a proxy left pointing at a
# sing-box that is gone would cut this runner's own traffic (its agent
# follows the system proxy).
reset_proxy_on() {
    services=$(network_services)
    old_ifs=$IFS
    IFS='
'
    for service in $services; do
        for kind in web secureweb socksfirewall; do
            state=$(networksetup "-get${kind}proxy" "$service" 2>/dev/null || :)
            if printf '%s\n' "$state" | grep -qx 'Enabled: Yes' &&
                printf '%s\n' "$state" | grep -qx 'Server: 127.0.0.1' &&
                printf '%s\n' "$state" | grep -qx "Port: $1"; then
                sudo networksetup "-set${kind}proxystate" "$service" off || :
            fi
        done
    done
    IFS=$old_ifs
}

# ---- Steps ----

step_install() {
    [ -f "$dmg" ] || fail "no DMG at '$dmg'"
    [ -f "$smoke_source" ] || fail "no smoke client at '$smoke_source'"
    for path in "$HELPER_PATH" "$PLIST_PATH" "$SUPPORT_DIR"; do
        [ ! -e "$path" ] || note "$path exists before the install"
    done
    sudo rm -rf "$WORK"
    mkdir -p "$WORK" "$LOGS"
    chmod 0755 "$WORK" "$LOGS"
    mount=$WORK/dmg
    mkdir "$mount"
    hdiutil attach -nobrowse -readonly -noautoopen -mountpoint "$mount" "$dmg" >/dev/null
    ditto "$mount/BoxPilot.app" "$APP"
    hdiutil detach "$mount" >/dev/null || hdiutil detach -force "$mount" >/dev/null
    rmdir "$mount"
    codesign --verify --strict --verbose=2 "$APP"
    cp "$smoke_source" "$SMOKE"
    chmod 0755 "$SMOKE"
    printf 'installing from %s as uid %s\n' "$CONTENTS" "$(id -u)"
    sudo /bin/sh "$CONTENTS/$BUNDLE_PAYLOAD_DIR/$INSTALL_SCRIPT" "$CONTENTS" "$(id -u)" ||
        fail "$INSTALL_SCRIPT failed (above)"
    launchd_print >/dev/null || fail "launchctl print system/$LABEL fails after the install"
    # Idempotent: the same install again.
    sudo /bin/sh "$CONTENTS/$BUNDLE_PAYLOAD_DIR/$INSTALL_SCRIPT" "$CONTENTS" "$(id -u)" ||
        fail "$INSTALL_SCRIPT failed the second time (above)"
    # Values it must refuse, before touching anything.
    for bad in 0 -1 '' 501x 0501; do
        if sudo /bin/sh "$CONTENTS/$BUNDLE_PAYLOAD_DIR/$INSTALL_SCRIPT" "$CONTENTS" "$bad" 2>/dev/null; then
            fail "$INSTALL_SCRIPT took the owner uid '$bad'"
        fi
    done
    if sudo /bin/sh "$CONTENTS/$BUNDLE_PAYLOAD_DIR/$INSTALL_SCRIPT" "Contents" "$(id -u)" 2>/dev/null; then
        fail "$INSTALL_SCRIPT took a relative Contents path"
    fi
    launchd_print >/dev/null || fail "the refused installs unloaded the helper"
    ok "installed, twice, and malformed arguments were refused"
}

step_inspect() {
    expect_stat "$HELPER_PATH" "root:wheel 755 Regular File"
    expect_stat "$SUPPORT_DIR" "root:wheel 755 Directory"
    expect_stat "$BIN_DIR" "root:wheel 755 Directory"
    expect_stat "$SING_BOX_PATH" "root:wheel 755 Regular File"
    expect_stat "$MANIFEST_PATH" "root:wheel 644 Regular File"
    expect_stat "$STATE_DIR" "root:wheel 700 Directory"
    expect_stat "$OWNER_FILE" "root:wheel 600 Regular File"
    expect_stat "$PLIST_PATH" "root:wheel 644 Regular File"
    # launchd makes the socket root's, mode 0666. Its group is launchd's
    # choice (daemon on macOS 14, whatever SockPathGroup says), which 0666
    # makes moot: the helper authorizes each connection by its uid.
    got=$(describe "$SOCKET_PATH") || fail "$SOCKET_PATH is missing"
    case $got in
        root:*' 666 Socket') ok "$SOCKET_PATH: $got" ;;
        *) fail "$SOCKET_PATH is $got, not root's, mode 666, a socket" ;;
    esac

    owner=$(sudo cat "$OWNER_FILE")
    [ "$owner" = "$(id -u)" ] || fail "the owner record says '$owner', not $(id -u)"
    ok "the owner record names uid $owner"
    in_bin=$(cd "$BIN_DIR" && find . -mindepth 1 -maxdepth 1 | sort | tr '\n' ' ')
    [ "$in_bin" = "./manifest.json ./sing-box " ] || fail "$BIN_DIR holds $in_bin"
    cmp "$SING_BOX_PATH" "$CONTENTS/MacOS/sing-box" || fail "the installed sing-box isn't the app's"
    cmp "$HELPER_PATH" "$CONTENTS/MacOS/boxpilot-helper" || fail "the installed helper isn't the app's"
    cmp "$PLIST_PATH" "$CONTENTS/$BUNDLE_PAYLOAD_DIR/$LABEL.plist" || fail "the installed plist isn't the app's"
    plutil -lint "$PLIST_PATH"
    hash=$(shasum -a 256 "$SING_BOX_PATH" | cut -d ' ' -f 1)
    grep -q "\"sha256\": *\"$hash\"" "$MANIFEST_PATH" || fail "the manifest doesn't hash the installed sing-box ($hash)"
    ok "sing-box, the helper and the plist are the app's; the manifest hashes sing-box ($hash)"

    print=$(launchd_print) || fail "launchctl print system/$LABEL fails"
    printf '%s\n' "$print" >"$LOGS/launchctl-print-inspect.txt"
    printf '%s\n' "$print" | grep -q "$HELPER_PATH" || fail "launchctl print doesn't name $HELPER_PATH"
    printf '%s\n' "$print" | grep -q "$SOCKET_PATH" || note "launchctl print doesn't name $SOCKET_PATH"
    printf '%s\n' "$print" | sed -n '1,60p'
    printf 'the paths, from /:\n'
    show_chain "$HELPER_PATH"
    show_chain "$SING_BOX_PATH"
    show_chain "$OWNER_FILE"
}

# One TUN run with the probes, checked from outside while it is up.
probed_tun_run() {
    ready=$WORK/tun.ready
    release=$WORK/tun.release
    rm -f "$ready" "$release"
    # The probes resolve a name through TUN: not from the cache.
    flush_dns
    smoke_background tun-probes tun --probes --ready-file "$ready" --release-file "$release" --end stop
    wait_ready "$ready" "$bg_pid" 180
    helper=$(helper_pid)
    [ -n "$helper" ] || fail "launchd shows no helper running while TUN is up"
    # LOCAL_PEERPID may name launchd, which created the listening socket.
    [ "$(ready_value "$ready" helper_pid)" = "$helper" ] ||
        note "the socket's peer is pid $(ready_value "$ready" helper_pid), launchd's job is $helper"
    if ! (
        assert_tun_up
        find_sing_box "$helper"
        assert_sing_box_under_helper "$sing_box" "$helper"
        assert_listeners "$helper" "$sing_box"
        runs=$(sudo ls -A "$STATE_DIR/runs" | grep -c . || :)
        [ "$runs" -eq 1 ] || fail "while one sing-box runs, $STATE_DIR/runs holds $runs entries"
        ok "one run directory"
    ); then
        touch "$release"
        wait "$bg_pid" || :
        cat "$LOGS/tun-probes.txt"
        fail "a check failed while TUN was up (above)"
    fi
    touch "$release"
    finish_background "$bg_pid" tun-probes
    assert_tun_down
}

# Everything in the state directory is root's alone.
assert_state_private() {
    shared=$(sudo find "$STATE_DIR" -perm +0077 -print)
    [ -z "$shared" ] || fail "in the state directory, these give their group or others access: $shared"
    foreign=$(sudo find "$STATE_DIR" ! -user root -print)
    [ -z "$foreign" ] || fail "in the state directory, these aren't root's: $foreign"
    ok "everything in $STATE_DIR is root's alone"
    sudo ls -lR "$STATE_DIR"
}

step_protocol() {
    hash=$(shasum -a 256 "$SING_BOX_PATH" | cut -d ' ' -f 1)
    version=$(sed -n 's/.*"version": *"\([^"]*\)".*/\1/p' "$MANIFEST_PATH" | head -n 1)
    smoke hello --expect start --sha256 "$hash" --sing-box-version "$version"
    smoke_as_root hello --expect start
    smoke refused
    smoke slots
    smoke write-deadline
    probed_tun_run
    smoke tun --end close
    assert_tun_down
    smoke tun --end mid-frame
    assert_tun_down
    assert_state_private
}

step_idle_exit() {
    if [ -z "$(helper_pid)" ]; then
        smoke hello --expect start
    fi
    before=$(helper_pid)
    printf 'waiting for the helper (pid %s) to exit by itself, 60 s after its last connection\n' "$before"
    wait_helper_stopped 100
    code=$(last_exit_code)
    [ "$code" = "$EXIT_OK" ] || fail "the idle helper exited with code '$code', not $EXIT_OK"
    helper_log_tail 20 | grep -q 'idle for' || fail "the helper exited, but its log doesn't say it was idle"
    ok "the helper exited by itself with code 0"
    smoke hello --expect start
    after=$(helper_pid)
    [ -n "$after" ] && [ "$after" != "$before" ] || fail "the next client didn't start a new helper ('$after')"
    ok "the next client started it again (pid $after)"
}

remove_other_user() {
    if dscl . -read "/Users/$OTHER_USER" >/dev/null 2>&1; then
        sudo dscl . -delete "/Users/$OTHER_USER" || :
        sudo dscacheutil -flushcache || :
    fi
}

create_other_user() {
    remove_other_user
    uid=600
    while dscl . -search /Users UniqueID "$uid" | grep -q .; do
        uid=$((uid + 1))
    done
    sudo dscl . -create "/Users/$OTHER_USER"
    sudo dscl . -create "/Users/$OTHER_USER" UniqueID "$uid"
    sudo dscl . -create "/Users/$OTHER_USER" PrimaryGroupID 20
    sudo dscl . -create "/Users/$OTHER_USER" UserShell /usr/bin/false
    sudo dscl . -create "/Users/$OTHER_USER" NFSHomeDirectory /var/empty
    sudo dscl . -create "/Users/$OTHER_USER" RealName 'BoxPilot smoke test'
    sudo dscacheutil -flushcache
    waited=0
    until [ "$(id -u "$OTHER_USER" 2>/dev/null)" = "$uid" ]; do
        waited=$((waited + 1))
        [ "$waited" -le 30 ] || fail "the account $OTHER_USER (uid $uid) didn't appear"
        sleep 1
    done
    other_uid=$uid
    ok "created the standard account $OTHER_USER, uid $other_uid"
}

step_other_user() {
    trap remove_other_user EXIT
    create_other_user
    rm -rf "$OTHER_DIR"
    mkdir -p "$OTHER_DIR"
    chmod 0777 "$OTHER_DIR"

    smoke_as_other hello --expect readonly
    smoke_as_other unauthorized
    smoke_as_other denied --dir "$STATE_DIR" --file "$LOG_FILE"
    if sudo -u "$OTHER_USER" ls "$STATE_DIR" >/dev/null 2>&1; then
        fail "$OTHER_USER can list $STATE_DIR"
    fi

    # Four read-only connections held: the owner still gets in.
    ready=$OTHER_DIR/slots.ready
    release=$OTHER_DIR/slots.release
    rm -f "$ready" "$release"
    printf -- '---- mac_smoke readonly-slots (as %s), in the background\n' "$OTHER_USER"
    (cd / && sudo -u "$OTHER_USER" "$SMOKE" readonly-slots --ready-file "$ready" --release-file "$release") \
        >"$LOGS/readonly-slots.txt" 2>&1 &
    slots_pid=$!
    name=readonly-slots
    wait_ready "$ready" "$slots_pid" 120
    smoke hello --expect start
    touch "$release"
    finish_background "$slots_pid" readonly-slots

    # Another install, naming this account, makes it the owner instead.
    sudo /bin/sh "$CONTENTS/$BUNDLE_PAYLOAD_DIR/$INSTALL_SCRIPT" "$CONTENTS" "$other_uid" ||
        fail "the install naming $OTHER_USER failed"
    smoke_as_other hello --expect start
    smoke hello --expect readonly
    smoke_as_root hello --expect start
    sudo /bin/sh "$CONTENTS/$BUNDLE_PAYLOAD_DIR/$INSTALL_SCRIPT" "$CONTENTS" "$(id -u)" ||
        fail "the install naming $(id -un) again failed"
    smoke hello --expect start
    smoke_as_other hello --expect readonly
    ok "each install's account owns the helper, and only that one"

    remove_other_user
    trap - EXIT
}

# Start the (stopped) helper with launchd's kickstart, and expect it to exit
# with code $2, saying why in its log, or the system log ($3: log or syslog).
expect_refusal() {
    what=$1
    code=$2
    sudo launchctl kickstart "system/$LABEL" || :
    waited=0
    until [ -z "$(helper_pid)" ] && [ "$(last_exit_code)" = "$code" ]; do
        waited=$((waited + 1))
        if [ "$waited" -gt 30 ]; then
            launchd_print | sed -n '1,40p' || :
            helper_log_tail 10
            fail "$what: the helper didn't exit with code $code (last exit code '$(last_exit_code)')"
        fi
        sleep 1
    done
    if [ "$3" = log ]; then
        helper_log_tail 5 | grep -q "refusing to run (exit code $code)" ||
            fail "$what: the helper's log doesn't say why it refused"
    else
        said=$(sudo log show --last 2m --style compact --predicate "process == \"$LABEL\"" 2>/dev/null |
            grep "refusing to run (exit code $code)" || :)
        [ -n "$said" ] || note "$what: the system log doesn't show the refusal"
    fi
    ok "$what: the helper refused to run, exit code $code"
}

# After a break is undone: the owner's hello starts the helper again.
expect_recovered() {
    smoke hello --expect start
    stop_helper
    code=$(last_exit_code)
    [ "$code" = "$EXIT_OK" ] || fail "after recovering, the helper stopped with code '$code'"
}

step_broken_install() {
    stop_helper

    # A sing-box that isn't the one the manifest hashes: one byte more.
    printf 'x' | sudo tee -a "$SING_BOX_PATH" >/dev/null
    expect_refusal "a tampered sing-box" "$EXIT_MANIFEST_REFUSED" log
    sudo install -o root -g wheel -m 0755 "$CONTENTS/MacOS/sing-box" "$SING_BOX_PATH"
    expect_recovered

    # A bin directory its group may write, when that group is staff.
    sudo chgrp staff "$BIN_DIR"
    sudo chmod g+w "$BIN_DIR"
    expect_refusal "a bin directory staff may write" "$EXIT_HELPER_DIR_REFUSED" log
    sudo chgrp wheel "$BIN_DIR"
    sudo chmod 0755 "$BIN_DIR"
    expect_recovered

    # A bin directory anyone may write.
    sudo chmod o+w "$BIN_DIR"
    expect_refusal "a bin directory others may write" "$EXIT_HELPER_DIR_REFUSED" log
    sudo chmod 0755 "$BIN_DIR"
    expect_recovered

    # A sing-box its group may write, whatever the group.
    sudo chmod g+w "$SING_BOX_PATH"
    expect_refusal "a group-writable sing-box" "$EXIT_HELPER_DIR_REFUSED" log
    sudo chmod 0755 "$SING_BOX_PATH"
    expect_recovered

    # A state directory others may read: its log can't be opened then, so
    # the reason goes to the system log.
    sudo chmod o+rx "$STATE_DIR"
    expect_refusal "a state directory others may read" "$EXIT_STATE_DIR_REFUSED" syslog
    sudo chmod 0700 "$STATE_DIR"
    expect_recovered

    # A malformed owner record: the helper runs, and nobody may start, root
    # included.
    printf 'not a uid\n' | sudo tee "$OWNER_FILE" >/dev/null
    smoke hello --expect readonly
    smoke_as_root hello --expect readonly
    helper_log_tail 10 | grep -q 'nobody may start' || fail "the helper's log doesn't say nobody may start"
    printf '%s\n' "$(id -u)" | sudo tee "$OWNER_FILE" >/dev/null
    expect_stat "$OWNER_FILE" "root:wheel 600 Regular File"
    smoke hello --expect start
    ok "a malformed owner record lets nobody start, and the restored one counts at once"
}

step_kill_helper() {
    ready=$WORK/kill.ready
    rm -f "$ready"
    smoke_background tun-helper-killed tun --ready-file "$ready" --end helper-killed
    killed_pid=$bg_pid
    wait_ready "$ready" "$killed_pid" 180
    helper=$(helper_pid)
    find_sing_box "$helper"
    assert_sing_box_under_helper "$sing_box" "$helper"
    printf 'killing the helper (pid %s) with SIGKILL while its sing-box (pid %s) runs\n' "$helper" "$sing_box"
    sudo kill -9 "$helper"
    waited=0
    while ps -p "$sing_box" >/dev/null 2>&1; do
        waited=$((waited + 1))
        if [ "$waited" -gt 20 ]; then
            ps -o pid,ppid,pgid,uid,command -p "$sing_box" || :
            sudo kill -9 "$sing_box" || :
            fail "sing-box (pid $sing_box) outlived the helper by 20 s"
        fi
        sleep 1
    done
    ok "sing-box went with the helper ($waited s)"
    finish_background "$killed_pid" tun-helper-killed
    # The helper died with its run: its marker and run directory wait for its
    # next start, which clears them.
    assert_tun_down allow-runs
    smoke hello --expect start
    left=$(sudo ls -A "$STATE_DIR/runs" || :)
    [ -z "$left" ] || fail "after the helper started again, run directories are left: $left"
    if sudo test -e "$STATE_DIR/running"; then
        fail "after the helper started again, its run marker is left"
    fi
    helper_log_tail 30 | grep -q 'the last helper stopped while sing-box ran' ||
        fail "the restarted helper's log doesn't say it cleaned up after the last one"
    ok "the next helper cleaned up after the one that was killed"
}

step_system_proxy() {
    # sing-box sets the proxy and unsets it itself on a clean stop.
    ready=$WORK/proxy.ready
    release=$WORK/proxy.release
    rm -f "$ready" "$release"
    smoke_background tun-proxy tun --system-proxy --ready-file "$ready" --release-file "$release" --end stop
    proxy_pid=$bg_pid
    wait_ready "$ready" "$proxy_pid" 180
    port=$(ready_value "$ready" proxy_port)
    trap 'reset_proxy_on "$port"' EXIT
    if ! proxy_set_on "$port"; then
        touch "$release"
        wait "$proxy_pid" || :
        cat "$LOGS/tun-proxy.txt"
        networksetup -listallhardwareports || :
        fail "while TUN runs with the system proxy, no network service's proxy is 127.0.0.1:$port"
    fi
    ok "sing-box set the system proxy to 127.0.0.1:$port"
    touch "$release"
    finish_background "$proxy_pid" tun-proxy
    if proxy_set_on "$port"; then
        fail "after a clean stop, the system proxy still points at 127.0.0.1:$port"
    fi
    ok "after a clean stop, the system proxy is off"
    assert_tun_down

    # Killed outright, helper and sing-box at once: nobody unset the proxy;
    # the next helper start resets it, by the run marker.
    ready=$WORK/proxy-killed.ready
    rm -f "$ready"
    smoke_background tun-proxy-killed tun --system-proxy --ready-file "$ready" --end helper-killed
    proxy_pid=$bg_pid
    wait_ready "$ready" "$proxy_pid" 180
    port=$(ready_value "$ready" proxy_port)
    trap 'reset_proxy_on "$port"' EXIT
    proxy_set_on "$port" || fail "while TUN runs with the system proxy, no proxy is 127.0.0.1:$port"
    helper=$(helper_pid)
    find_sing_box "$helper"
    printf 'killing the helper (pid %s) and sing-box (pid %s) together with SIGKILL\n' "$helper" "$sing_box"
    sudo kill -9 "$helper" "$sing_box"
    finish_background "$proxy_pid" tun-proxy-killed
    if proxy_set_on "$port"; then
        ok "the killed sing-box left the proxy on, as expected"
    else
        note "the system proxy was off before the helper restarted (sing-box got to unset it?)"
    fi
    smoke hello --expect start
    if proxy_set_on "$port"; then
        fail "the restarted helper didn't reset the system proxy 127.0.0.1:$port"
    fi
    helper_log_tail 30 | grep -q 'the last helper stopped while sing-box ran' ||
        fail "the restarted helper's log doesn't say it cleaned up after the last one"
    ok "the restarted helper reset the system proxy the killed run left"
    assert_tun_down
}

step_uninstall() {
    stop_helper
    # The logs step runs after the state directory is gone. (The file is
    # the runner's: the redirect is meant to be made without sudo.)
    # shellcheck disable=SC2024
    sudo cat "$LOG_FILE.1" "$LOG_FILE" >"$LOGS/helper.log.txt" 2>/dev/null || :
    output=$(sudo /bin/sh "$CONTENTS/$BUNDLE_PAYLOAD_DIR/$UNINSTALL_SCRIPT") ||
        fail "$UNINSTALL_SCRIPT failed: $output"
    printf '%s\n' "$output"
    printf '%s\n' "$output" | grep -q 'stays' || fail "$UNINSTALL_SCRIPT doesn't say the state directory stays"
    if launchd_print >/dev/null; then
        fail "launchctl print system/$LABEL still works after the uninstall"
    fi
    for path in "$HELPER_PATH" "$PLIST_PATH" "$BIN_DIR" "$SOCKET_PATH"; do
        if sudo test -e "$path"; then
            fail "$path is left after the uninstall"
        fi
    done
    sudo test -d "$STATE_DIR" || fail "the uninstall removed the state directory without --remove-state"
    ok "the daemon and its paths are gone; the state directory stays"
    sudo /bin/sh "$CONTENTS/$BUNDLE_PAYLOAD_DIR/$UNINSTALL_SCRIPT" --remove-state ||
        fail "$UNINSTALL_SCRIPT --remove-state failed"
    if sudo test -e "$SUPPORT_DIR"; then
        fail "$SUPPORT_DIR is left after --remove-state"
    fi
    ok "--remove-state removed $SUPPORT_DIR"
}

step_logs() {
    for log in "$LOG_FILE.1" "$LOG_FILE"; do
        if sudo test -f "$log"; then
            printf '==== %s\n' "$log"
            sudo cat "$log"
        fi
    done
    if [ -f "$LOGS/helper.log.txt" ]; then
        printf '==== the helper log, as the uninstall step found it\n'
        cat "$LOGS/helper.log.txt"
    fi
    printf '==== launchctl print system/%s\n' "$LABEL"
    launchd_print || printf '(not loaded)\n'
    printf '==== the system log for %s, the last hour\n' "$LABEL"
    sudo log show --last 1h --style compact --info --predicate \
        "process == \"$LABEL\" OR eventMessage CONTAINS \"$LABEL\"" 2>&1 | tail -n 300 || :
    for file in "$LOGS"/*.txt "$WORK"/*.ready "$OTHER_DIR"/*.ready; do
        [ -f "$file" ] || continue
        printf '==== %s\n' "$file"
        cat "$file"
    done
}

case $step in
    install) step_install ;;
    inspect) step_inspect ;;
    protocol) step_protocol ;;
    idle-exit) step_idle_exit ;;
    other-user) step_other_user ;;
    broken-install) step_broken_install ;;
    kill-helper) step_kill_helper ;;
    system-proxy) step_system_proxy ;;
    uninstall) step_uninstall ;;
    logs)
        step_logs
        exit 0
        ;;
    *) fail "unknown step '$step' (install, inspect, protocol, idle-exit, other-user, broken-install, kill-helper, system-proxy, uninstall, logs)" ;;
esac
printf 'helper smoke %s: every check held\n' "$step"
