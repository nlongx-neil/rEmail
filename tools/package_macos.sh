#!/usr/bin/env bash
# 在 macOS 上构建并组装 MailSender.app
# 用法：./tools/package_macos.sh [输出目录，默认 dist]
set -euo pipefail

ROOT="$(cd "$(dirname "$0")/.." && pwd)"
OUT="${1:-$ROOT/dist}"
APP="$OUT/MailSender.app"
BIN="$ROOT/target/release/mail_sender"
[ -f "$BIN" ] || BIN="${CARGO_TARGET_DIR:-$HOME/.cargo/shared-target}/release/mail_sender"

cargo build --release

rm -rf "$APP"
mkdir -p "$APP/Contents/MacOS" "$APP/Contents/Resources"
cp "$BIN" "$APP/Contents/MacOS/mail_sender"
cp "$ROOT/assets/icon.icns" "$APP/Contents/Resources/icon.icns"

cat > "$APP/Contents/Info.plist" <<'PLIST'
<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
    <key>CFBundleName</key><string>MailSender</string>
    <key>CFBundleDisplayName</key><string>MailSender</string>
    <key>CFBundleIdentifier</key><string>com.example.mailsender</string>
    <key>CFBundleVersion</key><string>0.1.0</string>
    <key>CFBundleShortVersionString</key><string>0.1.0</string>
    <key>CFBundlePackageType</key><string>APPL</string>
    <key>CFBundleExecutable</key><string>mail_sender</string>
    <key>CFBundleIconFile</key><string>icon.icns</string>
    <key>LSMinimumSystemVersion</key><string>10.15</string>
    <key>NSHighResolutionCapable</key><true/>
</dict>
</plist>
PLIST

plutil -lint "$APP/Contents/Info.plist"
echo "OK: $APP"
