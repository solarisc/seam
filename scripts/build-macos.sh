#!/bin/bash
# Build Seam.app (and optionally a disk image) from source.
#
#   scripts/build-macos.sh              build target/macos/Seam.app
#   scripts/build-macos.sh --universal  include both Apple silicon and Intel
#   scripts/build-macos.sh --dmg        also create target/macos/Seam-<version>.dmg
#   scripts/build-macos.sh --install    copy the app to /Applications and open it
#
# Set SEAM_CODESIGN_IDENTITY to sign with a Developer ID; otherwise the app
# is signed ad hoc. Ad hoc builds must be allowed again under Accessibility
# after every rebuild because macOS ties the permission to the signature.
set -euo pipefail

cd "$(dirname "$0")/.."
universal=false dmg=false install=false
for arg in "$@"; do
    case "$arg" in
        --universal) universal=true ;;
        --dmg) dmg=true ;;
        --install) install=true ;;
        *) echo "Unknown option: $arg" >&2; exit 2 ;;
    esac
done

version=$(sed -n 's/^version = "\(.*\)"/\1/p' Cargo.toml | head -1)
out=target/macos
app="$out/Seam.app"
identity="${SEAM_CODESIGN_IDENTITY:--}"

if $universal; then
    targets=(aarch64-apple-darwin x86_64-apple-darwin)
    for target in "${targets[@]}"; do
        rustup target add "$target" >/dev/null
        cargo build --release --features gui --bins --target "$target"
    done
else
    cargo build --release --features gui --bins
fi

rm -rf "$app"
mkdir -p "$app/Contents/MacOS" "$app/Contents/Resources"
for binary in seam seam-agent; do
    if $universal; then
        lipo -create -output "$app/Contents/MacOS/$binary" \
            "target/aarch64-apple-darwin/release/$binary" \
            "target/x86_64-apple-darwin/release/$binary"
    else
        cp "target/release/$binary" "$app/Contents/MacOS/$binary"
    fi
done
cp assets/seam.icns "$app/Contents/Resources/seam.icns"
cat > "$app/Contents/Info.plist" <<PLIST
<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
	<key>CFBundleName</key><string>Seam</string>
	<key>CFBundleDisplayName</key><string>Seam</string>
	<key>CFBundleIdentifier</key><string>dev.seam.app</string>
	<key>CFBundleVersion</key><string>$version</string>
	<key>CFBundleShortVersionString</key><string>$version</string>
	<key>CFBundlePackageType</key><string>APPL</string>
	<key>CFBundleExecutable</key><string>seam</string>
	<key>CFBundleIconFile</key><string>seam.icns</string>
	<key>LSMinimumSystemVersion</key><string>11.0</string>
	<key>NSHighResolutionCapable</key><true/>
	<key>NSLocalNetworkUsageDescription</key><string>Seam finds and connects to your other computers on this network.</string>
</dict>
</plist>
PLIST

# Sign the helper first, then the bundle that seals it.
codesign --force --options runtime --sign "$identity" --identifier dev.seam.agent "$app/Contents/MacOS/seam-agent"
codesign --force --options runtime --sign "$identity" "$app"
codesign --verify --deep --strict "$app"
echo "Built $app"

if $dmg; then
    staging="$out/dmg"
    rm -rf "$staging" "$out/Seam-$version.dmg"
    mkdir -p "$staging"
    cp -R "$app" "$staging/"
    ln -s /Applications "$staging/Applications"
    hdiutil create -quiet -volname Seam -srcfolder "$staging" -ov -format UDZO "$out/Seam-$version.dmg"
    rm -rf "$staging"
    echo "Built $out/Seam-$version.dmg"
fi

if $install; then
    # The running agent belongs to the copy being replaced.
    launchctl bootout "gui/$(id -u)/dev.seam.agent" 2>/dev/null || true
    rm -rf /Applications/Seam.app
    cp -R "$app" /Applications/
    open /Applications/Seam.app
    echo "Installed /Applications/Seam.app"
fi
