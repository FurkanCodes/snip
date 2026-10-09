#!/bin/sh
# Build Snip.app (release) so macOS gives it its own Screen Recording permission.
#
#   ./bundle.sh              -> target/Snip.app
#   ./bundle.sh --install    -> also copies it to ~/Applications and launches it
#
# Signs ad-hoc with a designated requirement pinned to the bundle ID, so the
# Screen Recording permission survives rebuilds. Set SIGN_ID to a real
# code-signing identity (e.g. "Apple Development: you@x.com") to use that instead.
set -eu
cd "$(dirname "$0")"

cargo build --release

APP=target/Snip.app
VERSION=$(sed -n 's/^version = "\(.*\)"/\1/p' Cargo.toml | head -1)
rm -rf "$APP"
mkdir -p "$APP/Contents/MacOS"
cp target/release/snip "$APP/Contents/MacOS/snip"

cat > "$APP/Contents/Info.plist" <<EOF
<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
  <key>CFBundleName</key><string>Snip</string>
  <key>CFBundleDisplayName</key><string>Snip</string>
  <key>CFBundleIdentifier</key><string>dev.snip.app</string>
  <key>CFBundleExecutable</key><string>snip</string>
  <key>CFBundlePackageType</key><string>APPL</string>
  <key>CFBundleShortVersionString</key><string>${VERSION}</string>
  <key>CFBundleVersion</key><string>${VERSION}</string>
  <key>LSMinimumSystemVersion</key><string>12.0</string>
  <key>LSUIElement</key><true/>
  <key>NSHighResolutionCapable</key><true/>
  <key>NSMicrophoneUsageDescription</key><string>Snip records your microphone when "Record microphone" is turned on.</string>
</dict>
</plist>
EOF

if [ -n "${SIGN_ID:-}" ]; then
  codesign --force --sign "$SIGN_ID" "$APP" >/dev/null 2>&1
else
  # Ad-hoc signatures normally identify the app by a hash of this exact build,
  # so macOS forgets Screen Recording permission after every rebuild. Pin the
  # identity to the bundle ID instead so the permission sticks.
  codesign --force --sign - \
    --requirements '=designated => identifier "dev.snip.app"' \
    "$APP" >/dev/null 2>&1
fi
echo "Built $APP ($(du -sh "$APP" | cut -f1))"

if [ "${1:-}" = "--install" ]; then
  mkdir -p ~/Applications
  pkill -f "Snip.app/Contents/MacOS/snip" 2>/dev/null || true
  rm -rf ~/Applications/Snip.app
  cp -R "$APP" ~/Applications/
  open ~/Applications/Snip.app
  echo "Installed to ~/Applications/Snip.app"
fi
