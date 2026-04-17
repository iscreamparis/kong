#!/usr/bin/env bash
# Build Kong.app + DMG for macOS
set -euo pipefail
cd "$(dirname "$0")"
exec bash scripts/macos/build-macos.sh "$@"
