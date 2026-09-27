#!/usr/bin/env bash
# pr-ready.sh - pre-flight checks for a cccc (ChesterRa/cccc) feature branch.
# Run inside a local clone of the cccc fork, on the feature branch.
# Read-only with respect to the branch: it fetches upstream and runs cargo;
# it never commits, rebases, or pushes.
set -euo pipefail

usage() {
  cat <<'USAGE'
Usage: pr-ready.sh [--help] [--no-fetch] [--upstream REMOTE] [--base BRANCH]

Pre-flight checks for a cccc feature branch before opening/updating a PR:
  1. git fetch <upstream> <base>           (default: upstream main)
  2. is HEAD based on the latest <upstream>/<base>? (merge-base check)
  3. list changed files and the Rust crates they belong to
  4. cargo fmt --check                     (workspace)
  5. cargo clippy -p <crate> --all-targets -- -D warnings   (touched crates only)
  6. cargo test -p <crate>                 (touched crates only)
  7. duplicate test names: cargo test -p <crate> -- --list | sort | uniq -d
  8. one-screen PASS/FAIL summary; exit 0 only if everything passed

Puts $HOME/.cargo/bin first on PATH. Honours CARGO_TARGET_DIR.
Options:
  --no-fetch        skip the git fetch (use the existing remote-tracking ref)
  --upstream NAME   upstream remote name (default: upstream)
  --base BRANCH     upstream base branch (default: main)
  -h, --help        show this help
USAGE
}

UPSTREAM=upstream
BASE=main
DO_FETCH=1
while [[ $# -gt 0 ]]; do
  case "$1" in
    -h|--help) usage; exit 0 ;;
    --no-fetch) DO_FETCH=0 ;;
    --upstream) UPSTREAM=${2:?--upstream needs a value}; shift ;;
    --base) BASE=${2:?--base needs a value}; shift ;;
    *) echo "unknown argument: $1" >&2; usage >&2; exit 2 ;;
  esac
  shift
done

export PATH="$HOME/.cargo/bin:$PATH"

ROOT=$(git rev-parse --show-toplevel) || { echo "not inside a git repository" >&2; exit 2; }
cd "$ROOT"
BRANCH=$(git rev-parse --abbrev-ref HEAD)
REF="$UPSTREAM/$BASE"
LOGDIR=$(mktemp -d "${TMPDIR:-/tmp}/pr-ready.XXXXXX")

declare -a RESULTS=()
FAILED=0
record() { # record STATUS LABEL [DETAIL]
  RESULTS+=("$(printf '%-4s  %-40s %s' "$1" "$2" "${3:-}")")
  if [[ "$1" == FAIL ]]; then FAILED=1; fi
  return 0
}
run_step() { # run_step LABEL LOGNAME cmd...
  local label=$1 log="$LOGDIR/$2.log"
  shift 2
  echo "==> $label"
  if "$@" >"$log" 2>&1; then
    record PASS "$label"
  else
    record FAIL "$label" "log: $log"
    tail -n 15 "$log" | sed 's/^/    /'
  fi
}

echo "repo:   $ROOT"
echo "branch: $BRANCH"
echo "cargo:  $(command -v cargo) ($(cargo --version 2>/dev/null || echo '?'))"
echo "target: ${CARGO_TARGET_DIR:-<default>}"
echo "logs:   $LOGDIR"
echo

# 1-2. fetch + merge-base check
if [[ $DO_FETCH -eq 1 ]]; then
  if git fetch --quiet "$UPSTREAM" "$BASE"; then
    record PASS "fetch $REF"
  else
    record FAIL "fetch $REF"
  fi
fi
UP_SHA=$(git rev-parse --verify --quiet "$REF^{commit}" || true)
if [[ -z "$UP_SHA" ]]; then
  record FAIL "based on latest $REF" "ref $REF not found"
  printf '%s\n' "${RESULTS[@]}"
  exit 1
fi
MB=$(git merge-base HEAD "$REF")
BEHIND=$(git rev-list --count "HEAD..$REF")
AHEAD=$(git rev-list --count "$REF..HEAD")
if [[ "$MB" == "$UP_SHA" ]]; then
  record PASS "based on latest $REF" "ahead $AHEAD"
else
  record FAIL "based on latest $REF" "behind $BEHIND, ahead $AHEAD: rebase"
fi

# 3. changed files -> crates (nearest ancestor Cargo.toml with a [package])
declare -A CRATES=()
crate_of() {
  local d
  d=$(dirname "$1")
  while :; do
    if [[ -f "$d/Cargo.toml" ]] && grep -q '^\[package\]' "$d/Cargo.toml"; then
      sed -n '/^\[package\]/,/^\[/{s/^name[[:space:]]*=[[:space:]]*"\([^"]*\)".*/\1/p}' \
        "$d/Cargo.toml" | head -n1
      return 0
    fi
    if [[ "$d" == "." || "$d" == "/" ]]; then return 0; fi
    d=$(dirname "$d")
  done
}
echo "Changed files vs merge-base with $REF:"
mapfile -t FILES < <(git diff --name-only "$MB" HEAD)
if [[ ${#FILES[@]} -eq 0 ]]; then echo "  (none)"; fi
for f in "${FILES[@]}"; do
  c=""
  if [[ -e "$f" ]]; then c=$(crate_of "$f"); fi
  printf '  %-58s %s\n' "$f" "${c:-(no crate)}"
  if [[ -n "$c" ]]; then CRATES["$c"]=1; fi
done
CRATE_LIST=()
if [[ ${#CRATES[@]} -gt 0 ]]; then
  mapfile -t CRATE_LIST < <(printf '%s\n' "${!CRATES[@]}" | sort)
fi
echo "Touched crates: ${CRATE_LIST[*]:-(none)}"
echo

# 4. fmt (workspace-wide, as CI does)
run_step "cargo fmt --check" fmt cargo fmt --all -- --check

# 5-7. per-crate clippy, tests, duplicate test names
for c in "${CRATE_LIST[@]}"; do
  run_step "clippy -p $c (-D warnings)" "clippy-$c" \
    cargo clippy -p "$c" --all-targets -- -D warnings
  run_step "test -p $c" "test-$c" cargo test -p "$c"
  echo "==> duplicate test names in $c"
  list="$LOGDIR/list-$c.log"
  if cargo test -p "$c" -- --list >"$list" 2>&1; then
    dups=$(grep -E ': test$' "$list" | sort | uniq -d || true)
    n=$(grep -cE ': test$' "$list" || true)
    if [[ -z "$dups" ]]; then
      record PASS "no duplicate test names in $c" "$n tests listed"
    else
      record FAIL "no duplicate test names in $c" "$(tr '\n' ' ' <<<"$dups")"
    fi
  else
    record FAIL "no duplicate test names in $c" "listing failed: $list"
  fi
done

# 8. summary
echo
echo "================ pr-ready: $BRANCH ================"
printf '%s\n' "${RESULTS[@]}"
echo "==================================================="
if [[ $FAILED -eq 0 ]]; then
  echo "OVERALL: PASS"
else
  echo "OVERALL: FAIL (logs in $LOGDIR)"
fi
exit "$FAILED"
