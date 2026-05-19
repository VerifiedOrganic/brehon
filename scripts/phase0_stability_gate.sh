#!/usr/bin/env bash
set -euo pipefail

usage() {
  cat <<'USAGE'
Usage: scripts/phase0_stability_gate.sh [--dry-run]

Run the Phase 0 stability gate harness.

Checks:
  - durable queue activation remains claimable and single-winner under contention
  - crash-window recovery preserves unique EventIds across restart
  - baseline stability counters are surfaced through brehon doctor

Options:
  --dry-run   Print the commands without executing them
  -h, --help  Show this help text
USAGE
}

repo_root="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$repo_root"

dry_run=0
case "${1:-}" in
  "") ;;
  --dry-run) dry_run=1 ;;
  -h|--help)
    usage
    exit 0
    ;;
  *)
    echo "Unknown argument: $1" >&2
    usage >&2
    exit 1
    ;;
esac

checks=(
  "queue activation stays claimable|cargo test -p brehon-store-fjall store::tests::test_concurrent_review_enqueues_are_unique_and_claimable -- --exact --nocapture"
  "single queue item keeps one winner under contention|cargo test -p brehon-store-fjall store::tests::test_single_review_item_has_single_claim_winner_under_contention -- --exact --nocapture"
  "crash-window durable prefix survives recovery|cargo test -p brehon-store-fjall store::tests::crash_window_recovery_preserves_only_precrash_durable_prefix -- --exact --nocapture"
  "crash-window recovery keeps EventIds non-reused|cargo test -p brehon-store-fjall store::tests::crash_window_no_eventid_reuse_after_recovery -- --exact --nocapture"
  "crash-window recovery keeps EventIds globally unique|cargo test -p brehon-store-fjall store::tests::crash_window_all_eventids_globally_unique -- --exact --nocapture"
  "crash-window recovery preserves consistent ids|cargo test -p brehon-store-fjall store::tests::crash_window_surviving_events_have_consistent_ids -- --exact --nocapture"
  "doctor surfaces baseline stability counters|cargo test -p brehon-doctor checkers::runtime::tests::test_stability_bounds_reads_live_runtime_snapshot -- --exact --nocapture"
)

run_check() {
  local label="$1"
  local command="$2"
  local output
  local passed_total

  printf '\n==> %s\n' "$label"

  if ! output=$(bash -o pipefail -c "$command" 2>&1); then
    printf '%s\n' "$output"
    return 1
  fi

  printf '%s\n' "$output"

  # Parses cargo's "test result: ok. <N> passed; ..." output format.
  passed_total=$(
    printf '%s\n' "$output" \
      | sed -n 's/.*test result: ok\. \([0-9][0-9]*\) passed;.*/\1/p' \
      | awk '{sum += $1} END {print sum + 0}'
  )

  if [[ "$passed_total" -eq 0 ]]; then
    echo "error: check '$label' matched zero tests" >&2
    return 1
  fi
}

if [[ "$dry_run" -eq 1 ]]; then
  printf 'Phase 0 stability gate dry run (%d checks)\n' "${#checks[@]}"
  for entry in "${checks[@]}"; do
    IFS='|' read -r label command <<<"$entry"
    printf ' - %s\n   %s\n' "$label" "$command"
  done
  exit 0
fi

printf 'Phase 0 stability gate: running %d checks\n' "${#checks[@]}"
for entry in "${checks[@]}"; do
  IFS='|' read -r label command <<<"$entry"
  run_check "$label" "$command"
done

printf '\nPhase 0 stability gate passed.\n'
