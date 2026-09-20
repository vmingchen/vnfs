#!/usr/bin/env bash
# Create the GitHub release for the current tag (if needed) and attach assets
# such as the generated SBOMs. No-op outside a tag release.
#
# Usage: attach-release-assets.sh <asset> [asset...]
set -euo pipefail

if [ "${GITHUB_REF_TYPE:-}" != "tag" ]; then
  echo "not a tag ref; skipping GitHub release assets"
  exit 0
fi

if [ "$#" -eq 0 ]; then
  echo "no assets provided" >&2
  exit 1
fi

tag=${GITHUB_REF_NAME:?GITHUB_REF_NAME is required}
if ! gh release view "$tag" >/dev/null 2>&1; then
  gh release create "$tag" --verify-tag --title "$tag" --generate-notes
fi
gh release upload "$tag" --clobber "$@"
