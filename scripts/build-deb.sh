#!/usr/bin/env bash
# Build a .deb for Debian/Ubuntu/Linux Mint (amd64) with plain dpkg-deb.
# Output: target/flurry_<version>_<arch>.deb
set -euo pipefail
cd "$(dirname "$0")/.."
version=$(sed -n 's/^version = "\(.*\)"/\1/p' Cargo.toml | head -1)
arch=$(dpkg --print-architecture)
cargo build --release --locked
stage=$(mktemp -d)
chmod 755 "$stage"
trap 'rm -rf "$stage"' EXIT
install -Dm755 target/release/flurry "$stage/usr/bin/flurry"
install -Dm644 packaging/flurry.desktop "$stage/usr/share/applications/flurry.desktop"
install -Dm644 packaging/flurry.svg "$stage/usr/share/icons/hicolor/scalable/apps/flurry.svg"
install -Dm644 LICENSE "$stage/usr/share/doc/flurry/copyright"
install -Dm644 THIRD_PARTY.md "$stage/usr/share/doc/flurry/THIRD_PARTY.md"
install -Dm644 README.md "$stage/usr/share/doc/flurry/README.md"
mkdir -p "$stage/DEBIAN"
size=$(du -sk "$stage/usr" | cut -f1)
cat > "$stage/DEBIAN/control" <<CONTROL
Package: flurry
Version: $version
Architecture: $arch
Maintainer: flurry authors <flurry@users.noreply.invalid>
Installed-Size: $size
Depends: libc6 (>= 2.34), libgcc-s1
Recommends: libglib2.0-bin
Suggests: curl, pulseaudio-utils, pipewire-bin
Section: x11
Priority: optional
Homepage: https://github.com/flurry-snow/flurry
Description: snow falling over your Linux desktop
 Falling snow behind your windows that piles up on the panel and on top of
 windows, melts, drips and grows icicles. Roll snowballs with the mouse,
 carry them and stack three into a snowman. Low CPU, tray icon and CLI.
 Works on X11 desktops with a compositor (Cinnamon, GNOME on Xorg, XFCE,
 MATE, KDE X11).
CONTROL
out="target/flurry_${version}_${arch}.deb"
dpkg-deb --root-owner-group --build "$stage" "$out" >/dev/null
echo "$out"
