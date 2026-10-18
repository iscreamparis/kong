#!/usr/bin/env bash
# Before/after M5 timing: cold and warm seconds of
#   cargo test --offline --no-default-features --quiet node::
# on this repository, on one core (CARGO_BUILD_JOBS=1, --test-threads=1).
# Usage (from the repository root): bash scripts/measure/cargo-warm.sh [--out DIR]
#   default DIR: docs/measurements/cargo-warm
#
# Phases, appended to $OUT/timings.txt as `<phase> seconds=<s> exit=<rc>`:
#   before_cold             fresh clone c1, CARGO_HOME=$WORK/before-home, `cargo fetch --locked`
#                           (network, not timed), then the timed test with the in-clone target dir
#   before_second_worktree  `git worktree add` wt2 of c1, same CARGO_HOME, cold target again
#   after_cold              fresh clone c3, `kong rules && kong use`, `eval "$(kong env)"`, timed test
#   after_second_worktree   worktree wt4 of c3, same steps (shared target dir from kong)
# Values only: no thresholds. Full cargo output of each phase goes to $OUT/logs/<phase>.log.
#
# Environment: KONG_STORE defaults to the real store (~/.local/share/kong/store);
# XDG_DATA_HOME=$WORK/data and KONG_CACHE=$WORK/cache keep the RULEZ envs, the vendored
# registry and the shared target dir private to this run, so after_cold is genuinely cold.
# Set KONG_BIN to use a prebuilt kong instead of `cargo build --no-default-features`.
# Takes well over 5 minutes (four compiles): run it as a programmatic card.
set -euo pipefail

OUT=docs/measurements/cargo-warm
while [ $# -gt 0 ]; do
  case "$1" in
    --out)
      if [ $# -lt 2 ]; then echo "cargo-warm: --out needs a directory" >&2; exit 2; fi
      OUT="$2"; shift 2 ;;
    --out=*) OUT="${1#--out=}"; shift ;;
    *) echo "cargo-warm: unknown argument: $1" >&2; exit 2 ;;
  esac
done

note() { echo "cargo-warm: $*" >&2; }
die() { echo "cargo-warm: $*" >&2; exit 1; }

# Clean PATH (CLAUDE.md: no inherited entries with spaces or under /mnt/).
CLEAN_PATH="${CARGO_HOME:-$HOME/.cargo}/bin:/root/.cargo/bin:/usr/local/bin:/usr/bin:/bin:/usr/sbin:/sbin"
IFS=: read -r -a _inherited <<< "${PATH:-}"
for p in "${_inherited[@]}"; do
  case "$p" in ""|*" "*|/mnt/*) ;; *) CLEAN_PATH="$CLEAN_PATH:$p" ;; esac
done
export PATH="$CLEAN_PATH"
command -v cargo >/dev/null || die "cargo not found on PATH ($PATH)"

REPO=$(git rev-parse --show-toplevel) || die "not inside a git repository"
COMMIT=$(git -C "$REPO" rev-parse HEAD)
case "$OUT" in /*) ;; *) OUT="$REPO/$OUT" ;; esac
mkdir -p "$OUT/logs"
TIMINGS="$OUT/timings.txt"

export CARGO_BUILD_JOBS=1
export RUST_TEST_THREADS=1
TEST_ARGS=(test --offline --no-default-features --quiet node:: -- --test-threads=1)

# The real store, resolved before XDG_DATA_HOME is redirected.
REAL_STORE="${XDG_DATA_HOME:-$HOME/.local/share}/kong/store"
export KONG_STORE="${KONG_STORE:-$REAL_STORE}"

WORK=$(mktemp -d "${TMPDIR:-/tmp}/kong-cargo-warm.XXXXXX")
cleanup() { chmod -R u+w "$WORK" 2>/dev/null || true; rm -rf "$WORK"; }
trap cleanup EXIT
export XDG_DATA_HOME="$WORK/data"
export KONG_CACHE="$WORK/cache"
mkdir -p "$XDG_DATA_HOME" "$KONG_CACHE"

# ── bootstrap kong (as in proof-m5.sh) ──────────────────────────────────────
if [ -n "${KONG_BIN:-}" ]; then
  KONG="$KONG_BIN"
else
  note "building kong (cargo build --no-default-features)"
  if ! (cd "$REPO" && { cargo build --offline --no-default-features || cargo build --no-default-features; }) \
      > "$OUT/logs/build-kong.log" 2>&1; then
    tail -20 "$OUT/logs/build-kong.log" >&2
    die "cargo build of kong failed (set KONG_BIN to a prebuilt kong)"
  fi
  KONG="$REPO/target/debug/kong"
fi
[ -x "$KONG" ] || die "kong binary not found: $KONG"
KONG_DIR=$(cd "$(dirname "$KONG")" && pwd)
export PATH="$KONG_DIR:$PATH"

now() { date +%s.%N; }
secs() { awk -v a="$1" -v b="$2" 'BEGIN { printf "%.1f", b - a }'; }

# record <phase> <t0> <t1> <rc>
record() {
  echo "$1 seconds=$(secs "$2" "$3") exit=$4" >> "$TIMINGS"
  note "$1 seconds=$(secs "$2" "$3") exit=$4"
}

# timed_test <phase> <dir> [env-script]: runs the cargo test in <dir> (subshell), timed.
timed_test() {
  local phase=$1 dir=$2 setup=${3:-true} t0 t1 rc=0
  t0=$(now)
  (cd "$dir" && eval "$setup" && cargo "${TEST_ARGS[@]}") > "$OUT/logs/$phase.log" 2>&1 || rc=$?
  t1=$(now)
  record "$phase" "$t0" "$t1" "$rc"
}

# kong_use <dir>: `kong rules && kong use` (not timed).
kong_use() {
  local dir=$1 log=$2
  if ! (cd "$dir" && kong rules && kong use) > "$log" 2>&1; then
    tail -20 "$log" >&2
    die "kong rules && kong use failed in $dir (see $log)"
  fi
}

clone_at_head() {
  git clone -q --no-hardlinks "$REPO" "$1"
  git -C "$1" -c advice.detachedHead=false checkout -q --detach "$COMMIT"
}

# ── preparation (not timed) ─────────────────────────────────────────────────
C1="$WORK/c1"; WT2="$WORK/wt2"; C3="$WORK/c3"; WT4="$WORK/wt4"
BEFORE_HOME="$WORK/before-home"
clone_at_head "$C1"
note "cargo fetch --locked into $BEFORE_HOME (network, not timed)"
if ! (cd "$C1" && CARGO_HOME="$BEFORE_HOME" cargo fetch --locked) > "$OUT/logs/before-fetch.log" 2>&1; then
  tail -20 "$OUT/logs/before-fetch.log" >&2
  die "cargo fetch --locked failed (see $OUT/logs/before-fetch.log)"
fi
clone_at_head "$C3"
kong_use "$C3" "$OUT/logs/after-kong-use-c3.log"
KONG_RUST=$("$C3/.rust-toolchain/bin/rustc" -V 2>/dev/null || echo unknown)

{
  echo "# cargo-warm: cargo ${TEST_ARGS[*]}"
  echo "date_utc=$(date -u +%Y-%m-%dT%H:%M:%SZ)"
  echo "commit=$COMMIT"
  echo "nproc=$(nproc)"
  echo "rustc=$(rustc -V 2>/dev/null || echo unknown)"
  echo "kong_toolchain=$KONG_RUST"
  echo "CARGO_BUILD_JOBS=$CARGO_BUILD_JOBS test_threads=1"
} > "$TIMINGS"

# ── before M5 ───────────────────────────────────────────────────────────────
timed_test before_cold "$C1" "export CARGO_HOME='$BEFORE_HOME'"
git -C "$C1" worktree add -q --detach "$WT2" "$COMMIT"
timed_test before_second_worktree "$WT2" "export CARGO_HOME='$BEFORE_HOME'"

# ── after M5 ────────────────────────────────────────────────────────────────
timed_test after_cold "$C3" 'eval "$(kong env)"'
git -C "$C3" worktree add -q --detach "$WT4" "$COMMIT"
kong_use "$WT4" "$OUT/logs/after-kong-use-wt4.log"
timed_test after_second_worktree "$WT4" 'eval "$(kong env)"'

note "timings written to $TIMINGS"
cat "$TIMINGS"
