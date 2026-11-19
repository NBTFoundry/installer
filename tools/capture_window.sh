#!/usr/bin/env bash
# SPDX-License-Identifier: GPL-3.0-or-later
# Screenshot of the installer's window for the README: docs/assets/installer.png.
# Builds the installer with the test release fixture bundled (into
# target/capture), opens it under Xvfb and frames the top of the window.
#
#   tools/capture_window.sh
#
# Needs Xvfb, xwd and ImageMagick (`magick`); pngquant is used when present.
set -euo pipefail

root="$(cd "$(dirname "$0")/.." && pwd)"
fixtures="$root/core/tests/fixtures/release"
out="$root/docs/assets/installer.png"
for tool in Xvfb xwd magick; do
  command -v "$tool" >/dev/null || { echo "$tool is required" >&2; exit 1; }
done

NBT_MANAGER_RELEASE="$fixtures/release-v1.nbtr" NBT_MANAGER_PUBLIC_KEY="$fixtures/release.pub" \
  CARGO_TARGET_DIR="$root/target/capture" cargo build --locked --quiet --manifest-path "$root/Cargo.toml"
bin="$root/target/capture/debug/nbt-manager-installer"

work="$(mktemp -d)"
cleanup() {
  [[ -n "${app:-}" ]] && kill "$app" 2>/dev/null || true
  [[ -n "${xvfb:-}" ]] && kill "$xvfb" 2>/dev/null || true
  rm -rf "${work:?}"
}
trap cleanup EXIT

exec 3<> >(:)
Xvfb -displayfd 3 -screen 0 700x700x24 -nolisten tcp 2>/dev/null &
xvfb=$!
read -r -t 10 number <&3 || { echo "Xvfb did not start" >&2; exit 1; }
# X11 only: never open the window on the desktop's Wayland session.
env -u WAYLAND_DISPLAY DISPLAY=":$number" LIBGL_ALWAYS_SOFTWARE=1 "$bin" >"$work/app.log" 2>&1 &
app=$!
sleep 4
xwd -root -silent -display ":$number" >"$work/screen.xwd"

w=700 h=232 border=12
magick "xwd:$work/screen.xwd" -crop "${w}x${h}+0+0" +repage \
  -bordercolor "#171b22" -border "$border" \
  \( -size "$((w + 2 * border))x$((h + 2 * border))" xc:none -fill white \
     -draw "roundrectangle 0,0 $((w + 2 * border - 1)),$((h + 2 * border - 1)) 16,16" \) \
  -compose DstIn -composite -strip -define png:exclude-chunks=date,time "$out"
if command -v pngquant >/dev/null; then
  pngquant --force --skip-if-larger --output "$out" -- "$out" || true
fi
echo "wrote $out"
