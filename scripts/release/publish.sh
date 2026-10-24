#!/usr/bin/env bash
# Usage: publish.sh <tag> <asset>...
# Requires GH_TOKEN and GH_REPO in the environment.
set -euo pipefail

tag=${1:?usage: publish.sh <tag> <asset>...}
shift
if [[ $tag != v* || $# -eq 0 ]]; then
  echo "usage: publish.sh v<version> <asset>..." >&2
  exit 2
fi

# SemVer: a hyphen before any +build metadata marks a prerelease, which must
# never become the release that releases/latest resolves to.
version=${tag#v}
if [[ ${version%%+*} == *-* ]]; then
  channel=(--prerelease --latest=false)
else
  channel=(--prerelease=false)
fi

if draft=$(gh release view "$tag" --json isDraft --jq .isDraft 2>/dev/null); then
  if [[ $draft != true ]]; then
    echo "error: release $tag is already published; refusing to replace its assets" >&2
    exit 1
  fi
  gh release edit "$tag" "${channel[@]}"
  gh release upload "$tag" "$@" --clobber
else
  gh release create "$tag" "$@" --draft --verify-tag --generate-notes \
    --title "loadout $tag" "${channel[@]}"
fi
