#!/bin/sh
# Build Lighthouse_Management-x86_64.AppImage in the repo root. Needs appimagetool on PATH.
set -eu
cd "$(dirname "$0")/.."
cargo build --release
dir=target/AppDir
rm -rf "$dir"
install -Dm755 target/release/lighthouse "$dir/usr/bin/lighthouse"
install -Dm644 lighthouse.desktop "$dir/lighthouse.desktop"
install -Dm644 assets/icon.png "$dir/lighthouse.png"
ln -s usr/bin/lighthouse "$dir/AppRun"
ARCH=x86_64 appimagetool "$dir" Lighthouse_Management-x86_64.AppImage
