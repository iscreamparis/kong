#!/usr/bin/env bash
# Milestone 1 proof: the fresh-clone failure is reproduced and the git-dependency
# design is written. Plain bash + grep: no cargo, no network.
# Usage (from the repository root): bash scripts/proof-m1.sh [--only P] [--quick|--full]
# --quick and --full change nothing: M1 has no unit-test module set.
set -u

D=docs/git-dependencies.md
T=docs/measurements/2026-10-18-royal-fiesta-fresh-clone.txt
ONLY=""

while [ $# -gt 0 ]; do
  case "$1" in
    --only)
      if [ $# -lt 2 ]; then echo "proof-m1: --only needs a pattern" >&2; exit 2; fi
      ONLY="$2"; shift 2 ;;
    --only=*) ONLY="${1#--only=}"; shift ;;
    --quick|--full) shift ;;
    *) echo "proof-m1: unknown argument: $1" >&2; exit 2 ;;
  esac
done

selected() {
  [ -z "$ONLY" ] && return 0
  case "$1" in *"$ONLY"*) return 0 ;; *) return 1 ;; esac
}

# report <label> <measured> <threshold> <ok:0|1>; exits 1 on FAIL.
report() {
  if [ "$4" -eq 1 ]; then
    echo "$1: measured=$2 threshold=$3 -> PASS"
  else
    echo "$1: measured=$2 threshold=$3 -> FAIL"
    exit 1
  fi
}

check_doc_sections() {
  local n=0 h
  for h in '## Measurements' '## Declaration' '## kong.rules' '## Store layout' \
           '## Credentials' '## Build step' '## Updating' '## Limits' '## Open questions'; do
    if [ -f "$D" ] && grep -qxF -- "$h" "$D"; then n=$((n + 1)); fi
  done
  local ok=0; [ "$n" -eq 9 ] && ok=1
  report doc_sections "$n" 9 "$ok"
}

check_failure_reproduced() {
  local code=none ok=0
  if [ -f "$T" ]; then
    # Exit code of the first `exit=` line after the `$ kong rules` line.
    code=$(awk '
      $0 == "$ kong rules" { seen = 1; next }
      seen && /^exit=/ { sub(/^exit=/, ""); print; exit }
    ' "$T")
    [ -n "$code" ] || code=none
    if grep -qF 'points at a directory that does not exist' "$T" \
       && grep -qF '@gamesaloon/rollback' "$T" \
       && [[ "$code" =~ ^[0-9]+$ ]] && [ "$code" -ne 0 ]; then
      ok=1
    fi
  fi
  report failure_reproduced "$code" 'exit!=0+error_text' "$ok"
}

check_fresh_clone() {
  local n=none ok=0
  if [ -f "$T" ]; then
    n=$(grep -m1 -E '^siblings=[0-9]+$' "$T" | sed 's/^siblings=//')
    [ -n "$n" ] || n=none
  fi
  [ "$n" = "0" ] && ok=1
  report fresh_clone "$n" 'siblings=0' "$ok"
}

check_no_secret() {
  local total=0 c f ok=1
  for f in "$D" "$T"; do
    if [ ! -f "$f" ]; then ok=0; continue; fi
    c=$(grep -E -c 'ghp_|github_pat_|gho_|x-access-token|AUTHORIZATION:' "$f")
    total=$((total + ${c:-0}))
  done
  [ "$total" -eq 0 ] || ok=0
  report no_secret "$total" 0 "$ok"
}

selected doc_sections && check_doc_sections
selected failure_reproduced && check_failure_reproduced
selected fresh_clone && check_fresh_clone
selected no_secret && check_no_secret
exit 0
