#!/usr/bin/env bash
# M4 proof: validate the committed Royal Fiesta real-case evidence without network access.
# Usage (from the repository root): bash scripts/proof-m4.sh [--only P]
set -u

D=docs/measurements/royal-fiesta-git-deps
T="$D/transcript-wsl.txt"
R="$D/kong.rules.json"
L="$D/dist-listing.txt"
ONLY=""

while [ $# -gt 0 ]; do
  case "$1" in
    --only)
      if [ $# -lt 2 ]; then echo "proof-m4: --only needs a pattern" >&2; exit 2; fi
      ONLY="$2"; shift 2 ;;
    --only=*) ONLY="${1#--only=}"; shift ;;
    *) echo "proof-m4: unknown argument: $1" >&2; exit 2 ;;
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

check_commands() {
  local ok=0
  if [ -f "$T" ] && awk '
    /^# second run \(cached\)$/ { second = 1; if (pending) bad = 1 }
    !second && /^\$ / {
      if (pending) bad = 1
      pending = 1
    }
    !second && /^exit=/ {
      if (!pending || $0 != "exit=0") bad = 1
      pending = 0
    }
    !second && /^\$ kong rules$/ { rules = 1 }
    !second && /^\$ kong use$/ { use = 1 }
    !second && /^\$ npm run web:build$/ { build = 1 }
    END { exit !(second && !pending && rules && use && build && !bad) }
  ' "$T" && awk '
    /^# second run \(cached\)$/ { second = 1 }
    second && /^\$ / {
      if (pending) bad = 1
      pending = 1
      if ($0 == "$ kong rules") rules = 1
      if ($0 == "$ kong use") use = 1
    }
    second && /Cloned |Built |Installed / { cached_work = 1 }
    second && /^exit=/ {
      if (!pending || $0 != "exit=0") bad = 1
      pending = 0
    }
    END { exit !(second && !pending && rules && use && !cached_work && !bad) }
  ' "$T"; then
    ok=1
  fi
  report commands_ok "$ok" 'all first-run exits=0; cached rules/use exit=0 with no clone/build/install' "$ok"
}

check_empty_dir() {
  local siblings=missing ok=0
  if [ -f "$T" ]; then
    siblings=$(sed -n 's/^siblings=//p' "$T" | head -n1)
  fi
  [ "$siblings" = 0 ] && ok=1
  report empty_dir "$siblings" 'siblings=0' "$ok"
}

check_pinned() {
  local commits=0 ok=0
  if [ -f "$R" ]; then
    commits=$(grep -Eo '"commit"[[:space:]]*:[[:space:]]*"[0-9a-f]{40}"' "$R" | wc -l | tr -d ' ')
    if [ "$commits" -ge 3 ] \
       && grep -q '"name": "@etcito/raa"' "$R" \
       && grep -A12 '"name": "@etcito/raa"' "$R" | grep -q '"build":' \
       && grep -q '"name": "@gamesaloon/rollback"' "$R" \
       && grep -q '"name": "@gamesaloon/rollback-rapier"' "$R"; then
      ok=1
    fi
  fi
  report pinned "$commits" '>=3 pinned commits; @etcito/raa build; both rollback packages' "$ok"
}

check_dist() {
  local bytes=0 has_index=0 has_js=0 ok=0
  if [ -f "$L" ]; then
    read -r bytes has_index has_js < <(awk '
      NF >= 2 {
        if ($1 ~ /^[0-9]+$/) total += $1
        if ($2 == "index.html") has_index = 1
        if ($2 ~ /\.js$/) js = 1
      }
      END { print total + 0, has_index + 0, js + 0 }
    ' "$L")
  fi
  if [ "$bytes" -gt 0 ] && [ "$has_index" -eq 1 ] && [ "$has_js" -eq 1 ]; then ok=1; fi
  report dist "bytes=$bytes index=$has_index js=$has_js" 'index.html, >=1 .js, total bytes>0' "$ok"
}

check_no_secret() {
  local matches=0 f count ok=1
  for f in "$D"/*; do
    [ -f "$f" ] || continue
    count=$(grep -Eic 'ghp_|github_pat_|gho_|AUTHORIZATION[[:space:]]*:' "$f" || true)
    matches=$((matches + count))
  done
  [ "$matches" -eq 0 ] || ok=0
  report no_secret "$matches" 0 "$ok"
}

selected commands_ok && check_commands
selected empty_dir && check_empty_dir
selected pinned && check_pinned
selected dist && check_dist
selected no_secret && check_no_secret
