#!/usr/bin/env bash
# Milestone 3 proof: a git dependency's build step runs once per commit and is cached,
# a build failure is diagnosable, and no credential is written to disk. The checks live
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

echo "proof-m3: warming the cargo registry (cargo fetch; no-op when already warm)"
if [ -f Cargo.lock ]; then cargo fetch --locked; else cargo fetch; fi

# --include-ignored keeps this valid once the test's #[ignore] is removed.
cargo test --offline --no-default-features --quiet proof_m3 -- \
  --include-ignored --nocapture --test-threads="$THREADS"

if [ "$FULL" -eq 1 ]; then
  cargo test --offline --no-default-features --quiet node:: -- --test-threads="$THREADS"
  cargo test --offline --no-default-features --quiet gitdep:: -- --test-threads="$THREADS"
fi
