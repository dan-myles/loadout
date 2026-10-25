#!/usr/bin/env bash
# Runs CI's gates on this checkout: rustfmt, clippy -D warnings, every test, and the release-script
# tests. Every gate runs even after one fails; the exit status is nonzero if any failed.
#
#   scripts/ci/check.sh [--fresh DIR] [GATE...]     GATE: fmt clippy test python (default: all)
#
# By default cargo reuses the usual target directory, and its fingerprints rebuild whatever the
# source changed. --fresh DIR builds in DIR, which must not exist yet, so no artifact from an
# earlier checkout can stand in for this one; use it for evidence you hand on. Tests always run;
# a reused compiled binary is never a reused test result.
set -uo pipefail

root=$(git -C "$(dirname "$0")" rev-parse --show-toplevel) || exit 2
cd "$root" || exit 2

gates=()
while (($#)); do
  case $1 in
    --fresh)
      [[ $# -ge 2 ]] || { echo "--fresh needs a directory" >&2; exit 2; }
      if [[ -e $2 ]]; then
        echo "--fresh $2: already exists; give a new path" >&2
        exit 2
      fi
      mkdir -p "$2" && export CARGO_TARGET_DIR=$(cd "$2" && pwd) || exit 2
      shift 2
      ;;
    fmt | clippy | test | python) gates+=("$1"); shift ;;
    *) echo "unknown argument: $1" >&2; exit 2 ;;
  esac
done
((${#gates[@]})) || gates=(fmt clippy test python)

# Same build settings as .github/workflows/ci.yml.
export CARGO_INCREMENTAL=0 CARGO_PROFILE_DEV_DEBUG=0 PYTHONDONTWRITEBYTECODE=1

head=$(git rev-parse HEAD)
dirty=$(git status --porcelain --untracked-files=normal)
echo "checking $head${dirty:+ plus uncommitted changes}"
echo "target dir: ${CARGO_TARGET_DIR:-$root/target}"

results=()
run() {
  local name=$1 start=$SECONDS
  shift
  echo "::: $name: $*"
  "$@"
  local status=$?
  results+=("$(printf '%-28s exit %-3s %4ss' "$name" "$status" $((SECONDS - start)))")
  return "$status"
}

failed=0
for gate in "${gates[@]}"; do
  case $gate in
    fmt) run rustfmt cargo fmt --all --check || failed=1 ;;
    clippy) run clippy cargo clippy --locked --workspace --all-targets -- -D warnings || failed=1 ;;
    test)
      if cargo nextest --version >/dev/null 2>&1; then
        run "tests (nextest)" cargo nextest run --locked --workspace --no-fail-fast || failed=1
        run doctests cargo test --locked --workspace --doc --no-fail-fast || failed=1
      else
        echo "cargo-nextest not installed: running test binaries one at a time with cargo test"
        run tests cargo test --locked --workspace --no-fail-fast || failed=1
      fi
      ;;
    python) run "release script tests" python3 -m unittest discover -s scripts/release || failed=1 ;;
  esac
done

echo
printf '%s\n' "${results[@]}"
if [[ $(git rev-parse HEAD) != "$head" || $(git status --porcelain --untracked-files=normal) != "$dirty" ]]; then
  echo "the checkout changed while checking; these results do not describe it" >&2
  exit 1
fi
echo "results for $head${dirty:+ plus uncommitted changes}"
exit "$failed"
