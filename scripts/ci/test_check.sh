#!/usr/bin/env bash
# Orchestration controls for scripts/ci/check.sh. Each case runs a copy of the script in a new
# scratch Git repository with a fake cargo first on PATH, and for some cases a git wrapper that
# fails or acts on a chosen call. Nothing is compiled and no project test runs: these check only
# the script's argument handling, gate sequencing, exit status and result binding.
#
#   scripts/ci/test_check.sh        prints one line per check and exits nonzero if any failed
set -uo pipefail

script=$(cd "$(dirname "$0")" && pwd -P)/check.sh || exit 2
real_git=$(command -v git) || exit 2
scratch=$(mktemp -d) && scratch=$(cd "$scratch" && pwd -P) || exit 2
trap 'rm -rf "$scratch"' EXIT

unset GIT_DIR GIT_WORK_TREE GIT_INDEX_FILE GIT_OBJECT_DIRECTORY CARGO_TARGET_DIR
export GIT_CONFIG_NOSYSTEM=1 GIT_CONFIG_GLOBAL=/dev/null
export GIT_AUTHOR_NAME=control GIT_AUTHOR_EMAIL=control@example.invalid
export GIT_COMMITTER_NAME=control GIT_COMMITTER_EMAIL=control@example.invalid

mkdir -p "$scratch/bin" "$scratch/gitwrap" "$scratch/cases"
cat >"$scratch/bin/cargo" <<'EOF'
#!/usr/bin/env bash
# Fake cargo: logs the step, writes a build file into the target dir, then edits the checkout
# (FAKE_EDIT_ON/FAKE_EDIT) and fails (FAKE_FAIL) when asked.
if [[ $* == "nextest --version" ]]; then [[ -n ${FAKE_NEXTEST-} ]]; exit; fi
case $1 in
  test) if [[ " $* " == *" --doc "* ]]; then step=doc; else step=test; fi ;;
  *) step=$1 ;;
esac
printf '%s|%s|target=%s|incremental=%s|debug=%s\n' "$step" "$*" "${CARGO_TARGET_DIR-}" \
  "${CARGO_INCREMENTAL-}" "${CARGO_PROFILE_DEV_DEBUG-}" >>"$FAKE_LOG"
target=${CARGO_TARGET_DIR:-target}
mkdir -p "$target" && echo "$step" >"$target/built-$step"
if [[ ${FAKE_EDIT_ON-} == "$step" ]]; then eval "$FAKE_EDIT"; fi
[[ " ${FAKE_FAIL-} " != *" $step "* ]]
EOF
cat >"$scratch/gitwrap/git" <<'EOF'
#!/usr/bin/env bash
# From the FAKE_GIT_AT-th call whose arguments contain FAKE_GIT_MATCH on, either fail like Git does
# or run FAKE_GIT_DO first; every other call goes to the real git.
if [[ -n ${FAKE_GIT_MATCH-} && " $* " == *" $FAKE_GIT_MATCH "* ]]; then
  n=$(($(cat "$FAKE_GIT_COUNT" 2>/dev/null || echo 0) + 1))
  echo "$n" >"$FAKE_GIT_COUNT"
  if ((n >= FAKE_GIT_AT)); then
    if [[ -z ${FAKE_GIT_DO-} ]]; then
      echo "fatal: fake failure of git $*" >&2
      exit 128
    fi
    eval "$FAKE_GIT_DO"
  fi
fi
exec "$REAL_GIT" "$@"
EOF
chmod +x "$scratch/bin/cargo" "$scratch/gitwrap/git"

failures=0
pass() { printf 'ok    %s\n' "$*"; }
fail() { printf 'FAIL  %s\n' "$*"; failures=$((failures + 1)); }

# new_repo NAME: a committed repository holding a copy of check.sh, one tracked file and one file
# to stage, with /target ignored as in this project.
new_repo() {
  local repo=$scratch/$1
  mkdir -p "$repo/scripts/ci" &&
    cp "$script" "$repo/scripts/ci/check.sh" &&
    printf '/target\n' >"$repo/.gitignore" &&
    printf 'one\n' >"$repo/source.txt" &&
    printf 'two\n' >"$repo/staged.txt" &&
    git -C "$repo" init -q &&
    git -C "$repo" add -A &&
    git -C "$repo" commit -qm fixture &&
    printf '%s\n' "$repo"
}

# check CASE [VAR=VALUE...] -- REPO ARG...: runs the repository's check.sh with the fakes; leaves
# the exit status in $code, the output in $out and the fake cargo's log in $log.
check() {
  local case=$1 vars=()
  shift
  while [[ $1 != -- ]]; do vars+=("$1"); shift; done
  local repo=$2
  shift 2
  log=$scratch/cases/$case.log out=$scratch/cases/$case.out
  : >"$log"
  rm -f "$scratch/cases/$case.count"
  (cd "$repo" && env PATH="$scratch/gitwrap:$scratch/bin:$PATH" REAL_GIT="$real_git" FAKE_LOG="$log" \
    FAKE_GIT_COUNT="$scratch/cases/$case.count" "${vars[@]}" bash scripts/ci/check.sh "$@") >"$out" 2>&1
  code=$?
}

steps() { cut -d'|' -f1 "$log" | paste -sd' ' -; }
expect_code() { if [[ $code == "$2" ]]; then pass "$1: exit $code"; else fail "$1: exit $code, want $2"; sed 's/^/      /' "$out"; fi; }
expect_steps() { local got; got=$(steps); if [[ $got == "$2" ]]; then pass "$1: cargo steps '${got}'"; else fail "$1: cargo steps '$got', want '$2'"; fi; }
expect_out() { if grep -qF -- "$2" "$out"; then pass "$1: says '$2'"; else fail "$1: no '$2' in output"; sed 's/^/      /' "$out"; fi; }
porcelain() { git -C "$1" status --porcelain --untracked-files=all; }

echo "orchestration controls: fake cargo, no compiler, no project tests"

# Gate sequencing, build environment and failure continuation.
repo=$(new_repo seq) || exit 2
head=$(git -C "$repo" rev-parse HEAD)
check pass FAKE_NEXTEST=1 -- "$repo" fmt clippy test
expect_code pass 0
expect_steps pass "fmt clippy nextest doc"
expect_out pass "results for $head (checkout fingerprint"
if grep -q 'incremental=0|debug=0$' "$log" && ! grep -qv 'incremental=0|debug=0$' "$log"; then
  pass "pass: every cargo call had CARGO_INCREMENTAL=0 CARGO_PROFILE_DEV_DEBUG=0"
else
  fail "pass: build environment"
fi
grep -q '^nextest|nextest run --locked --workspace --no-fail-fast|' "$log" &&
  grep -q '^doc|test --locked --workspace --doc --no-fail-fast|' "$log" &&
  grep -q '^clippy|clippy --locked --workspace --all-targets -- -D warnings|' "$log" &&
  pass "pass: gate commands as in CI" || fail "pass: gate commands"
check clippy-fails FAKE_NEXTEST=1 FAKE_FAIL=clippy -- "$repo" fmt clippy test
expect_code clippy-fails 1
expect_steps clippy-fails "fmt clippy nextest doc"
check nextest-fails FAKE_NEXTEST=1 FAKE_FAIL=nextest -- "$repo" fmt clippy test
expect_code nextest-fails 1
expect_steps nextest-fails "fmt clippy nextest doc"
check doc-fails FAKE_NEXTEST=1 FAKE_FAIL=doc -- "$repo" test fmt
expect_code doc-fails 1
expect_steps doc-fails "nextest doc fmt"
check no-nextest -- "$repo" clippy test
expect_code no-nextest 0
expect_steps no-nextest "clippy test"
grep -q '^test|test --locked --workspace --no-fail-fast|' "$log" &&
  pass "no-nextest: falls back to cargo test --no-fail-fast" || fail "no-nextest: fallback command"
check no-nextest-fails FAKE_FAIL=test -- "$repo" test fmt
expect_code no-nextest-fails 1
expect_steps no-nextest-fails "test fmt"
check unknown-gate -- "$repo" fmt lint
expect_code unknown-gate 2
expect_steps unknown-gate ""

# Uncommitted changes: reported, and bound by content rather than by git status.
repo=$(new_repo dirty) || exit 2
printf 'edited\n' >>"$repo/source.txt"
printf 'staged\n' >>"$repo/staged.txt" && git -C "$repo" add staged.txt
printf 'new\n' >"$repo/untracked.txt"
check dirty-unchanged -- "$repo" fmt
expect_code dirty-unchanged 0
expect_out dirty-unchanged "plus uncommitted changes (checkout fingerprint"
for edit in \
  "tracked|echo more >>source.txt" \
  "index|echo more >>staged.txt && git add staged.txt" \
  "untracked|echo more >>untracked.txt" \
  "exec-bit|chmod +x source.txt"; do
  name=dirty-${edit%%|*}-edit
  before=$(porcelain "$repo")
  check "$name" FAKE_EDIT_ON=clippy "FAKE_EDIT=${edit#*|}" -- "$repo" clippy fmt
  after=$(porcelain "$repo")
  [[ $before == "$after" ]] && pass "$name: git status is the same before and after" ||
    fail "$name: control did not keep git status equal"
  expect_code "$name" 1
  expect_steps "$name" "clippy fmt"
  expect_out "$name" "the checkout changed while checking"
done
check put-back FAKE_EDIT_ON=clippy "FAKE_EDIT=cp source.txt k && echo x >>source.txt && mv k source.txt" -- "$repo" clippy
expect_code put-back 0
pass "put-back: an edit undone before the end is not detected (documented limit)"

repo=$(new_repo clean-edits) || exit 2
check clean-edit FAKE_EDIT_ON=fmt "FAKE_EDIT=echo x >>source.txt" -- "$repo" fmt
expect_code clean-edit 1
git -C "$repo" checkout -q -- source.txt
check new-commit FAKE_EDIT_ON=fmt "FAKE_EDIT=git commit -q --allow-empty -m moved" -- "$repo" fmt
expect_code new-commit 1
check build-output-ignored FAKE_NEXTEST=1 -- "$repo" fmt test
expect_code build-output-ignored 0
[[ -f $repo/target/built-nextest ]] && pass "build-output-ignored: built into ignored target/" ||
  fail "build-output-ignored: no build file in target/"

# Git failures: before the gates nothing runs (exit 2); after them the result is withheld (exit 1).
repo=$(new_repo git-errors) || exit 2
for spec in "root|--show-toplevel|1" "head|--verify|1" "status|status|1" "index|--stage|1" \
  "paths|--others|1" "hash|--stdin-paths|1" "fingerprint|--stdin|1"; do
  IFS='|' read -r name match at <<<"$spec"
  check "git-$name-fails-before" "FAKE_GIT_MATCH=$match" "FAKE_GIT_AT=$at" -- "$repo" fmt
  expect_code "git-$name-fails-before" 2
  expect_steps "git-$name-fails-before" ""
done
for spec in "head|--verify|3" "index|--stage|2" "paths|--others|2" "hash|--stdin-paths|2" "fingerprint|--stdin|2"; do
  IFS='|' read -r name match at <<<"$spec"
  check "git-$name-fails-after" "FAKE_GIT_MATCH=$match" "FAKE_GIT_AT=$at" -- "$repo" fmt
  expect_code "git-$name-fails-after" 1
  expect_steps "git-$name-fails-after" "fmt"
  expect_out "git-$name-fails-after" "cannot fingerprint the checkout again"
done

# Checkouts the fingerprint does not describe are refused before any gate.
unborn=$scratch/unborn && mkdir -p "$unborn/scripts/ci" && cp "$script" "$unborn/scripts/ci/" &&
  printf '/target\n' >"$unborn/.gitignore" && git -C "$unborn" init -q || exit 2
check unborn -- "$unborn" fmt
expect_code unborn 2
expect_steps unborn ""
expect_out unborn "HEAD does not name a commit"
repo=$(new_repo conflict) || exit 2
blob=$(git -C "$repo" rev-parse HEAD:source.txt)
printf '100644 %s 1\tsource.txt\n100644 %s 2\tsource.txt\n' "$blob" "$blob" |
  git -C "$repo" update-index --index-info || exit 2
check conflict -- "$repo" fmt
expect_code conflict 2
expect_steps conflict ""
expect_out conflict "source.txt has a merge conflict"
repo=$(new_repo nested) || exit 2
git -C "$repo" init -q inner && printf 'x\n' >"$repo/inner/file"
check nested-repository -- "$repo" fmt
expect_code nested-repository 2
expect_steps nested-repository ""

# --fresh: validated before anything runs, created exclusively, outside the checkout.
repo=$(new_repo fresh) || exit 2
for spec in "missing|" "empty|''" "version|--version fmt" "help|--help fmt" "dash|-x fmt"; do
  IFS='|' read -r name rest <<<"$spec"
  eval "args=(--fresh $rest)"
  check "fresh-$name" -- "$repo" "${args[@]}"
  expect_code "fresh-$name" 2
  expect_steps "fresh-$name" ""
done
[[ -z $(porcelain "$repo") && ! -e $repo/--version && ! -e $repo/--help ]] &&
  pass "fresh option-like operands: nothing created" || fail "fresh option-like operands created files"
mkdir "$scratch/exists" && touch "$scratch/file" && ln -s "$scratch/nowhere" "$scratch/dangling"
for path in exists file dangling; do
  check "fresh-$path" -- "$repo" --fresh "$scratch/$path" fmt
  expect_code "fresh-$path" 2
  expect_steps "fresh-$path" ""
done
check fresh-no-parent -- "$repo" --fresh "$scratch/no/such/dir" fmt
expect_code fresh-no-parent 2
[[ ! -e $scratch/no ]] && pass "fresh-no-parent: no parents created" || fail "fresh-no-parent: created parents"
for path in .ci-review-target target/review-new; do
  check "fresh-inside-${path%%/*}" -- "$repo" --fresh "$path" fmt
  expect_code "fresh-inside-${path%%/*}" 2
  expect_steps "fresh-inside-${path%%/*}" ""
  [[ ! -e $repo/$path ]] && pass "fresh-inside-${path%%/*}: not created" || fail "fresh-inside: created $path"
done
check fresh-race FAKE_GIT_MATCH=status FAKE_GIT_AT=1 "FAKE_GIT_DO=mkdir \"$scratch/raced\"" -- \
  "$repo" --fresh "$scratch/raced" fmt
expect_code fresh-race 2
expect_steps fresh-race ""
[[ -z $(ls -A "$scratch/raced") ]] && pass "fresh-race: the directory that appeared was not used" ||
  fail "fresh-race: wrote into a directory it did not create"
check fresh-outside FAKE_NEXTEST=1 -- "$repo" --fresh ../fresh-target fmt test
expect_code fresh-outside 0
grep -q "|target=$scratch/fresh-target|" "$log" && [[ -f $scratch/fresh-target/built-nextest ]] &&
  pass "fresh-outside: every cargo call built in the new directory" || fail "fresh-outside: target dir"
[[ $(grep -c "|target=$scratch/fresh-target|" "$log") == 3 ]] || fail "fresh-outside: not every call"

# Names the fingerprint cannot read unambiguously are refused; ordinary odd names are hashed. The
# symlink, quoted-name and fresh-newline cases follow review 196's tracked-symlink-trailing-newline,
# tracked-leading-quote-file and fresh-newline-basename-run counterexamples.
repo=$(new_repo links) || exit 2
ln -s destination "$repo/link" && git -C "$repo" add link && git -C "$repo" commit -qm link || exit 2
check tracked-link-gains-newline FAKE_EDIT_ON=fmt "FAKE_EDIT=ln -sfn -- \$'destination\\n' link" -- "$repo" fmt
expect_code tracked-link-gains-newline 1
expect_steps tracked-link-gains-newline "fmt"
expect_out tracked-link-gains-newline "cannot fingerprint the checkout again"
check tracked-link-has-newline -- "$repo" fmt
expect_code tracked-link-has-newline 2
expect_steps tracked-link-has-newline ""
expect_out tracked-link-has-newline "unsupported symlink target with a newline"
ln -sfn destination "$repo/link" && ln -s destination "$repo/ulink" || exit 2
check untracked-link-gains-newline FAKE_EDIT_ON=fmt "FAKE_EDIT=ln -sfn -- \$'destination\\n' ulink" -- "$repo" fmt
expect_code untracked-link-gains-newline 1
expect_steps untracked-link-gains-newline "fmt"
expect_out untracked-link-gains-newline "cannot fingerprint the checkout again"
check untracked-link-has-newline -- "$repo" fmt
expect_code untracked-link-has-newline 2
expect_steps untracked-link-has-newline ""
ln -sfn "destination with spaces" "$repo/ulink" || exit 2
check link-retargeted FAKE_EDIT_ON=fmt "FAKE_EDIT=ln -sfn other-destination ulink" -- "$repo" fmt
expect_code link-retargeted 1

repo=$(new_repo quoted) || exit 2
printf 'alias\n' >"$repo/quoted.txt" && printf 'literal\n' >"$repo/\"quoted.txt\"" || exit 2
check untracked-quoted-name FAKE_EDIT_ON=fmt "FAKE_EDIT=echo changed >>'\"quoted.txt\"'" -- "$repo" fmt
expect_code untracked-quoted-name 2
expect_steps untracked-quoted-name ""
expect_out untracked-quoted-name "unsupported file name"
git -C "$repo" add -A && git -C "$repo" commit -qm quoted || exit 2
check tracked-quoted-name FAKE_EDIT_ON=fmt "FAKE_EDIT=echo changed >>'\"quoted.txt\"'" -- "$repo" fmt
expect_code tracked-quoted-name 2
expect_steps tracked-quoted-name ""
repo=$(new_repo carriage-return) || exit 2
printf 'alias\n' >"$repo/cr.txt" && printf 'literal\n' >"$repo/cr.txt"$'\r' || exit 2
check carriage-return-name FAKE_EDIT_ON=fmt "FAKE_EDIT=echo changed >>\$'cr.txt\\r'" -- "$repo" fmt
expect_code carriage-return-name 2
expect_steps carriage-return-name ""
expect_out carriage-return-name "unsupported file name"
repo=$(new_repo odd-names) || exit 2
odd='a "b" c\d.txt'
printf 'one\n' >"$repo/$odd" && printf 'two\n' >"$repo/sp ace.txt" && printf 'three\n' >"$repo/x\"" &&
  git -C "$repo" add -A && git -C "$repo" commit -qm odd || exit 2
check odd-names-unchanged -- "$repo" fmt
expect_code odd-names-unchanged 0
check odd-name-edited FAKE_EDIT_ON=fmt "FAKE_EDIT=echo more >>'$odd'" -- "$repo" fmt
expect_code odd-name-edited 1
expect_out odd-name-edited "the checkout changed while checking"
git -C "$repo" checkout -q -- . || exit 2
check trailing-quote-edited FAKE_EDIT_ON=fmt "FAKE_EDIT=echo more >>'x\"'" -- "$repo" fmt
expect_code trailing-quote-edited 1

# --fresh and target paths holding a newline are refused, never shortened to another directory.
repo=$(new_repo fresh-newline) || exit 2
mkdir "$scratch/p"$'\n' && ln -s "$scratch/p"$'\n' "$scratch/plink" || exit 2
for spec in "trailing|$scratch/requested"$'\n' "middle|$scratch/re"$'\n'"q" "parent|$scratch/plink/new"; do
  name=fresh-newline-${spec%%|*}
  check "$name" -- "$repo" --fresh "${spec#*|}" fmt
  expect_code "$name" 2
  expect_steps "$name" ""
done
if [[ ! -e $scratch/requested && ! -e $scratch/requested$'\n' && ! -e $scratch/re && ! -e $scratch/p/new &&
  -z $(ls -A "$scratch/p"$'\n') ]]; then
  pass "fresh-newline: no directory created under any spelling"
else
  fail "fresh-newline: created a directory"
fi
check target-newline "CARGO_TARGET_DIR=target/x"$'\n' -- "$repo" fmt
expect_code target-newline 2
expect_steps target-newline ""

# A target dir inside the checkout must be ignored.
repo=$(new_repo target-dirs) || exit 2
check target-unignored CARGO_TARGET_DIR=build -- "$repo" fmt
expect_code target-unignored 2
expect_steps target-unignored ""
check target-ignored CARGO_TARGET_DIR=target/other -- "$repo" fmt
expect_code target-ignored 0
git -C "$repo" rm -q --cached .gitignore && rm "$repo/.gitignore" && git -C "$repo" commit -qm unignore
check default-target-unignored -- "$repo" fmt
expect_code default-target-unignored 2
expect_steps default-target-unignored ""

echo
if ((failures)); then
  echo "$failures check(s) failed"
  exit 1
fi
echo "all checks passed"
