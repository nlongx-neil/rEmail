#!/usr/bin/env bash
# 在 macOS 上构建通用二进制（arm64 + x86_64）的 MailSender.app 与 .dmg
# 用法：./tools/package_macos.sh [输出目录，默认 dist]
#
# Windows 交叉编译（需 mingw-w64，链接时用 Rust 自带 CRT 规避 mingw crt2.o 的兼容问题）：
#   RUSTFLAGS="-C link-self-contained=yes" cargo build --release --target x86_64-pc-windows-gnu
# 或直接推送到 GitHub，由 .github/workflows/build.yml 在 windows-latest 上产出 exe artifact。
set -euo pipefail

ROOT="$(cd "$(dirname "$0")/.." && pwd)"
OUT="${1:-$ROOT/dist}"
VERSION="$(sed -n 's/^version = "\(.*\)"/\1/p' "$ROOT/Cargo.toml" | head -1)"
APP="$OUT/MailSender.app"
BIN_ARM="${CARGO_TARGET_DIR:-$ROOT/target}/release/mail_sender"
BIN_X86="${CARGO_TARGET_DIR:-$ROOT/target}/x86_64-apple-darwin/release/mail_sender"
DMG="$OUT/MailSender-$VERSION-universal.dmg"

cargo build --release --manifest-path "$ROOT/Cargo.toml"
cargo build --release --target x86_64-apple-darwin --manifest-path "$ROOT/Cargo.toml"

mkdir -p "$OUT"
rm -rf "$APP"
mkdir -p "$APP/Contents/MacOS" "$APP/Contents/Resources"
lipo -create -output "$APP/Contents/MacOS/mail_sender" "$BIN_ARM" "$BIN_X86"
chmod +x "$APP/Contents/MacOS/mail_sender"
cp "$ROOT/assets/icon.icns" "$APP/Contents/Resources/icon.icns"

cat > "$APP/Contents/Info.plist" <<PLIST
<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
    <key>CFBundleInfoDictionaryVersion</key><string>6.0</string>
    <key>CFBundleName</key><string>MailSender</string>
    <key>CFBundleDisplayName</key><string>MailSender</string>
    <key>CFBundleIdentifier</key><string>com.rEmail.mailSender</string>
    <key>CFBundleVersion</key><string>$VERSION</string>
    <key>CFBundleShortVersionString</key><string>$VERSION</string>
    <key>CFBundlePackageType</key><string>APPL</string>
    <key>CFBundleExecutable</key><string>mail_sender</string>
    <key>CFBundleIconFile</key><string>icon.icns</string>
    <key>LSMinimumSystemVersion</key><string>11.0</string>
    <key>NSHighResolutionCapable</key><true/>
</dict>
</plist>
PLIST

plutil -lint "$APP/Contents/Info.plist"
# Ad-hoc 签名：Apple Silicon 上未签名二进制会被系统直接终止
codesign --force --deep --sign - "$APP"

# 制作拖拽安装用 dmg（内含 Applications 软链接）
STAGE="$(mktemp -d /tmp/mailsender_dmg.XXXXXX)"
cp -R "$APP" "$STAGE/MailSender.app"
ln -s /Applications "$STAGE/Applications"
rm -f "$DMG"
hdiutil create -volname "MailSender" -srcfolder "$STAGE" -ov -format UDZO "$DMG" >/dev/null
rm -rf "$STAGE"
hdiutil verify "$DMG" >/dev/null

echo "OK: $APP"
echo "OK: $DMG"
