#!/usr/bin/env bash
# SPDX-License-Identifier: GPL-3.0-or-later
# Builds the downloadable installer for one platform, with the NBT Manager
# release pinned in release/manager.env: fetched from its GitHub release and
# checked against the SHA-256 there; core/build.rs then checks its signature
# against release/manager-public.pem. Used by .github/workflows/release.yml.
#
#   tools/release_build.sh linux-x64|windows-x64|macos OUT_DIR
#
# windows-x64 cross-builds on Linux with MinGW (x86_64-w64-mingw32-gcc);
# macos builds a universal application (arm64 and x86_64) on a Mac.
set -euo pipefail

platform=${1:?platform}
mkdir -p "${2:?output folder}"
out=$(cd "$2" && pwd)
root=$(cd "$(dirname "$0")/.." && pwd)
# shellcheck source=../release/manager.env
source "$root/release/manager.env"
version=$(sed -n 's/^version = "\(.*\)"$/\1/p' "$root/Cargo.toml" | head -n 1)
target_dir=${CARGO_TARGET_DIR:-$root/target}
work=$(mktemp -d)
trap 'rm -rf "$work"' EXIT

sha256() { if command -v sha256sum >/dev/null; then sha256sum "$1"; else shasum -a 256 "$1"; fi | cut -d' ' -f1; }

release="$work/nbt-manager-$MANAGER_VERSION.nbtr"
curl -fsSL --retry 3 -o "$release" \
  "https://github.com/NBTFoundry/manager/releases/download/v$MANAGER_VERSION/nbt-manager-$MANAGER_VERSION.nbtr"
[[ "$(sha256 "$release")" == "$MANAGER_SHA256" ]] \
  || { echo "release_build.sh: the manager release does not match release/manager.env" >&2; exit 1; }
export NBT_MANAGER_RELEASE="$release" NBT_MANAGER_PUBLIC_KEY="$root/release/manager-public.pem"

name="nbt-manager-installer-$version"
stage="$work/$name"
mkdir -p "$stage"
cp "$root/LICENSE" "$root/DISCLAIMER.md" "$root/legal/NOTICE.txt" "$stage/"

build() { cargo build --release --locked --manifest-path "$root/Cargo.toml" --target "$1"; }

case "$platform" in
  linux-x64)
    build x86_64-unknown-linux-gnu
    cp "$target_dir/x86_64-unknown-linux-gnu/release/nbt-manager-installer" "$stage/"
    tar -C "$work" -czf "$out/$name-linux-x64.tar.gz" "$name"
    ;;
  windows-x64)
    CARGO_TARGET_X86_64_PC_WINDOWS_GNU_LINKER=x86_64-w64-mingw32-gcc \
      CC_x86_64_pc_windows_gnu=x86_64-w64-mingw32-gcc build x86_64-pc-windows-gnu
    cp "$target_dir/x86_64-pc-windows-gnu/release/nbt-manager-installer.exe" "$stage/"
    rm -f "$out/$name-windows-x64.zip"
    (cd "$work" && zip -qr "$out/$name-windows-x64.zip" "$name")
    ;;
  macos)
    build aarch64-apple-darwin
    build x86_64-apple-darwin
    app="$stage/NBT Manager Installer.app"
    mkdir -p "$app/Contents/MacOS"
    lipo -create -output "$app/Contents/MacOS/nbt-manager-installer" \
      "$target_dir/aarch64-apple-darwin/release/nbt-manager-installer" \
      "$target_dir/x86_64-apple-darwin/release/nbt-manager-installer"
    cat >"$app/Contents/Info.plist" <<EOF
<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
  <key>CFBundleExecutable</key><string>nbt-manager-installer</string>
  <key>CFBundleIdentifier</key><string>org.nbtfoundry.installer</string>
  <key>CFBundleName</key><string>NBT Manager Installer</string>
  <key>CFBundlePackageType</key><string>APPL</string>
  <key>CFBundleShortVersionString</key><string>$version</string>
  <key>CFBundleVersion</key><string>$version</string>
  <key>LSMinimumSystemVersion</key><string>10.13</string>
  <key>NSHighResolutionCapable</key><true/>
  <key>NSLocalNetworkUsageDescription</key><string>Connects to the head unit and the vehicle gateway on the car's network.</string>
</dict>
</plist>
EOF
    codesign --force --sign - "$app"
    rm -f "$out/$name-macos.zip"
    ditto -c -k --keepParent "$stage" "$out/$name-macos.zip"
    ;;
  *)
    echo "release_build.sh: unknown platform $platform" >&2
    exit 2
    ;;
esac
