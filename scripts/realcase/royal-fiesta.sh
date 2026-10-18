#!/usr/bin/env bash
# Milestone 4 real case: Royal Fiesta builds from GitHub alone. A fresh clone in an
# empty directory runs `kong rules && kong use && npm run web:build` against a cold
# KONG_STORE, then `kong rules` and `kong use` once more (the cached second run).
# Usage (from the repository root):
#   bash scripts/realcase/royal-fiesta.sh [--game <git url or path>] [--branch <b>]
#        [--kong <binary>] [--host <ssh target>] [--out <dir>] [--keep]
# Local run: writes transcript-wsl.txt, dist-listing.txt, kong.rules.json, timings.txt.
# --host H: writes ONLY transcript-vm.txt (the VM run, or the reason it was not performed).
#   Without --kong it builds the static musl binary (target x86_64-unknown-linux-musl).
#   --host uses ssh/scp, so only the supervisor runs it, outside the sandbox, never a card:
#     bash scripts/realcase/royal-fiesta.sh --host "${RF_VM_HOST:-etcito-vm}" --branch nmc/68-kong-dependencies-from-github-b2--m4-royal-fiesta-builds-from --out /root/nmc/evidence/68-m4/royal-fiesta-vm
#   which writes /root/nmc/evidence/68-m4/royal-fiesta-vm/transcript-vm.txt.
# Slow (cold cargo/wasm build): the local run is the programmatic card of .nmc/commands.json.
set -u

# Clean Linux PATH (the card execs this script with no shell setup): cargo first, then
# the system dirs, then inherited entries without spaces and outside /mnt/ (keeps a
# node/nvm install, drops the WSL Windows interop PATH). The raw $PATH is never appended.
clean_path() {
  local out="${CARGO_HOME:-$HOME/.cargo}/bin:/root/.cargo/bin:/usr/local/bin:/usr/bin:/bin:/usr/sbin:/sbin"
  local e rest="${PATH-}:"
  while [ -n "$rest" ]; do
    e="${rest%%:*}"; rest="${rest#*:}"
    case "$e" in ""|*" "*|/mnt/*) continue ;; esac
    case ":$out:" in *":$e:"*) continue ;; esac
    out="$out:$e"
  done
  printf '%s' "$out"
}
PATH=$(clean_path)
export PATH

DEFAULT_GAME=https://github.com/Etcito/royal-fiesta.git
GAME=$DEFAULT_GAME
BRANCH=""
KONG=""
HOST=""
OUT=docs/measurements/royal-fiesta-git-deps
KEEP=0
REMOTE_DIR=""

while [ $# -gt 0 ]; do
  case "$1" in
    --game|--branch|--kong|--host|--out|--_remote)
      if [ $# -lt 2 ]; then echo "royal-fiesta: $1 needs a value" >&2; exit 2; fi
      case "$1" in
        --game) GAME="$2" ;;
        --branch) BRANCH="$2" ;;
        --kong) KONG="$2" ;;
        --host) HOST="$2" ;;
        --out) OUT="$2" ;;
        --_remote) REMOTE_DIR="$2" ;;  # internal: steps 2-5 on the VM, transcript on stdout
      esac
      shift 2 ;;
    --keep) KEEP=1; shift ;;
    *) echo "royal-fiesta: unknown argument: $1" >&2; exit 2 ;;
  esac
done

# The VM side (--_remote) never builds, so it does not need cargo.
if [ -z "$REMOTE_DIR" ] && ! command -v cargo > /dev/null 2>&1; then
  echo "royal-fiesta: cargo not found on PATH ($PATH)" >&2
  exit 2
fi

# Replaces token values and token-shaped strings with ***. Tokens reach awk through
# its environment only, never through argv.
redact() {
  KONG_GIT_TOKEN="${KONG_GIT_TOKEN-}" GITHUB_TOKEN="${GITHUB_TOKEN-}" GH_TOKEN="${GH_TOKEN-}" awk '
    BEGIN {
      n = 0; split("KONG_GIT_TOKEN GITHUB_TOKEN GH_TOKEN", v, " ")
      for (i = 1; i <= 3; i++) if (ENVIRON[v[i]] != "") t[++n] = ENVIRON[v[i]]
    }
    {
      line = $0
      for (i = 1; i <= n; i++) {
        out = ""
        while ((p = index(line, t[i])) > 0) {
          out = out substr(line, 1, p - 1) "***"; line = substr(line, p + length(t[i]))
        }
        line = out line
      }
      gsub(/ghp_[A-Za-z0-9]+|github_pat_[A-Za-z0-9_]+|gho_[A-Za-z0-9]+/, "***", line)
      gsub(/AUTHORIZATION: [^ ]+ [^ ]+/, "***", line)
      print line
    }'
}

strip_ansi() { sed -E 's/\x1b\[[0-9;?]*[A-Za-z]//g; s/\r$//'; }

now() { date +%s.%N; }
elapsed() { awk -v a="$1" -v b="$2" 'BEGIN { printf "%.1f", b - a }'; }

# step <dir> <display> <cmd...>: appends `$ display`, the ANSI-stripped combined output
# and `exit=N` to $TX. Sets RC, WALL and leaves the stripped output in $W/last.out.
# A failure does not stop the run: every exit code is recorded.
step() {
  local dir="$1" display="$2" t0 t1
  shift 2
  printf '$ %s\n' "$display" >> "$TX"
  t0=$(now)
  ( cd "$dir" && "$@" ) < /dev/null > "$W/last.raw" 2>&1
  RC=$?
  t1=$(now)
  WALL=$(elapsed "$t0" "$t1")
  strip_ansi < "$W/last.raw" > "$W/last.out"
  cat "$W/last.out" >> "$TX"
  printf 'exit=%s\n' "$RC" >> "$TX"
  printf 'wall_s %s=%s\n' "$display" "$WALL" >> "$W/wall.txt"
}

# Kong's own timing lines: "Cloned|Installed|Built <name> <commit12> in X s".
kong_timings() {
  grep -oE '(Cloned|Installed|Built) [^ ]+ [0-9a-f]{12} in [0-9.]+ s' "$W/last.out" \
    | sed "s/^/$1: /" >> "$W/kong-timings.txt"
}

# use_npm: npm from PATH, else the Kong-managed Node runtime (its bin dir is prepended).
use_npm() {
  local d
  if command -v npm > /dev/null 2>&1; then
    echo "# npm from PATH: $(command -v npm)" >> "$TX"; return
  fi
  for d in "$KONG_STORE"/node/runtime/*/bin; do
    if [ -x "$d/npm" ]; then
      export PATH="$d:$PATH"
      echo "# npm from the Kong-managed Node runtime: $d" >> "$TX"; return
    fi
  done
  echo "# npm not found on PATH nor under \$KONG_STORE/node/runtime/*/bin" >> "$TX"
}

# Steps 2-5: clone into an empty parent, cold first run, cached second run.
# Needs W (work dir), TX (transcript) and KONG (absolute). Sets P, CLONE_S, FIRST_OK.
run_case() {
  P=$(mktemp -d)
  mkdir -p "$W/bin"
  ln -sf "$KONG" "$W/bin/kong"
  export PATH="$W/bin:$PATH"

  local clone_args=(git clone) display="git clone"
  if [ -n "$BRANCH" ]; then clone_args+=(--branch "$BRANCH"); display="$display --branch $BRANCH"; fi
  clone_args+=("$GAME" royal-fiesta)
  step "$P" "$display $GAME royal-fiesta" "${clone_args[@]}"
  CLONE_S=$WALL

  step "$P" "ls -A $P" ls -A "$P"
  echo "siblings=$(grep -vxF royal-fiesta "$W/last.out" | grep -c .)" >> "$TX"

  local C="$P/royal-fiesta"
  step "$C" "git rev-parse HEAD" git rev-parse HEAD

  export KONG_STORE="$P/store"
  echo "# KONG_STORE=\$P/store (empty: cold clone and build)" >> "$TX"
  step "$C" "kong --version" kong --version

  local rc_rules rc_use rc_npm
  step "$C" "kong rules" kong rules; rc_rules=$RC; kong_timings run1
  step "$C" "kong use" kong use; rc_use=$RC; kong_timings run1
  use_npm
  step "$C" "npm run web:build" npm run web:build; rc_npm=$RC

  echo "# second run (cached)" >> "$TX"
  step "$C" "kong rules" kong rules; kong_timings run2
  sed -i '$ s/^wall_s kong rules=/wall_s kong rules (cached)=/' "$W/wall.txt"
  step "$C" "kong use" kong use; kong_timings run2
  sed -i '$ s/^wall_s kong use=/wall_s kong use (cached)=/' "$W/wall.txt"

  FIRST_OK=0
  [ "$rc_rules" -eq 0 ] && [ "$rc_use" -eq 0 ] && [ "$rc_npm" -eq 0 ] && FIRST_OK=1
}

cleanup_dir() {
  [ -n "${1-}" ] && [ -d "$1" ] || return 0
  chmod -R u+w "$1" 2> /dev/null
  rm -rf "$1"
}

P=""
W=""
on_exit() {
  if [ "$KEEP" -eq 1 ]; then
    [ -n "$P" ] && echo "royal-fiesta: kept $P" >&2
    [ -n "$W" ] && echo "royal-fiesta: kept $W" >&2
    return 0
  fi
  cleanup_dir "$P"
  cleanup_dir "$W"
}
trap on_exit EXIT

# --- internal: the VM side of --host (the transcript goes to stdout) ---------------
if [ -n "$REMOTE_DIR" ]; then
  KONG="$REMOTE_DIR/kong"
  W=$(mktemp -d)
  TX=/dev/stdout
  run_case
  [ "$FIRST_OK" -eq 1 ] && exit 0
  exit 1
fi

ROOT=$(cd "$(dirname "$0")/../.." && pwd)
mkdir -p "$OUT" || { echo "royal-fiesta: cannot create $OUT" >&2; exit 2; }
OUT=$(cd "$OUT" && pwd)

# Step 1: the kong binary (built headless when --kong is absent), made absolute.
# --host builds the static musl binary, which runs on any Linux VM.
resolve_kong() {
  if [ -z "$KONG" ] && [ -n "$HOST" ]; then
    echo "royal-fiesta: cargo build --release --no-default-features --target x86_64-unknown-linux-musl" >&2
    ( cd "$ROOT" && cargo build --release --no-default-features --target x86_64-unknown-linux-musl ) >&2 \
      || { echo "royal-fiesta: cargo build failed" >&2; exit 1; }
    KONG="$ROOT/target/x86_64-unknown-linux-musl/release/kong"
  elif [ -z "$KONG" ]; then
    echo "royal-fiesta: cargo build --release --no-default-features" >&2
    ( cd "$ROOT" && cargo build --release --no-default-features ) >&2 \
      || { echo "royal-fiesta: cargo build failed" >&2; exit 1; }
    KONG="$ROOT/target/release/kong"
  fi
  [ -f "$KONG" ] || { echo "royal-fiesta: kong binary not found: $KONG" >&2; exit 2; }
  KONG="$(cd "$(dirname "$KONG")" && pwd)/$(basename "$KONG")"
}

# --- VM run ------------------------------------------------------------------------
if [ -n "$HOST" ]; then
  VM="$OUT/transcript-vm.txt"
  ALIAS="${HOST##*@}"
  W=$(mktemp -d)
  SSH_OPTS=(-o BatchMode=yes -o ConnectTimeout=15)
  if ! ssh "${SSH_OPTS[@]}" "$HOST" true < /dev/null 2> "$W/ssh.err"; then
    { echo "# VM run not performed: host $ALIAS unreachable"; strip_ansi < "$W/ssh.err"; } \
      | redact > "$VM"
    echo "royal-fiesta: host $ALIAS unreachable, reason written to $VM" >&2
    exit 0
  fi
  resolve_kong
  NOTE=""
  if [ -e "$GAME" ]; then
    NOTE="# --game $GAME is a local path, not on the VM: cloning $DEFAULT_GAME"
    GAME=$DEFAULT_GAME
  fi
  {
    echo "# Royal Fiesta real case on the VM (host alias $ALIAS), $(date -u +%Y-%m-%dT%H:%M:%SZ)."
    [ -n "$NOTE" ] && echo "$NOTE"
    echo '$ uname -sr'
    ssh "${SSH_OPTS[@]}" "$HOST" uname -sr < /dev/null 2>&1
    echo "exit=$?"
  } > "$W/vm.raw"
  RD=$(ssh "${SSH_OPTS[@]}" "$HOST" mktemp -d < /dev/null 2>> "$W/vm.raw")
  RC=1
  if [ -z "$RD" ]; then
    echo "# VM run not performed: remote mktemp -d failed" >> "$W/vm.raw"
  elif ! scp -q "${SSH_OPTS[@]}" "$KONG" "$HOST:$RD/kong" < /dev/null >> "$W/vm.raw" 2>&1 \
     || ! scp -q "${SSH_OPTS[@]}" "$0" "$HOST:$RD/royal-fiesta.sh" < /dev/null >> "$W/vm.raw" 2>&1; then
    echo "# VM run not performed: copying kong to the VM failed" >> "$W/vm.raw"
  else
    REMOTE_CMD="chmod +x $(printf %q "$RD/kong") && bash $(printf %q "$RD/royal-fiesta.sh")"
    REMOTE_CMD="$REMOTE_CMD --_remote $(printf %q "$RD") --game $(printf %q "$GAME")"
    [ -n "$BRANCH" ] && REMOTE_CMD="$REMOTE_CMD --branch $(printf %q "$BRANCH")"
    [ "$KEEP" -eq 1 ] && REMOTE_CMD="$REMOTE_CMD --keep"
    ssh "${SSH_OPTS[@]}" "$HOST" "$REMOTE_CMD" < /dev/null >> "$W/vm.raw" 2>&1
    RC=$?
  fi
  if [ -n "$RD" ] && [ "$KEEP" -eq 0 ]; then
    ssh "${SSH_OPTS[@]}" "$HOST" "rm -rf $(printf %q "$RD")" < /dev/null > /dev/null 2>&1
  fi
  redact < "$W/vm.raw" > "$VM"
  echo "royal-fiesta: VM transcript written to $VM (exit=$RC)" >&2
  [ "$RC" -eq 0 ] && exit 0
  exit 1
fi

# --- local run -----------------------------------------------------------------------
resolve_kong
[ -d "$GAME" ] && GAME=$(cd "$GAME" && pwd)
W=$(mktemp -d)
TX="$W/transcript.raw"
: > "$W/wall.txt"
: > "$W/kong-timings.txt"
{
  echo "# Royal Fiesta real case, $(date -u +%Y-%m-%dT%H:%M:%SZ), $(uname -sr)."
  echo "# Game: $GAME${BRANCH:+ (branch $BRANCH)}. Empty parent \$P from mktemp -d."
  echo "# PATH=$PATH"
} > "$TX"
# Toolchain preflight (shows e.g. a missing wasm32-unknown-unknown target up front).
step "$ROOT" "cargo --version" cargo --version
command -v rustup > /dev/null 2>&1 \
  && step "$ROOT" "rustup target list --installed" rustup target list --installed
command -v node > /dev/null 2>&1 && step "$ROOT" "node --version" node --version
command -v npm > /dev/null 2>&1 && step "$ROOT" "npm --version" npm --version
: > "$W/wall.txt"  # timings.txt holds the real-case commands only

run_case
C="$P/royal-fiesta"

redact < "$TX" > "$OUT/transcript-wsl.txt"
if [ -d "$C/apps/web/dist" ]; then
  ( cd "$C" && find apps/web/dist -type f -printf '%s %P\n' | sort -k2 ) > "$OUT/dist-listing.txt"
else
  : > "$OUT/dist-listing.txt"
fi
if [ -f "$C/kong.rules" ]; then
  redact < "$C/kong.rules" > "$OUT/kong.rules.json"
else
  rm -f "$OUT/kong.rules.json"
fi
{
  echo "clone_s=$CLONE_S"
  cat "$W/kong-timings.txt"
  cat "$W/wall.txt"
} | redact > "$OUT/timings.txt"

echo "royal-fiesta: evidence written to $OUT (first run ok=$FIRST_OK)" >&2
[ "$FIRST_OK" -eq 1 ] && exit 0
exit 1
