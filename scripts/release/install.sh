#!/bin/sh
# Install the loadout CLI on macOS or Linux from a GitHub release, then optionally run `loadout init`.
#
#   curl -fsSL https://raw.githubusercontent.com/dan-myles/loadout/main/scripts/release/install.sh | sh
#   curl -fsSL .../install.sh | sh -s -- https://github.com/you/your-loadout.git --name laptop
#
# Run with --help for the options. docs/releasing.md describes every step and the install record.
set -eu

REPO=dan-myles/loadout
RECORD_KEYS='installer repository source target version archive_sha256 binary binary_sha256'
version=latest
base_url=
tmp=
staged=
staged_record=

say() { printf 'loadout-install: %s\n' "$*" >&2; }
die() { say "error: $*"; exit 1; }
usage() {
  cat <<'EOF'
usage: install.sh [--version vX.Y.Z] [--base-url URL] [LOADOUT_URL [INIT_ARGS...]]

Installs loadout to ~/.local/bin after verifying the release archive's sha256. Needs curl and git.
  --version vX.Y.Z   install that release instead of the latest one
  --base-url URL     fetch the assets from URL instead of GitHub: https, or http on localhost
Given LOADOUT_URL, runs `loadout init LOADOUT_URL INIT_ARGS...` afterwards.
An existing ~/.local/bin/loadout is replaced only if this installer put it there.
EOF
}
usage_error() { say "error: $*"; usage >&2; exit 2; }
cleanup() {
  if [ -n "$staged" ]; then rm -f "$staged"; fi
  if [ -n "$staged_record" ]; then rm -f "$staged_record"; fi
  if [ -n "$tmp" ]; then rm -rf "$tmp"; fi
  staged= staged_record= tmp=
}
trap cleanup EXIT
trap 'exit 1' HUP INT TERM

while [ $# -gt 0 ]; do
  case $1 in
    --version)
      [ $# -ge 2 ] || usage_error "--version needs a value"
      version=$2
      shift 2
      ;;
    --base-url)
      [ $# -ge 2 ] || usage_error "--base-url needs a value"
      base_url=$2
      shift 2
      ;;
    -h | --help)
      usage
      exit 0
      ;;
    --)
      shift
      break
      ;;
    -*) usage_error "unknown option $1" ;;
    *) break ;;
  esac
done

loadout_url=
if [ $# -gt 0 ]; then
  loadout_url=$1
  shift
  case $loadout_url in
    '' | -*) usage_error "the loadout URL must not be empty or start with -" ;;
  esac
fi

case $version in
  latest) ;;
  v[0-9]*.[0-9]*.[0-9]*)
    case $version in *[!A-Za-z0-9.+-]*) usage_error "--version $version has characters a release tag can't" ;; esac
    ;;
  *) usage_error "--version must be a release tag like v1.2.3" ;;
esac

# Every hop is https, except an explicit loopback --base-url, which gets plain http and no redirects.
proto='=https'
max_redirs=5
if [ -n "$base_url" ]; then
  case $base_url in
    *[[:space:]]* | *[[:cntrl:]]* | *'?'* | *'#'* | *'@'* | *'\'* | *'"'* | *"'"*)
      usage_error "--base-url must be a plain URL: no spaces, quotes, credentials, query, or fragment"
      ;;
  esac
  case $base_url in
    https://*) authority=${base_url#https://} ;;
    http://*)
      authority=${base_url#http://}
      authority=${authority%%/*}
      case $authority in
        127.0.0.1 | localhost | '[::1]') ;;
        *:*)
          port=${authority##*:}
          case ${authority%:*} in
            127.0.0.1 | localhost | '[::1]') ;;
            *) usage_error "--base-url may use http only for 127.0.0.1, localhost, or [::1]" ;;
          esac
          case $port in '' | *[!0-9]*) usage_error "--base-url has an invalid port" ;; esac
          ;;
        *) usage_error "--base-url may use http only for 127.0.0.1, localhost, or [::1]" ;;
      esac
      proto='=http'
      max_redirs=0
      ;;
    *) usage_error "--base-url must use https, or http on localhost" ;;
  esac
  case ${authority%%/*} in '') usage_error "--base-url has no host" ;; esac
  base_url=${base_url%/}
fi

os=$(uname -s)
arch=$(uname -m)
case $os in
  Linux) os_part=unknown-linux-musl ;;
  Darwin) os_part=apple-darwin ;;
  *) die "unsupported operating system $os: releases exist for macOS and Linux (and Windows, through install.ps1)" ;;
esac
case $arch in
  x86_64 | amd64) arch=x86_64 ;;
  aarch64 | arm64) arch=aarch64 ;;
  *) die "unsupported CPU architecture $arch: releases exist for x86_64 and aarch64" ;;
esac
# A shell running under Rosetta on Apple silicon reports x86_64; install the native build instead.
if [ "$os" = Darwin ] && [ "$arch" = x86_64 ] && [ "$(sysctl -n sysctl.proc_translated 2>/dev/null || true)" = 1 ]; then
  arch=aarch64
fi
target=$arch-$os_part

if ! command -v git >/dev/null 2>&1; then
  case $os in
    Darwin) hint="xcode-select --install (or brew install git)" ;;
    *) hint="your distribution's package manager, for example sudo apt install git, sudo dnf install git, or sudo pacman -S git" ;;
  esac
  die "git is required but is not on PATH. Install it with $hint, then run this installer again"
fi
# wget can't be restricted to https redirects, so it is not a fallback.
command -v curl >/dev/null 2>&1 || die "curl is required to download loadout over https only. Install curl, then run this installer again"
command -v tar >/dev/null 2>&1 || die "tar is required to unpack loadout"
command -v cmp >/dev/null 2>&1 || die "cmp is required to verify the checksum file"
if command -v sha256sum >/dev/null 2>&1; then
  sha_cmd=sha256sum
elif command -v shasum >/dev/null 2>&1; then
  sha_cmd="shasum -a 256"
else
  die "sha256sum or shasum is required to verify the download"
fi

# Prints the file's sha256, or says why not and returns 1; every caller checks that status, since a
# failure inside a command substitution or a condition wouldn't stop the script by itself.
# The file goes in on stdin, not as an argument: for a name holding a backslash or newline,
# sha256sum and shasum escape the name and prefix the digest with a backslash, which would then be
# compared and written to the install record as part of the hash. The tool's output and exit
# status are captured together, so a failing tool or an unreadable file is never taken for a hash.
sha256_of() {
  if ! hash_out=$($sha_cmd <"$1"); then
    say "error: could not hash $1: $sha_cmd failed"
    return 1
  fi
  hash_digest=${hash_out%% *}
  case $hash_digest in
    '' | *[!0-9a-f]*)
      say "error: could not hash $1: $sha_cmd printed '$hash_out'"
      return 1
      ;;
  esac
  if [ ${#hash_digest} -ne 64 ]; then
    say "error: could not hash $1: $sha_cmd printed '$hash_out'"
    return 1
  fi
  printf '%s' "$hash_digest"
}

download() {
  curl -q --fail --silent --show-error --location --max-redirs "$max_redirs" \
    --proto "$proto" --proto-redir '=https' --tlsv1.2 \
    --retry 3 --connect-timeout 20 --output "$2" "$1" || die "download failed: $1"
}

# The install record is TOML. Values holding a control character or invalid UTF-8 are refused
# before anything changes, so only backslash and double quote ever need escaping.
check_text() {
  case $1 in *[[:cntrl:]]*) die "$2 contains a control character, which the install record can't hold" ;; esac
  if printf '%s' "$1" | LC_ALL=C grep -q '[^ -~]'; then
    command -v iconv >/dev/null 2>&1 || die "$2 is not plain ASCII and iconv isn't available to check it is UTF-8"
    printf '%s' "$1" | iconv -f UTF-8 -t UTF-8 >/dev/null 2>&1 || die "$2 is not valid UTF-8"
  fi
}

toml_string() {
  printf '"%s"' "$(printf '%s' "$1" | LC_ALL=C sed -e 's/\\/\\\\/g' -e 's/"/\\"/g')"
}

record_line() {
  LC_ALL=C sed -n "s/^$1 = //p" "$record"
}

# Data only. The whole file must be UTF-8 without NUL or other control bytes and end in LF; each line
# is the header comment, exactly `schema = 1`, or one known key with a basic string.
record_matches() {
  [ -f "$record" ] && [ ! -L "$record" ] || { why="there is no install record $record"; return 1; }
  why="$record is not a valid install record"
  [ -s "$record" ] || return 1
  [ "$(($(tr -d '\000' <"$record" | wc -c)))" -eq "$(($(wc -c <"$record")))" ] || return 1
  [ "$(($(tail -c 1 "$record" | wc -l)))" -eq 1 ] || return 1
  if LC_ALL=C grep -q '[^ -~]' "$record"; then
    command -v iconv >/dev/null 2>&1 || { why="$record is not plain ASCII and iconv isn't available to check it"; return 1; }
    iconv -f UTF-8 -t UTF-8 <"$record" >/dev/null 2>&1 || return 1
  fi
  LC_ALL=C grep -q '[[:cntrl:]]' "$record" && return 1
  LC_ALL=C grep -Evq "^(#.*|schema = 1|($(printf '%s' "$RECORD_KEYS" | tr ' ' '|')) = \"([^\"\\\\]|\\\\[\"\\\\])*\")\$" "$record" && return 1
  for key in schema $RECORD_KEYS; do
    [ "$(LC_ALL=C grep -c "^$key = " "$record")" = 1 ] || { why="$record does not have exactly one $key"; return 1; }
  done
  [ "$(record_line installer)" = '"install.sh"' ] || { why="$record was written by another installer"; return 1; }
  [ "$(record_line target)" = "\"$target\"" ] || { why="$record is for another target than $target"; return 1; }
  [ "$(record_line binary)" = "$(toml_string "$dest")" ] || { why="$record describes another binary path"; return 1; }
  # A file that can't be hashed is an error, never an unmanaged or a matching binary.
  binary_sha=$(sha256_of "$dest") || exit 1
  [ "$(record_line binary_sha256)" = "\"$binary_sha\"" ] || { why="$dest changed after it was installed"; return 1; }
}

# Sets dest and record from the canonical install directory and refuses anything this installer
# didn't put there: links, folders, and an existing binary without a matching record.
check_destination() {
  canon_dir=$(CDPATH= cd -P -- "$1" && pwd -P) || die "could not resolve $1"
  check_text "$canon_dir" "the install directory $canon_dir"
  dest=$canon_dir/loadout
  record=$canon_dir/loadout.install.toml
  for path in "$dest" "$record"; do
    if [ -L "$path" ] || { [ -e "$path" ] && [ ! -f "$path" ]; }; then
      die "$path is a symlink or not a regular file, so another installer may own it. It was left alone"
    fi
  done
  if [ -e "$dest" ] && ! record_matches; then
    die "$dest was not installed by this installer ($why), so it was left alone. To install here, remove $dest and $record yourself first"
  fi
}

# install.sh, install.ps1 and loadout self-update each hold this lock from their last check of the
# installed files until the record is in place, so none overwrites what another just installed. It is
# flock(2) on the file, through flock(1) or else perl, held on fd 9; the file itself stays.
lock_install() {
  lock=$1/.loadout.install.lock
  if [ -L "$lock" ] || { [ -e "$lock" ] && [ ! -f "$lock" ]; }; then
    die "$lock is a symlink or not a regular file, so this install can't be locked. It was left alone"
  fi
  true >>"$lock" 2>/dev/null || die "could not create $lock"
  exec 9>>"$lock"
  locked=0
  if command -v flock >/dev/null 2>&1; then
    flock -n 9 || locked=$?
  elif command -v perl >/dev/null 2>&1; then
    perl -MFcntl=:flock -e 'open(my $fh, ">&=", 9) or exit 2; flock($fh, LOCK_EX | LOCK_NB) or exit 1' || locked=$?
  else
    die "flock or perl is required to lock $1 against another install running at the same time"
  fi
  case $locked in
    0) ;;
    1) die "another install or loadout self-update is changing $1 right now, so nothing was replaced. Try again when it is done" ;;
    *) die "could not lock $lock" ;;
  esac
}

# The loadout URL as loadout itself shows it: the userinfo of a scheme:// URL, which can be a token,
# becomes ***. An scp-like user@host:path keeps its SSH user name.
shown_url() {
  case $1 in
    *://*)
      scheme=${1%%://*}
      rest=${1#*://}
      authority=${rest%%/*}
      case $authority in
        *@*) printf '%s://***@%s%s' "$scheme" "${authority##*@}" "${rest#"$authority"}" ;;
        *) printf '%s' "$1" ;;
      esac
      ;;
    *) printf '%s' "$1" ;;
  esac
}

# The sha256 of the binary and of the record, or `absent`, as one line; returns 1 if either can't
# be hashed.
install_state() {
  for path in "$dest" "$record"; do
    if [ -e "$path" ]; then
      state_hash=$(sha256_of "$path") || return 1
      printf '%s ' "$state_hash"
    else
      printf 'absent '
    fi
  done
}

# Exactly one regular loadout-<target>/loadout, and no entry outside loadout-<target>/ or nested in it.
check_listing() {
  binaries=0
  while IFS= read -r name; do
    case $name in
      "loadout-$target" | "loadout-$target/") ;;
      "loadout-$target/"*)
        leaf=${name#"loadout-$target/"}
        case $leaf in '' | . | .. | */* | *[!A-Za-z0-9._-]*) return 1 ;; esac
        if [ "$leaf" = loadout ]; then binaries=$((binaries + 1)); fi
        ;;
      *) return 1 ;;
    esac
  done
  [ "$binaries" -eq 1 ]
}

[ -n "${HOME:-}" ] || die "HOME is not set"
case $HOME in /*) ;; *) die "HOME must be an absolute path" ;; esac
check_text "$HOME" "HOME ($HOME)"
bin_dir=$HOME/.local/bin
seen='absent absent '
if [ -d "$bin_dir" ]; then
  check_destination "$bin_dir"
  seen=$(install_state) || exit 1
fi

asset=loadout-$target.tar.gz
if [ -n "$base_url" ]; then
  url=$base_url/$asset
elif [ "$version" = latest ]; then
  url=https://github.com/$REPO/releases/latest/download/$asset
else
  url=https://github.com/$REPO/releases/download/$version/$asset
fi
check_text "$url" "the download URL"

tmp=$(mktemp -d) || die "could not create a temporary directory"
say "downloading $url"
download "$url" "$tmp/$asset"
download "$url.sha256" "$tmp/$asset.sha256"

# The sidecar must be byte-for-byte `<64 lowercase hex>  <asset>` and one LF, nothing else.
want=$(LC_ALL=C sed -n '1s/^\([0-9a-f]\{64\}\)  .*/\1/p' "$tmp/$asset.sha256")
[ ${#want} -eq 64 ] || die "$asset.sha256 is not exactly one '<sha256>  $asset' line"
printf '%s  %s\n' "$want" "$asset" >"$tmp/expected.sha256"
cmp -s "$tmp/expected.sha256" "$tmp/$asset.sha256" ||
  die "$asset.sha256 is not exactly one '<sha256>  $asset' line"
got=$(sha256_of "$tmp/$asset") || exit 1
[ "$got" = "$want" ] || die "checksum mismatch for $asset: expected $want, got $got"
say "verified sha256 $got"

tar -tzf "$tmp/$asset" >"$tmp/listing" || die "could not read $asset"
check_listing <"$tmp/listing" ||
  die "$asset must hold exactly one loadout-$target/loadout and only plain names inside loadout-$target/"
# GNU and BSD tar both start each verbose line with the ls-style type: '-' file, 'd' directory,
# 'h' hard link, 'l' symlink. Only files and directories are expected.
tar -tvzf "$tmp/$asset" >"$tmp/types" || die "could not read $asset"
[ "$(($(wc -l <"$tmp/types")))" -eq "$(($(wc -l <"$tmp/listing")))" ] && ! LC_ALL=C grep -vq '^[-d]' "$tmp/types" ||
  die "$asset may contain only regular files and directories"
mkdir "$tmp/unpacked"
tar -xzf "$tmp/$asset" -C "$tmp/unpacked" "loadout-$target/loadout" ||
  die "could not extract loadout-$target/loadout from $asset"
new=$tmp/unpacked/loadout-$target/loadout
[ -f "$new" ] && [ ! -L "$new" ] || die "loadout-$target/loadout in $asset is not a regular file"
chmod 755 "$new"

reported=$("$new" --version 2>/dev/null) || die "the downloaded loadout does not run on this machine ($target)"
case $reported in
  "loadout "*) ;;
  *) die "the downloaded binary printed an unexpected version: $reported" ;;
esac
installed_version=${reported#loadout }
installed_version=${installed_version%% *}
case $installed_version in '' | *[!A-Za-z0-9.+-]*) die "the downloaded binary printed an unexpected version: $reported" ;; esac
if [ "$version" != latest ] && [ "v$installed_version" != "$version" ]; then
  die "release $version contains loadout $installed_version"
fi

mkdir -p "$bin_dir" || die "could not create $bin_dir"
check_destination "$bin_dir"
# Another install may have finished while this one downloaded, so check again under the lock.
lock_install "$canon_dir"
check_destination "$bin_dir"
now=$(install_state) || exit 1
[ "$now" = "$seen" ] ||
  die "$dest or $record changed while this install was downloading, so nothing was replaced. Run the installer again"

staged=$(mktemp "$canon_dir/.loadout.new.XXXXXX") || die "could not create a staging file in $canon_dir"
cat "$new" >"$staged" && chmod 755 "$staged" || die "could not write $staged"
staged_sha=$(sha256_of "$staged") || exit 1
staged_record=$(mktemp "$canon_dir/.loadout.install.toml.new.XXXXXX") ||
  die "could not create a staging file in $canon_dir"
{
  printf '# loadout install record. Written by install.sh or install.ps1 and read by loadout self-update. Do not edit.\n'
  printf 'schema = 1\n'
  printf 'installer = "install.sh"\n'
  printf 'repository = "%s"\n' "$REPO"
  printf 'source = %s\n' "$(toml_string "$url")"
  printf 'target = "%s"\n' "$target"
  printf 'version = "%s"\n' "$installed_version"
  printf 'archive_sha256 = "%s"\n' "$want"
  printf 'binary = %s\n' "$(toml_string "$dest")"
  printf 'binary_sha256 = "%s"\n' "$staged_sha"
} >"$staged_record" && chmod 644 "$staged_record" || die "could not write $staged_record"

mv -f "$staged" "$dest" || die "could not replace $dest; the previous binary is unchanged"
staged=
mv -f "$staged_record" "$record" ||
  die "installed $dest but could not write $record, so self-update will not replace this binary"
staged_record=
exec 9>&-
say "installed loadout $installed_version ($target) to $dest"

case ":${PATH:-}:" in
  *":$bin_dir:"* | *":$canon_dir:"*) ;;
  *) say "$bin_dir is not on PATH. Add it in your shell profile, for example: export PATH=\"\$HOME/.local/bin:\$PATH\"" ;;
esac

if [ -n "$loadout_url" ]; then
  cleanup
  trap - EXIT
  # The init arguments aren't shown: they are the user's own, and could hold anything.
  say "running $dest init $(shown_url "$loadout_url")${1+ and the init arguments you gave}"
  exec "$dest" init "$loadout_url" "$@"
fi
