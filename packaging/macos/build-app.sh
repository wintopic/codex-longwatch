#!/bin/sh
set -eu

target="${1:-$(rustc -vV | sed -n 's/^host: //p')}"
version="${LONGWATCH_VERSION:-0.1.0}"
app="dist/Longwatch.app"
cargo build --locked --release --all-features --target "$target"
rm -rf "$app"
mkdir -p "$app/Contents/MacOS" "$app/Contents/Resources"
cp "target/$target/release/codex-longwatch" "$app/Contents/MacOS/codex-longwatch"
cp packaging/macos/Info.plist "$app/Contents/Info.plist"
cp packaging/macos/Longwatch-Light.icns "$app/Contents/Resources/Longwatch-Light.icns"
cp packaging/macos/Longwatch-Dark.icns "$app/Contents/Resources/Longwatch-Dark.icns"
sed -i.bak "s/<key>CFBundleShortVersionString<\/key><string>0.1.0/<key>CFBundleShortVersionString<\/key><string>$version/; s/<key>CFBundleVersion<\/key><string>0.1.0/<key>CFBundleVersion<\/key><string>$version/" "$app/Contents/Info.plist"
rm -f "$app/Contents/Info.plist.bak"
codesign --deep --force --sign "${CODE_SIGN_IDENTITY:--}" "$app" || true
