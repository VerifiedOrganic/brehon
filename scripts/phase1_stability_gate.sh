#!/usr/bin/env bash
set -euo pipefail

usage() {
  cat <<'USAGE'
Usage: scripts/phase1_stability_gate.sh [--dry-run]

Run the Phase 1 stability gate harness.

Checks:
  - runtime ownership shuts down ACP work without leaked waiters/sessions
  - permission mediation waits for explicit decisions and honors timeout policy
  - dead-worker reconciliation and shutdown drain prevent orphaned work

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
  "permission requests wait for explicit decisions|cargo test -p brehon-acp session::tests::test_permission_request_waits_for_explicit_decision -- --exact --nocapture"
  "permission timeout policy is enforced when unresolved|cargo test -p brehon-acp session::tests::test_permission_request_uses_timeout_policy_when_unresolved -- --exact --nocapture"
  "timed-out ACP waits clean pending request/prompt maps|cargo test -p brehon-acp session::tests::test_wait_for_response_timeout_cleans_up_pending_maps -- --exact --nocapture"
  "ACP kill wakes outstanding waiters to avoid orphaned requests|cargo test -p brehon-acp session::tests::test_kill_wakes_outstanding_prompt_and_request_waiters -- --exact --nocapture"
  "ACP kill awaits reader ownership shutdown|cargo test -p brehon-acp session::tests::test_kill_awaits_reader_task -- --exact --nocapture"
  "worker pool death handling clears dead-worker assignment|cargo test -p brehon-orchestrator worker_pool::tests::handle_worker_death_clears_dead_worker_assignment -- --exact --nocapture"
  "orchestrator reconciles missing-worker task ownership|cargo test -p brehon-orchestrator orchestrator::tests::tick_unassigns_tasks_owned_by_missing_workers -- --exact --nocapture"
  "review-owned dead-worker tasks are preserved for review flow|cargo test -p brehon-orchestrator orchestrator::tests::tick_respawns_dead_worker_with_review_owned_task_without_unassigning_task -- --exact --nocapture"
  "drain tracker reports immediate completion with no in-flight work|cargo test -p brehon-types drain::tests::drain_sync_returns_zero_when_no_work -- --exact --nocapture"
  "drain tracker enforces timeout policy for stuck in-flight work|cargo test -p brehon-types drain::tests::drain_sync_times_out_with_stuck_work -- --exact --nocapture"
)

required_symbols=(
  "crates/brehon-acp/src/session.rs|test_permission_request_waits_for_explicit_decision"
  "crates/brehon-acp/src/session.rs|test_permission_request_uses_timeout_policy_when_unresolved"
  "crates/brehon-acp/src/session.rs|test_wait_for_response_timeout_cleans_up_pending_maps"
  "crates/brehon-acp/src/session.rs|test_kill_wakes_outstanding_prompt_and_request_waiters"
  "crates/brehon-acp/src/session.rs|test_kill_awaits_reader_task"
  "crates/brehon-orchestrator/src/worker_pool.rs|handle_worker_death_clears_dead_worker_assignment"
  "crates/brehon-orchestrator/src/orchestrator.rs|tick_respawns_dead_worker_with_review_owned_task_without_unassigning_task"
)

verify_required_symbols() {
  local entry file symbol

  for entry in "${required_symbols[@]}"; do
    IFS='|' read -r file symbol <<<"$entry"
    if [[ ! -f "$file" ]]; then
      echo "error: required file missing: $file" >&2
      return 1
    fi
    symbol_line="$(rg -n -m1 "\bfn\s+${symbol}\b" "$file" | cut -d: -f1 || true)"
    if [[ -z "$symbol_line" ]]; then
      echo "error: required test symbol missing in $file: $symbol" >&2
      return 1
    fi

    # Check if the symbol is marked #[ignore] within the 8 lines before it.
    if (( symbol_line > 1 )); then
      context_start=$(( symbol_line > 8 ? symbol_line - 8 : 1 ))
      context_end=$(( symbol_line - 1 ))
      if sed -n "${context_start},${context_end}p" "$file" | rg -q "#\[ignore"; then
        echo "error: required test symbol is marked #[ignore] in $file: $symbol" >&2
        return 1
      fi
    fi
  done
}

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
  printf 'Phase 1 stability gate dry run (%d checks)\n' "${#checks[@]}"
  for entry in "${checks[@]}"; do
    IFS='|' read -r label command <<<"$entry"
    printf ' - %s\n   %s\n' "$label" "$command"
  done
  exit 0
fi

if ! command -v rg >/dev/null 2>&1; then
  echo "error: ripgrep (rg) is required for phase1 symbol preflight checks" >&2
  exit 1
fi

verify_required_symbols

printf 'Phase 1 stability gate: running %d checks\n' "${#checks[@]}"
for entry in "${checks[@]}"; do
  IFS='|' read -r label command <<<"$entry"
  run_check "$label" "$command"
done

printf '\nPhase 1 stability gate passed.\n'
