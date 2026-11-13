#!/usr/bin/env bash
# Required kernel-side rows of the cross-language failure matrix.
set -euo pipefail
cd "$(dirname -- "${BASH_SOURCE[0]}")/.."
log=$(mktemp)
trap 'rm -f "$log"' EXIT
for name in \
  failure_after_publication_retains_inflight_ownership_without_replay \
  fatal_write_submission_keeps_confirmed_prefix_and_untouched_suffix_distinct \
  sync_publication_loss_keeps_prior_success_and_unsubmitted_suffix
do
  cargo test -p vfsi-uring --lib "engine::tests::$name" -- --exact --nocapture 2>&1 | tee "$log"
  grep -Eq '^test result: ok\. 1 passed; 0 failed; 0 ignored;' "$log" || {
    echo "Required kernel outcome case did not run: $name" >&2
    exit 1
  }
done
