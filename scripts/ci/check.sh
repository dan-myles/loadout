#!/usr/bin/env bash
# Runs CI's gates on this checkout: rustfmt, clippy -D warnings, every test, and the release-script
# tests. Every gate runs even after one fails; the exit status is nonzero if any failed.
#
#   scripts/ci/check.sh [--fresh DIR] [GATE...]     GATE: fmt clippy test python (default: all)
#
# By default cargo reuses the usual target directory, as it always does. --fresh DIR instead builds
# in DIR, which this script creates and which must not exist yet, must be outside this checkout and
# must have an existing parent; use it for evidence you hand on, so no earlier build is reused.
# Tests always run; a reused compiled binary is never a reused test result.
#
# Results are tied to the checkout's contents, not only its commit: just before and just after the
# gates the script fingerprints HEAD, the index, and the bytes, executable bit and symlink target of
# every tracked and untracked file. Ignored files (target/, __pycache__/, *.pyc) and anything outside
# the checkout, such as the toolchain or ~/.cargo, are not covered. Equal fingerprints do not show
# that nothing changed in between and was put back. Uncommitted changes are fine and are reported.
#
# Exit 2: nothing was run (bad arguments, a target dir that would change the checkout, or a
# checkout Git cannot describe: no commit, a merge conflict, a submodule, an untracked nested
# repository, a file name with a newline, or any failing Git command). Exit 1: a gate failed, or
# the fingerprint changed or could not be read again. Exit 0: every gate passed on this checkout.
set -uo pipefail

root=$(git -C "$(dirname "$0")" rev-parse --show-toplevel) && root=$(cd "$root" && pwd -P) || exit 2
cd "$root" || exit 2

# The absolute path of $1, whose parent must exist; $1 itself need not.
resolve() {
  local parent
  parent=$(cd "$(dirname -- "$1")" 2>/dev/null && pwd -P) || return 1
  printf '%s/%s\n' "${parent%/}" "$(basename -- "$1")"
}

fresh=
gates=()
while (($#)); do
  case $1 in
    --fresh)
      if [[ $# -lt 2 || -z $2 || $2 == -* ]]; then
        echo "--fresh needs a directory" >&2
        exit 2
      fi
      if [[ -e $2 || -L $2 ]]; then
        echo "--fresh $2: already exists; give a new path" >&2
        exit 2
      fi
      if ! fresh=$(resolve "$2"); then
        echo "--fresh $2: its parent directory does not exist" >&2
        exit 2
      fi
      if [[ $fresh == "$root" || $fresh == "$root"/* ]]; then
        echo "--fresh $2: must be outside the checkout, or the build would change what is checked" >&2
        exit 2
      fi
      shift 2
      ;;
    fmt | clippy | test | python) gates+=("$1"); shift ;;
    *) echo "unknown argument: $1" >&2; exit 2 ;;
  esac
done
((${#gates[@]})) || gates=(fmt clippy test python)

# Prints a NUL-separated description of HEAD, the index and every tracked and untracked file, or
# fails if Git does or the checkout holds something it does not describe.
describe() {
  local tmp status=1
  tmp=$(mktemp -d) || return 1
  if describe_into "$tmp"; then status=0; fi
  rm -rf "$tmp"
  return "$status"
}

describe_into() {
  local tmp=$1 record path link stage=$'^[0-7]+ [0-9a-f]+ ([0-3])\t' head
  head=$(git rev-parse --verify --quiet 'HEAD^{commit}') || { echo "HEAD does not name a commit" >&2; return 1; }
  git ls-files -z --stage >"$tmp/index" || return 1
  while IFS= read -r -d '' record; do
    if [[ ! $record =~ $stage ]]; then
      echo "unexpected index entry: $record" >&2
      return 1
    elif [[ ${BASH_REMATCH[1]} != 0 ]]; then
      echo "${record#*$'\t'} has a merge conflict" >&2
      return 1
    elif [[ $record == 160000\ * ]]; then
      echo "${record#*$'\t'} is a submodule" >&2
      return 1
    fi
  done <"$tmp/index"
  git ls-files -z --cached --others --exclude-standard >"$tmp/paths" || return 1
  : >"$tmp/files"
  {
    printf 'HEAD %s\0' "$head"
    cat "$tmp/index"
    while IFS= read -r -d '' path; do
      if [[ $path == *$'\n'* ]]; then
        echo "file name with a newline: $path" >&2
        return 1
      elif [[ -L $path ]]; then
        link=$(readlink -- "$path") || return 1
        printf 'link %s\0%s\0' "$path" "$link"
      elif [[ -f $path ]]; then
        printf '%s\n' "$path" >>"$tmp/files"
        if [[ -x $path ]]; then printf 'exec %s\0' "$path"; fi
      elif [[ ! -e $path ]]; then
        printf 'missing %s\0' "$path"
      else
        echo "$path is neither a file nor a symlink (an untracked nested repository?)" >&2
        return 1
      fi
    done <"$tmp/paths"
    cat "$tmp/files"
    git hash-object --no-filters --stdin-paths <"$tmp/files" || return 1
  } >"$tmp/description" || return 1
  cat "$tmp/description"
}

fingerprint() {
  local id
  id=$(describe | git hash-object --stdin) && [[ -n $id ]] && printf '%s\n' "$id"
}

# A target directory inside the checkout must be ignored, or the build itself changes the checkout.
target=${fresh:-${CARGO_TARGET_DIR:-$root/target}}
[[ $target == /* ]] || target=$root/$target
if resolved=$(resolve "$target"); then target=$resolved; fi
if [[ $target == "$root" || $target == "$root"/* ]] && ! git check-ignore -q -- "$target"; then
  echo "target dir $target is inside the checkout and not ignored by Git" >&2
  exit 2
fi

head=$(git rev-parse --verify --quiet 'HEAD^{commit}') || { echo "HEAD does not name a commit" >&2; exit 2; }
status=$(git status --porcelain --untracked-files=all) || exit 2
before=$(fingerprint) || { echo "cannot fingerprint the checkout; nothing was run" >&2; exit 2; }
state="$head${status:+ plus uncommitted changes}"

# Same build settings as .github/workflows/ci.yml.
export CARGO_INCREMENTAL=0 CARGO_PROFILE_DEV_DEBUG=0 PYTHONDONTWRITEBYTECODE=1
if [[ -n $fresh ]]; then
  mkdir -- "$fresh" || exit 2
  export CARGO_TARGET_DIR=$fresh
fi
echo "checking $state (checkout fingerprint $before)"
echo "target dir: $target"

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
if ! after=$(fingerprint); then
  echo "cannot fingerprint the checkout again; these results are not bound to it" >&2
  exit 1
fi
if [[ $after != "$before" ]]; then
  echo "the checkout changed while checking ($before -> $after); these results do not describe it" >&2
  exit 1
fi
echo "results for $state (checkout fingerprint $before)"
exit "$failed"
