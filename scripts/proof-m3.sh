#!/usr/bin/env bash
# Milestone 3 proof: a git dependency's build step runs once per commit and is cached,
# a build failure is diagnosable, no credential is written to disk, and solidify/eject
# handle git dependencies. Clone and build seconds are printed as values. The checks live
# in src/gitdep/proof_m3.rs (cargo test, no network).
# Usage (from the repository root): bash scripts/proof-m3.sh [--only P] [--quick|--full]
# --full also runs the node:: and gitdep:: unit-test modules.
set -euo pipefail

FULL=0
while [ $# -gt 0 ]; do
  case "$1" in
    --only)
      if [ $# -lt 2 ]; then echo "proof-m3: --only needs a pattern" >&2; exit 2; fi
      export KONG_PROOF_ONLY="$2"; shift 2 ;;
    --only=*) export KONG_PROOF_ONLY="${1#--only=}"; shift ;;
    --quick) shift ;;
    --full) FULL=1; shift ;;
    *) echo "proof-m3: unknown argument: $1" >&2; exit 2 ;;
  esac
done

THREADS="${NMC_CPUS:-1}"

# The cargo registry and build cache come from Kong (`kong rules && kong use`), never from cargo fetch.
if command -v kong >/dev/null 2>&1 && [ -f kong.rules ] && kong env >/dev/null 2>&1; then eval "$(kong env)"; fi

# --include-ignored is harmless now that the test's #[ignore] is removed.
OUT="$(mktemp)"
trap 'rm -f "$OUT"' EXIT
cargo test --offline --no-default-features --quiet proof_m3 -- \
  --include-ignored --nocapture --test-threads="$THREADS" 2>&1 | tee "$OUT"

# Timings are values, not thresholds: taken from the build_once check line.
if grep -q '^build_once: .*clone_seconds=' "$OUT"; then
  CLONE_S="$(sed -n 's/^build_once: .*clone_seconds=\([0-9.]*\).*/\1/p' "$OUT" | head -n1)"
  BUILD_S="$(sed -n 's/^build_once: .*build_seconds=\([0-9.]*\).*/\1/p' "$OUT" | head -n1)"
  echo "proof-m3: clone_seconds=${CLONE_S:-?} build_seconds=${BUILD_S:-?}"
fi

if [ "$FULL" -eq 1 ]; then
  cargo test --offline --no-default-features --quiet node:: -- --test-threads="$THREADS"
  cargo test --offline --no-default-features --quiet gitdep:: -- --test-threads="$THREADS"
fi
