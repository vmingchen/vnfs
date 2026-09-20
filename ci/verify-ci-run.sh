#!/usr/bin/env bash
# Block a release until the CI workflow for the same commit has succeeded.
#
# The release workflows trigger on a tag push, which also triggers CI for the
# tagged commit. Polling the completed CI conclusion ensures no release is
# published unless the audit, Rust, Python, and integration jobs all passed.
#
# Usage: verify-ci-run.sh <workflow-name> <commit-sha> [timeout-seconds]
set -euo pipefail

workflow=${1:?usage: verify-ci-run.sh <workflow> <commit> [timeout-seconds]}
commit=${2:?usage: verify-ci-run.sh <workflow> <commit> [timeout-seconds]}
timeout=${3:-3600}

if [ -z "${GH_TOKEN:-}" ] && [ -z "${GITHUB_TOKEN:-}" ]; then
  echo "GH_TOKEN or GITHUB_TOKEN is required" >&2
  exit 1
fi

deadline=$(( $(date +%s) + timeout ))
while true; do
  conclusion=$(gh run list \
    --workflow "$workflow" \
    --commit "$commit" \
    --limit 30 \
    --json status,conclusion \
    --jq 'if length == 0 then "pending"
          elif any(.[]; .conclusion == "success") then "success"
          elif any(.[]; .status != "completed") then "pending"
          else "failure" end')
  case "$conclusion" in
    success)
      echo "CI workflow '$workflow' succeeded for $commit"
      exit 0
      ;;
    failure)
      echo "CI workflow '$workflow' did not succeed for $commit" >&2
      gh run list --workflow "$workflow" --commit "$commit" --limit 30 || true
      exit 1
      ;;
    pending)
      echo "Waiting for CI workflow '$workflow' on $commit..."
      ;;
    *)
      echo "unexpected CI status: $conclusion" >&2
      exit 1
      ;;
  esac
  if [ "$(date +%s)" -ge "$deadline" ]; then
    echo "timed out waiting for CI workflow '$workflow' on $commit" >&2
    exit 1
  fi
  sleep 30
done
