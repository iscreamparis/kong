#!/usr/bin/env bash
# build/build-macos.sh — Build Kong.app + Kong-<version>-macos-<arch>.dmg
set -euo pipefail

cd "$(dirname "$0")/../.."
ROOT="$(pwd)"
SCRIPTS_DIR="$ROOT/scripts/macos"
OUT_DIR="$ROOT/dist"
mkdir -p "$OUT_DIR"

# ── Version from Cargo.toml ───────────────────────────────────────────────────
VERSION=$(grep '^version' Cargo.toml | head -1 | sed 's/version = "\(.*\)"/\1/')
echo "Kong $VERSION — macOS release build"

# ── Detect architecture ───────────────────────────────────────────────────────
ARCH="$(uname -m)"
case "$ARCH" in
    arm64)  CARGO_TARGET="aarch64-apple-darwin" ;;
    x86_64) CARGO_TARGET="x86_64-apple-darwin" ;;
    *)      echo "Unsupported architecture: $ARCH"; exit 1 ;;
esac
echo "Target: $CARGO_TARGET"

# ── Ensure the Rust target is installed ──────────────────────────────────────
rustup target add "$CARGO_TARGET" 2>/dev/null || true

# ── Cargo build ──────────────────────────────────────────────────────────────
echo ""
echo "Building release binary..."
cargo build --release --target "$CARGO_TARGET"

BINARY="target/$CARGO_TARGET/release/kong"
strip "$BINARY"
echo "Binary: $BINARY ($(du -sh "$BINARY" | cut -f1))"

# ── Assemble Kong.app bundle ──────────────────────────────────────────────────
STAGING="$OUT_DIR/staging"
APP_STAGING="$STAGING/Kong.app"
MACOS_DIR="$APP_STAGING/Contents/MacOS"
RES_DIR="$APP_STAGING/Contents/Resources"

echo ""
echo "Assembling Kong.app..."
rm -rf "$STAGING"
mkdir -p "$MACOS_DIR" "$RES_DIR"

# The compiled binary is the sole CFBundleExecutable
cp "$BINARY" "$MACOS_DIR/kong"
chmod +x "$MACOS_DIR/kong"

cp "$SCRIPTS_DIR/Info.plist" "$APP_STAGING/Contents/Info.plist"

# ── DMG content staging ───────────────────────────────────────────────────────
DMG_STAGING="$STAGING/dmg"
mkdir -p "$DMG_STAGING"
cp -R "$APP_STAGING" "$DMG_STAGING/Kong.app"

# /Applications shortcut for Finder drag-install
ln -s /Applications "$DMG_STAGING/Applications"

# ── Create DMG ────────────────────────────────────────────────────────────────
DMG_NAME="Kong-${VERSION}-macos-${ARCH}"
DMG_TMP="$OUT_DIR/${DMG_NAME}-tmp.dmg"
DMG_FINAL="$OUT_DIR/${DMG_NAME}.dmg"

echo ""
echo "Creating DMG..."
rm -f "$DMG_TMP" "$DMG_FINAL"

# Create a writable sparse image
hdiutil create \
    -srcfolder "$DMG_STAGING" \
    -volname "Kong $VERSION" \
    -fs HFS+ \
    -format UDRW \
    -o "$DMG_TMP"

# Convert to compressed read-only DMG
hdiutil convert "$DMG_TMP" -format UDZO -imagekey zlib-level=9 -o "$DMG_FINAL"
rm -f "$DMG_TMP"

echo ""
# Clean up staging
rm -rf "$STAGING"

echo "Done: $DMG_FINAL ($(du -sh "$DMG_FINAL" | cut -f1))"
echo ""
echo "To test: open '$DMG_FINAL'"
echo "Gatekeeper warning: right-click Kong.app → Open to bypass, or:"
echo "  xattr -cr '$DMG_FINAL'"
