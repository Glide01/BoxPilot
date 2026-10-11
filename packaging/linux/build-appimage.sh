#!/usr/bin/env bash
# Package BoxPilot as an x86_64 AppImage.
#
#   packaging/linux/build-appimage.sh <release-binary> <sing-box-binary> <version> <out-dir>
#
# e.g. packaging/linux/build-appimage.sh \
#        target/x86_64-unknown-linux-gnu/release/box_pilot_gui \
#        release/sing-box 1.13.5 release
#
# Writes <out-dir>/BoxPilot-<version>-x86_64.AppImage. The AppDir is assembled
# in a temporary directory and removed afterwards:
#
#   AppRun                  execs usr/bin/box-pilot
#   boxpilot.desktop        packaging/linux/boxpilot.desktop
#   boxpilot.png, .DirIcon  assets/icon.png
#   usr/bin/box-pilot       the BoxPilot release binary
#   usr/bin/sing-box        the bundled sing-box; BoxPilot looks for it next
#                           to its own executable
#
# appimagetool and the AppImage runtime are pinned and checksum-verified, so
# the build never picks up a moving "continuous" release. Set APPIMAGETOOL to
# an appimagetool AppImage, or APPIMAGE_RUNTIME to a type2 runtime file, to
# use local copies instead of downloading them. appimagetool runs with
# --appimage-extract-and-run, so FUSE is not needed (CI containers lack it).

set -euo pipefail

APPIMAGETOOL_VERSION="1.9.1"
APPIMAGETOOL_URL="https://github.com/AppImage/appimagetool/releases/download/${APPIMAGETOOL_VERSION}/appimagetool-x86_64.AppImage"
APPIMAGETOOL_SHA256="ed4ce84f0d9caff66f50bcca6ff6f35aae54ce8135408b3fa33abfc3cb384eb0"

RUNTIME_VERSION="20251108"
RUNTIME_URL="https://github.com/AppImage/type2-runtime/releases/download/${RUNTIME_VERSION}/runtime-x86_64"
RUNTIME_SHA256="2fca8b443c92510f1483a883f60061ad09b46b978b2631c807cd873a47ec260d"

die() {
    echo "build-appimage: $*" >&2
    exit 1
}

[ "$#" -eq 4 ] || die "usage: $0 <release-binary> <sing-box-binary> <version> <out-dir>"

app_bin=$1
singbox_bin=$2
version=$3
out_dir=$4

[ -f "$app_bin" ] || die "release binary not found: $app_bin"
[ -f "$singbox_bin" ] || die "sing-box binary not found: $singbox_bin"
[[ "$version" =~ ^[0-9]+\.[0-9]+\.[0-9]+(-[0-9A-Za-z.]+)?$ ]] || die "not a valid version: '$version'"

here=$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)
repo_root=$(cd "$here/../.." && pwd)
icon="$repo_root/assets/icon.png"
[ -f "$icon" ] || die "icon not found: $icon"

work=$(mktemp -d)
trap 'rm -rf "$work"' EXIT

# Download $1 to $2 and check it against the SHA-256 in $3.
fetch() {
    local url=$1 dest=$2 sha=$3
    echo "Downloading $url"
    curl -fsSL --retry 3 -o "$dest" "$url"
    echo "$sha  $dest" | sha256sum -c --quiet - || die "SHA-256 mismatch for $url"
}

appimagetool=${APPIMAGETOOL:-}
if [ -z "$appimagetool" ]; then
    appimagetool="$work/appimagetool"
    fetch "$APPIMAGETOOL_URL" "$appimagetool" "$APPIMAGETOOL_SHA256"
    chmod +x "$appimagetool"
fi
[ -x "$appimagetool" ] || die "appimagetool is not executable: $appimagetool"

runtime=${APPIMAGE_RUNTIME:-}
if [ -z "$runtime" ]; then
    runtime="$work/runtime-x86_64"
    fetch "$RUNTIME_URL" "$runtime" "$RUNTIME_SHA256"
fi
[ -f "$runtime" ] || die "AppImage runtime not found: $runtime"

appdir="$work/BoxPilot.AppDir"
install -D -m 0755 "$app_bin" "$appdir/usr/bin/box-pilot"
install -D -m 0755 "$singbox_bin" "$appdir/usr/bin/sing-box"
install -m 0755 "$here/AppRun" "$appdir/AppRun"
install -m 0644 "$here/boxpilot.desktop" "$appdir/boxpilot.desktop"
install -m 0644 "$icon" "$appdir/boxpilot.png"
install -m 0644 "$icon" "$appdir/.DirIcon"

mkdir -p "$out_dir"
out="$out_dir/BoxPilot-$version-x86_64.AppImage"
rm -f "$out"

ARCH=x86_64 "$appimagetool" --appimage-extract-and-run \
    --no-appstream \
    --runtime-file "$runtime" \
    "$appdir" "$out"

echo "Built $out ($(du -h "$out" | cut -f1))"
