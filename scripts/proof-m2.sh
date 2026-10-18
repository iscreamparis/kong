#!/usr/bin/env bash
# Milestone 2 proof: a kong.git dependency is pinned, cloned once into the store,
# linked in place and finds its own dependencies; projects without kong.git are
# unchanged. The checks live in src/node/git/proof_m2.rs (cargo test, no network).
# Usage (from the repository root): bash scripts/proof-m2.sh [--only P] [--quick|--full]
# --full also runs the node:: unit-test modules.
set -euo pipefail

FULL=0
while [ $# -gt 0 ]; do
  case "$1" in
    --only)
      if [ $# -lt 2 ]; then echo "proof-m2: --only needs a pattern" >&2; exit 2; fi
      export KONG_PROOF_ONLY="$2"; shift 2 ;;
    --only=*) export KONG_PROOF_ONLY="${1#--only=}"; shift ;;
    --quick) shift ;;
    --full) FULL=1; shift ;;
    *) echo "proof-m2: unknown argument: $1" >&2; exit 2 ;;
  esac
done

THREADS="${NMC_CPUS:-1}"

# The cargo registry and build cache come from Kong (`kong rules && kong use`), never from cargo fetch.
if command -v kong >/dev/null 2>&1 && [ -f kong.rules ] && kong env >/dev/null 2>&1; then eval "$(kong env)"; fi

# --include-ignored keeps this valid once the test's #[ignore] is removed.
cargo test --offline --no-default-features --quiet proof_m2 -- \
  --include-ignored --nocapture --test-threads="$THREADS"

if [ "$FULL" -eq 1 ]; then
  cargo test --offline --no-default-features --quiet node:: -- --test-threads="$THREADS"
fi
