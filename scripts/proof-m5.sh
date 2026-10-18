#!/usr/bin/env bash
# M5 proof: Kong keeps the Cargo registry and the build cache warm.
# Usage (from the repository root): bash scripts/proof-m5.sh [--only P] [--full] [--keep]
#
# A fresh clone of this repository at HEAD runs `kong rules && kong use`, sources
# `.rust-toolchain/activate.sh`, then plain `cargo check --offline --no-default-features`
# and `cargo test --offline --no-default-features node::` (no extra cargo flags). A second
# worktree of the same commit loads `eval "$(kong env)"` and runs the same test without
# recompiling the dependency crates.
#
# Hostile sandbox: the nomorechat sandbox ships a read-only ~/.cargo/config.toml that
# replaces crates-io with a directory registry whose checksums do not match Cargo.lock.
# Cargo reads it as CARGO_HOME config and, for checkouts under $HOME, as a parent-directory
# config. The proof reproduces it: every kong/cargo child runs with a cleared environment
# and HOME=<work>/home, whose .cargo/config.toml points crates-io at a bogus registry, and
# the clone lives under that HOME. Kong's environment overrides it twice: CARGO_HOME is the
# gitignored .rust-toolchain/cargo-home, and the .rust-toolchain/bin/cargo wrapper passes
# `--config .rust-toolchain/cargo-home/config.toml`, which outranks every config file.
#
# Writable vs read-only in the sandbox:
#   - writable: the proof work dir (mktemp under $TMPDIR), which holds the shared target
#     dir and the vendored registry (KONG_CACHE=<work>/cache), the clones and the RULEZ envs;
#   - read-only: the store (KONG_STORE=<work>/store). It is seeded first (toolchain links,
#     crates from the local cargo download cache, else `kong rules` downloads them), then
#     made read-only with chmod before `kong use` and every cargo run.
# Network: kong and cargo children get proxies pointing at a closed loopback port, so any
# download fails (only `kong rules` keeps the real proxy, and only when a crate is missing
# from the local cache). Set KONG_BIN to use a prebuilt kong instead of `cargo build`.
set -u

ONLY=""
KEEP=0
while [ $# -gt 0 ]; do
  case "$1" in
    --only)
      if [ $# -lt 2 ]; then echo "proof-m5: --only needs a pattern" >&2; exit 2; fi
      ONLY="$2"; shift 2 ;;
    --only=*) ONLY="${1#--only=}"; shift ;;
    --full|--quick) shift ;;
    --keep) KEEP=1; shift ;;
    *) echo "proof-m5: unknown argument: $1" >&2; exit 2 ;;
  esac
done

selected() {
  [ -z "$ONLY" ] && return 0
  case "$1" in *"$ONLY"*) return 0 ;; *) return 1 ;; esac
}

report() {
  if [ "$4" -eq 1 ]; then
    echo "$1: measured=$2 threshold=$3 -> PASS"
  else
    echo "$1: measured=$2 threshold=$3 -> FAIL"
    exit 1
  fi
}

note() { echo "proof-m5: $*" >&2; }
die() { echo "proof-m5: $*" >&2; exit 1; }

REPO=$(git rev-parse --show-toplevel) || die "not inside a git repository"
COMMIT=$(git -C "$REPO" rev-parse HEAD) || die "no HEAD commit"
SYS_PATH=/usr/local/bin:/usr/bin:/bin
DEAD_PROXY=http://127.0.0.1:9
REAL_HOME=${HOME:-}
REAL_CARGO_HOME=${CARGO_HOME:-$REAL_HOME/.cargo}
export RUST_TEST_THREADS=${NMC_CPUS:-1}

W=$(mktemp -d "${TMPDIR:-/tmp}/kong-proof-m5.XXXXXX") || die "mktemp failed"
cleanup() {
  chmod -R u+w "$W" 2>/dev/null
  if [ "$KEEP" -eq 1 ]; then note "work dir kept: $W"; else rm -rf "$W"; fi
}
trap cleanup EXIT
H="$W/home"; STORE="$W/store"; CACHE="$W/cache"; LOGS="$W/logs"
mkdir -p "$H/work" "$STORE" "$CACHE" "$LOGS"

# ── kong binary ─────────────────────────────────────────────────────────────
if [ -n "${KONG_BIN:-}" ]; then
  KONG=$KONG_BIN
else
  note "building kong (cargo build --offline --no-default-features)"
  if ! (cd "$REPO" && cargo build --offline --no-default-features) > "$LOGS/build-kong.log" 2>&1; then
    tail -20 "$LOGS/build-kong.log" >&2
    die "cargo build of kong failed (set KONG_BIN to a prebuilt kong)"
  fi
  KONG="$REPO/target/debug/kong"
fi
[ -x "$KONG" ] || die "kong binary not found: $KONG"

# ── store seeding (toolchain links + crates from the local download cache) ─
RUSTC_BIN=${RUSTC:-rustc}
SYSROOT=$("$RUSTC_BIN" --print sysroot) || die "no rustc to seed the store toolchain"
for f in bin/cargo bin/rustc bin/rustdoc lib; do
  [ -e "$SYSROOT/$f" ] || die "incomplete Rust toolchain at $SYSROOT (missing $f)"
done
RUST_VERSION=$("$SYSROOT/bin/rustc" -V | awk '{print $2}' | sed 's/-.*//')
TC="$STORE/rust/toolchain/$RUST_VERSION"
mkdir -p "$TC/bin"
for exe in cargo rustc rustdoc; do ln -s "$SYSROOT/bin/$exe" "$TC/bin/$exe"; done
ln -s "$SYSROOT/lib" "$TC/lib"

LOCK_CRATES=$(awk '
  /^\[\[package\]\]/ { n = ""; v = ""; s = "" }
  /^name = /    { n = $3; gsub(/"/, "", n) }
  /^version = / { v = $3; gsub(/"/, "", v) }
  /^source = "registry\+/ { print n " " v }
' "$REPO/Cargo.lock")
TOTAL=0; SEEDED=0
while read -r name version; do
  [ -n "$name" ] || continue
  TOTAL=$((TOTAL + 1))
  for crate in "$REAL_CARGO_HOME"/registry/cache/*/"$name-$version.crate"; do
    [ -f "$crate" ] || continue
    mkdir -p "$STORE/rust/crates/$name-$version"
    tar -xzf "$crate" -C "$STORE/rust/crates/$name-$version" && SEEDED=$((SEEDED + 1))
    break
  done
done <<EOF
$LOCK_CRATES
EOF
note "store seeded: toolchain $RUST_VERSION (links to $SYSROOT), $SEEDED/$TOTAL crates from $REAL_CARGO_HOME/registry/cache"

# ── hostile HOME cargo config ───────────────────────────────────────────────
# Same shape as the sandbox's: crates-io replaced by a directory registry that
# holds every crate of Cargo.lock, with checksums that do not match it. The crates
# are filled in after `kong rules` (hard links of the store sources).
BOGUS="$H/bogus-registry"
mkdir -p "$BOGUS"
mkdir -p "$H/.cargo"
HOSTILE="$H/.cargo/config.toml"
printf '[source.crates-io]\nreplace-with = "sandbox-vendor"\n\n[source.sandbox-vendor]\ndirectory = "%s"\n' "$BOGUS" > "$HOSTILE"
chmod a-w "$HOSTILE" "$H/.cargo"

if [ -f "$REAL_HOME/.cargo/config.toml" ]; then
  REAL_CFG="$REAL_HOME/.cargo/config.toml($(grep -m1 'replace-with' "$REAL_HOME/.cargo/config.toml" | tr -d ' "' || echo no-replace-with))"
else
  REAL_CFG="none($REAL_HOME/.cargo/config.toml absent)"
fi
note "external Cargo config: sandbox=$REAL_CFG planted=$HOSTILE (crates-io -> sandbox-vendor $BOGUS)"

# ── helpers: a cleared environment, timing ─────────────────────────────────
# clean_env <proxy> <cmd...>: HOME is the hostile home, PATH has no cargo.
clean_env() {
  local proxy=$1; shift
  env -i HOME="$H" PATH="$SYS_PATH" TERM=dumb LANG=C.UTF-8 \
    KONG_STORE="$STORE" KONG_CACHE="$CACHE" \
    RUST_TEST_THREADS="$RUST_TEST_THREADS" NO_PROXY=127.0.0.1,localhost no_proxy=127.0.0.1,localhost \
    HTTPS_PROXY="$proxy" HTTP_PROXY="$proxy" ALL_PROXY="$proxy" \
    https_proxy="$proxy" http_proxy="$proxy" all_proxy="$proxy" "$@"
}
now() { date +%s.%N; }
secs() { awk -v a="$1" -v b="$2" 'BEGIN { printf "%.1f", b - a }'; }
fetched() { grep -Eq '^[[:space:]]*(Updating|Downloading|Downloaded) ' "$1"; }
# Dependency crates compiled (every "Compiling" line except the kong crate itself).
dep_compiles() { grep -E '^[[:space:]]*Compiling ' "$1" | grep -Evc '^[[:space:]]*Compiling kong v'; }
first_error() { grep -m1 -E '^error' "$1" | tr -s ' ' | cut -c1-160; }
# Run `kong` from PATH in a project dir: the clean PATH gets kong's directory.
KONG_DIR=$(cd "$(dirname "$KONG")" && pwd)
in_project() { # in_project <dir> <proxy> <log> <shell script>
  local dir=$1 proxy=$2 log=$3 script=$4
  clean_env "$proxy" bash -c "export PATH='$KONG_DIR':\"\$PATH\"; cd '$dir' && $script" > "$log" 2>&1
}

NEED_CHECK=0; NEED_TEST=0
selected cargo_check && NEED_CHECK=1
selected sandbox_config_override && NEED_CHECK=1
selected cargo_test_node && NEED_TEST=1
selected second_worktree_reuse && NEED_TEST=1

# ── fresh clone, kong rules, kong use ──────────────────────────────────────
C="$H/work/kong"
git clone -q --no-hardlinks "$REPO" "$C" && git -C "$C" -c advice.detachedHead=false checkout -q --detach "$COMMIT" \
  || die "fresh clone of $REPO at $COMMIT failed"
[ "$(git -C "$C" ls-files Cargo.lock)" = Cargo.lock ] || die "the clone carries no committed Cargo.lock"
RULES_PROXY=$DEAD_PROXY
if [ "$SEEDED" -lt "$TOTAL" ]; then
  RULES_PROXY=${HTTPS_PROXY:-${https_proxy:-}}
  note "$((TOTAL - SEEDED)) crates not in the local cache: kong rules downloads them (proxy ${RULES_PROXY:-none})"
fi
T0=$(now)
in_project "$C" "$RULES_PROXY" "$LOGS/rules1.log" "kong rules" || { tail -20 "$LOGS/rules1.log" >&2; die "kong rules failed in the clone"; }
RULES_S=$(secs "$T0" "$(now)")
for d in "$STORE"/rust/crates/*/*/; do
  crate=$(basename "$d")
  cp -al "$d" "$BOGUS/$crate" && printf '{"files":{},"package":"%064d"}' 0 > "$BOGUS/$crate/.cargo-checksum.json"
done
chmod -R a-w "$STORE"
T0=$(now)
in_project "$C" "$DEAD_PROXY" "$LOGS/use1.log" "kong use" || { tail -20 "$LOGS/use1.log" >&2; die "kong use failed in the clone (store read-only)"; }
USE_S=$(secs "$T0" "$(now)")
note "clone at ${COMMIT:0:12}: kong rules ${RULES_S} s, kong use ${USE_S} s (store read-only)"
ENV_CFG="$C/.rust-toolchain/cargo-home/config.toml"
ACT=". .rust-toolchain/activate.sh"

# ── registry_offline ───────────────────────────────────────────────────────
if selected registry_offline; then
  ok=0; detail=""
  in_project "$C" "$DEAD_PROXY" "$LOGS/metadata.log" "$ACT && cargo metadata --locked --offline --format-version 1 >/dev/null"
  meta=$?
  lock_clean=$(git -C "$C" status --porcelain -- Cargo.lock | wc -l)
  vendor=$(sed -n 's/^directory = "\(.*\)"$/\1/p' "$ENV_CFG" 2>/dev/null)
  vendored=$(ls "$vendor" 2>/dev/null | wc -l)
  archives=$(find "$C/.rust-toolchain/cargo-home" \( -name '*.crate' -o -type d -name index \) 2>/dev/null | wc -l)
  if [ "$meta" -eq 0 ] && [ "$lock_clean" -eq 0 ] && [ "$vendored" -ge "$TOTAL" ] && [ "$archives" -eq 0 ] \
     && grep -q 'replace-with = "kong-vendor"' "$ENV_CFG" && ! fetched "$LOGS/metadata.log"; then
    ok=1
  else
    detail=",metadata_exit=$meta,$(first_error "$LOGS/metadata.log")"
  fi
  report registry_offline "vendored=$vendored/$TOTAL,kong_downloaded=$((TOTAL - SEEDED)),cargo_fetched=$archives,lock_rewritten=$lock_clean,store=read-only$detail" \
    'cargo metadata --locked --offline exits 0, every lock crate vendored, nothing fetched, Cargo.lock untouched' "$ok"
fi

# ── cargo_check (plain, under activate.sh) ─────────────────────────────────
CHECK_OK=0
if [ "$NEED_CHECK" -eq 1 ]; then
  T0=$(now)
  in_project "$C" "$DEAD_PROXY" "$LOGS/check.log" "$ACT && cargo check --offline --no-default-features" && ! fetched "$LOGS/check.log" && CHECK_OK=1
  CHECK_S=$(secs "$T0" "$(now)")
  if selected cargo_check; then
    m="exit_ok=$CHECK_OK,seconds=$CHECK_S"
    [ "$CHECK_OK" -eq 1 ] || m="$m,$(first_error "$LOGS/check.log")"
    report cargo_check "$m" 'plain `cargo check --offline --no-default-features` after activate.sh exits 0, no fetch' "$CHECK_OK"
  fi
fi

# ── cargo_test_node (first worktree, cold shared target dir) ───────────────
TEST1_OK=0
if [ "$NEED_TEST" -eq 1 ]; then
  T0=$(now)
  in_project "$C" "$DEAD_PROXY" "$LOGS/test1.log" "$ACT && cargo test --offline --no-default-features node::" \
    && grep -q '^test result: ok' "$LOGS/test1.log" && ! fetched "$LOGS/test1.log" && TEST1_OK=1
  TEST1_S=$(secs "$T0" "$(now)")
  TEST1_DEPS=$(dep_compiles "$LOGS/test1.log")
  if selected cargo_test_node; then
    m="exit_ok=$TEST1_OK,seconds=$TEST1_S,dep_crates_compiled=$TEST1_DEPS,$(grep -m1 '^test result:' "$LOGS/test1.log" | tr ' ' '_' | tr -d ';')"
    [ "$TEST1_OK" -eq 1 ] || m="$m,$(first_error "$LOGS/test1.log")"
    report cargo_test_node "$m" 'plain `cargo test --offline --no-default-features node::` passes, no fetch' "$TEST1_OK"
  fi
fi

# ── second_worktree_reuse (same commit, `eval "$(kong env)"`) ──────────────
if selected second_worktree_reuse; then
  [ "$TEST1_OK" -eq 1 ] || die "second_worktree_reuse needs the first worktree's cargo test to pass (see $LOGS/test1.log)"
  C2="$H/work/kong-wt2"
  git -C "$C" worktree add -q --detach "$C2" "$COMMIT" || die "git worktree add failed"
  in_project "$C2" "$DEAD_PROXY" "$LOGS/use2.log" "kong rules && kong use" || { tail -20 "$LOGS/use2.log" >&2; die "kong rules && kong use failed in the second worktree"; }
  T0=$(now)
  in_project "$C2" "$DEAD_PROXY" "$LOGS/test2.log" 'eval "$(kong env)" && cargo test --offline --no-default-features node::'
  t2=$?
  TEST2_S=$(secs "$T0" "$(now)")
  TEST2_DEPS=$(dep_compiles "$LOGS/test2.log")
  dir1=$(in_project "$C" "$DEAD_PROXY" /dev/stdout "kong env" | sed -n "s/^export CARGO_TARGET_DIR='\(.*\)'$/\1/p")
  dir2=$(in_project "$C2" "$DEAD_PROXY" /dev/stdout "kong env" | sed -n "s/^export CARGO_TARGET_DIR='\(.*\)'$/\1/p")
  ok=0
  if [ "$t2" -eq 0 ] && grep -q '^test result: ok' "$LOGS/test2.log" && [ "$TEST2_DEPS" -eq 0 ] \
     && [ -n "$dir1" ] && [ "$dir1" = "$dir2" ] && ! fetched "$LOGS/test2.log"; then
    ok=1
  fi
  m="dep_crates_recompiled=$TEST2_DEPS,first_seconds=$TEST1_S,second_seconds=$TEST2_S,same_target_dir=$([ -n "$dir1" ] && [ "$dir1" = "$dir2" ] && echo yes || echo no)"
  [ "$ok" -eq 1 ] || m="$m,exit=$t2,$(first_error "$LOGS/test2.log")"
  report second_worktree_reuse "$m" 'second worktree, same Cargo.lock: cargo test node:: passes with 0 dependency crates recompiled' "$ok"
fi

# ── sandbox_config_override ────────────────────────────────────────────────
if selected sandbox_config_override; then
  # Control: the bare store cargo in the same clone, HOME config in force.
  env -i HOME="$H" PATH="$TC/bin:$SYS_PATH" TERM=dumb CARGO_TARGET_DIR="$W/control-target" \
    HTTPS_PROXY="$DEAD_PROXY" HTTP_PROXY="$DEAD_PROXY" ALL_PROXY="$DEAD_PROXY" \
    bash -c "cd '$C' && cargo check --offline --no-default-features" > "$LOGS/bare.log" 2>&1
  bare=$?
  bare_broken=0
  [ "$bare" -ne 0 ] && grep -Eq "$BOGUS|checksum" "$LOGS/bare.log" && bare_broken=1
  # The env lives in RULEZ and is linked into the project as .rust-toolchain.
  env_home=$(in_project "$C" "$DEAD_PROXY" /dev/stdout "kong env" | sed -n "s/^export CARGO_HOME='\(.*\)'$/\1/p")
  wrapper_cfg=0
  [ -n "$env_home" ] && grep -q -- "--config '$env_home/config.toml'" "$C/.rust-toolchain/bin/cargo" \
    && cmp -s "$env_home/config.toml" "$ENV_CFG" && wrapper_cfg=1
  home_ok=0
  [ -n "$env_home" ] && grep -q "^export CARGO_HOME='$env_home'$" "$C/.rust-toolchain/activate.sh" \
    && grep -q "^export CARGO_HOME='$env_home'$" "$C/.rust-toolchain/bin/cargo" && home_ok=1
  ok=0
  [ "$bare_broken" -eq 1 ] && [ "$CHECK_OK" -eq 1 ] && [ "$wrapper_cfg" -eq 1 ] && [ "$home_ok" -eq 1 ] && ok=1
  note "external Cargo config overridden: $HOSTILE (also a parent-directory config of $C)"
  note "  by CARGO_HOME=$env_home and cargo --config $env_home/config.toml (wrapper $C/.rust-toolchain/bin/cargo, linked from RULEZ)"
  m="sandbox_config=$REAL_CFG,planted=$HOSTILE,bare_store_cargo=$([ "$bare_broken" -eq 1 ] && echo FAIL || echo "exit$bare")($(first_error "$LOGS/bare.log")),kong_cargo_check=$([ "$CHECK_OK" -eq 1 ] && echo PASS || echo FAIL),by=CARGO_HOME+--config(config_flag=$wrapper_cfg,cargo_home=$home_ok)"
  report sandbox_config_override "$m" 'bare store cargo fails on the HOME crates-io replacement; kong env (CARGO_HOME + --config) passes' "$ok"
fi
