#!/usr/bin/env bash
# Package BoxPilot for macOS as a DMG holding BoxPilot.app.
#
#   packaging/macos/build-dmg.sh <release-binary> <sing-box-binary> <version> <arch> <out-dir> \
#       [<sing-box-version>]
#
# e.g. packaging/macos/build-dmg.sh \
#        target/aarch64-apple-darwin/release/box_pilot_gui \
#        release/sing-box 1.13.5 arm64 release 1.14.2
#
# <arch> is arm64 or x86_64 and only names the file: the binaries must
# already be built for it. The privileged helper, boxpilot-helper, is taken
# from beside the release binary (the workspace's release build builds it
# too). <sing-box-version> goes into the helper's manifest; without it, the
# script asks sing-box itself, which works only when it runs on this Mac.
# Writes <out-dir>/BoxPilot-<version>-macos-<arch>.dmg. The bundle is
# assembled in a temporary directory and removed afterwards:
#
#   BoxPilot.app/Contents/Info.plist           packaging/macos/Info.plist,
#                                              versions filled in
#   BoxPilot.app/Contents/MacOS/BoxPilot       the BoxPilot release binary
#   BoxPilot.app/Contents/MacOS/sing-box       the bundled sing-box; BoxPilot
#                                              looks for it next to its own
#                                              executable, and the helper
#                                              installs this same file
#   BoxPilot.app/Contents/MacOS/boxpilot-helper    the privileged helper
#   BoxPilot.app/Contents/Resources/Helper/    what helper-install.sh needs
#                                              beside them (ADR 0006 rule 7):
#       manifest.json                          sing-box's version and the
#                                              SHA-256 of the signed file
#       io.github.glide01.boxpilot.helper.plist    the launchd plist
#       helper-install.sh, helper-uninstall.sh
#   BoxPilot.app/Contents/Resources/BoxPilot.icns   from assets/icon.png
#
# The app is signed ad hoc (no Developer ID, so not notarized either): the
# inner sing-box and helper first, then the bundle, which seals them and
# the helper's payload. The manifest hashes sing-box as signed, which is
# what helper-install.sh copies. Gatekeeper still blocks the first open of
# a downloaded copy; the release notes say how to allow it (ADR 0005). The
# DMG holds the app and an /Applications link to drag it onto.
#
# macOS only: needs sips, iconutil, codesign, plutil and hdiutil (all part
# of the base system / Xcode command line tools).

set -euo pipefail

die() {
    echo "build-dmg: $*" >&2
    exit 1
}

[ "$#" -eq 5 ] || [ "$#" -eq 6 ] ||
    die "usage: $0 <release-binary> <sing-box-binary> <version> <arch> <out-dir> [<sing-box-version>]"

app_bin=$1
singbox_bin=$2
version=$3
arch=$4
out_dir=$5
helper_bin=$(dirname "$app_bin")/boxpilot-helper

[ -f "$app_bin" ] || die "release binary not found: $app_bin"
[ -f "$singbox_bin" ] || die "sing-box binary not found: $singbox_bin"
[ -f "$helper_bin" ] || die "the privileged helper not found beside the release binary: $helper_bin"
[[ "$version" =~ ^[0-9]+\.[0-9]+\.[0-9]+(-[0-9A-Za-z.]+)?$ ]] || die "not a valid version: '$version'"
if [ "$#" -eq 6 ]; then
    singbox_version=$6
else
    singbox_version=$("$singbox_bin" version 2>/dev/null | sed -n 's/^sing-box version //p' | head -n 1) ||
        singbox_version=
fi
# The manifest's version rule (crates/boxpilot-helper/src/manifest.rs) and
# the release workflow's: so it needs no escaping in the JSON below.
[[ "${#singbox_version}" -le 64 && "$singbox_version" =~ ^[0-9]+\.[0-9]+\.[0-9]+(-[0-9A-Za-z.]+)?$ ]] ||
    die "not a valid sing-box version: '$singbox_version' (pass it as the sixth argument)"
case "$arch" in
    arm64 | x86_64) ;;
    *) die "arch must be arm64 or x86_64, not '$arch'" ;;
esac
for tool in sips iconutil codesign plutil hdiutil; do
    command -v "$tool" >/dev/null || die "$tool not found (this script runs on macOS)"
done

here=$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)
repo_root=$(cd "$here/../.." && pwd)
icon="$repo_root/assets/icon.png"
[ -f "$icon" ] || die "icon not found: $icon"

work=$(mktemp -d)
trap 'rm -rf "$work"' EXIT

staging="$work/dmg"
app="$staging/BoxPilot.app"
contents="$app/Contents"
mkdir -p "$contents/MacOS" "$contents/Resources"
install -m 0755 "$app_bin" "$contents/MacOS/BoxPilot"
install -m 0755 "$singbox_bin" "$contents/MacOS/sing-box"
install -m 0755 "$helper_bin" "$contents/MacOS/boxpilot-helper"

# CFBundleVersion takes numbers and dots only: drop a pre-release suffix.
sed -e "s/@VERSION@/$version/g" -e "s/@BUNDLE_VERSION@/${version%%-*}/g" \
    "$here/Info.plist" >"$contents/Info.plist"
plutil -lint "$contents/Info.plist" >/dev/null || die "Info.plist is not valid"
printf 'APPL????' >"$contents/PkgInfo"

# Every size an .icns holds. The source is 256 px, so 512 and up are
# upscaled; Finder only shows those in its largest views.
iconset="$work/BoxPilot.iconset"
mkdir -p "$iconset"
for size in 16 32 128 256 512; do
    sips -z "$size" "$size" "$icon" --out "$iconset/icon_${size}x${size}.png" >/dev/null
    double=$((size * 2))
    sips -z "$double" "$double" "$icon" --out "$iconset/icon_${size}x${size}@2x.png" >/dev/null
done
iconutil -c icns "$iconset" -o "$contents/Resources/BoxPilot.icns"

# Inside out: a bundle's signature seals the code it holds, so sing-box and
# the helper are signed before the bundle (which signs BoxPilot itself as
# its main executable).
codesign --force --sign - "$contents/MacOS/sing-box"
codesign --force --sign - "$contents/MacOS/boxpilot-helper"

# The helper's payload (ADR 0006 rule 7). The manifest hashes sing-box as
# signed: the file helper-install.sh copies, byte for byte. Written by hand,
# as packaging/windows/stage-helper.ps1 writes Windows' (no extra files: the
# macOS sing-box loads nothing from beside itself); crates/boxpilot-helper's
# install_manifest test checks it with the helper's own parser.
payload="$contents/Resources/Helper"
mkdir -p "$payload"
singbox_sha256=$(shasum -a 256 "$contents/MacOS/sing-box" | cut -d ' ' -f 1)
[[ "$singbox_sha256" =~ ^[0-9a-f]{64}$ ]] || die "unexpected SHA-256 '$singbox_sha256' for sing-box"
cat >"$payload/manifest.json" <<EOF
{
  "manifest_version": 1,
  "sing_box": {
    "file": "sing-box",
    "version": "$singbox_version",
    "sha256": "$singbox_sha256"
  },
  "extra_files": []
}
EOF
install -m 0644 "$here/io.github.glide01.boxpilot.helper.plist" "$payload/"
plutil -lint "$payload/io.github.glide01.boxpilot.helper.plist" >/dev/null || die "the helper's plist is not valid"
install -m 0755 "$here/helper-install.sh" "$here/helper-uninstall.sh" "$payload/"
echo "The helper's manifest:"
cat "$payload/manifest.json"

codesign --force --sign - "$app"
codesign --verify --strict --verbose=2 "$app"

ln -s /Applications "$staging/Applications"

mkdir -p "$out_dir"
out="$out_dir/BoxPilot-$version-macos-$arch.dmg"
rm -f "$out"

# hdiutil now and then fails with "Resource busy" on CI runners; a retry
# gets past it.
for attempt in 1 2 3; do
    if hdiutil create -volname "BoxPilot" -srcfolder "$staging" \
        -fs HFS+ -format UDZO -ov "$out"; then
        break
    fi
    [ "$attempt" -lt 3 ] || die "hdiutil create failed"
    sleep 5
done

echo "Built $out ($(du -h "$out" | cut -f1))"
