#!/usr/bin/env bash
# Build an AppImage. Needs `appimagetool` on PATH (from
# https://github.com/AppImage/appimagetool/releases); this script does not
# download anything itself.
# Output: target/Flurry-<version>-<arch>.AppImage
set -euo pipefail
cd "$(dirname "$0")/.."
command -v appimagetool >/dev/null || {
  echo "appimagetool not found: download it from https://github.com/AppImage/appimagetool/releases" >&2
  exit 1
}
version=$(sed -n 's/^version = "\(.*\)"/\1/p' Cargo.toml | head -1)
arch=$(uname -m)
cargo build --release --locked
app=$(mktemp -d)/Flurry.AppDir
trap 'rm -rf "$(dirname "$app")"' EXIT
install -Dm755 target/release/flurry "$app/usr/bin/flurry"
install -Dm644 packaging/flurry.desktop "$app/flurry.desktop"
install -Dm644 packaging/flurry.svg "$app/flurry.svg"
install -Dm644 packaging/flurry.svg "$app/usr/share/icons/hicolor/scalable/apps/flurry.svg"
cat > "$app/AppRun" <<'RUN'
#!/bin/sh
exec "$(dirname "$(readlink -f "$0")")/usr/bin/flurry" "$@"
RUN
chmod +x "$app/AppRun"
out="target/Flurry-${version}-${arch}.AppImage"
ARCH=$arch appimagetool "$app" "$out"
echo "$out"
