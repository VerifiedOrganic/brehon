#!/usr/bin/env bash
set -euo pipefail

usage() {
  cat <<'USAGE'
Usage: scripts/phase5_stability_gate.sh [--dry-run]

Run the Phase 5 stability gate harness.

This gate encodes STABILITY_REVIEW §10 unattended-stability exit criteria
as repeatable automated checks.

Checks:
  - Crash/restart and queue ownership invariants remain enforced (Phase 0)
  - Permission mediation and runtime cleanup remain enforced (Phase 1)
  - Git recovery invariants hold for interrupted operations and stale locks
  - Doctor integrity diagnostics remain enforced for queue/view/index drift
  - Lease recovery remains correct across restart with stale wall-clock data
  - Startup recovery can rebuild task/review views from authoritative events
  - Retention sweeps retry promptly after transient failures
  - Soak and chaos suites exercise unattended boundedness and failure handling

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

# Format: "label|mode|command"
# mode=cargo enforces the non-vacuous "at least one test passed" guard.
# mode=script is used when delegating to prior phase harness scripts.
checks=(
  "phase0 crash/queue baseline gate|script|./scripts/phase0_stability_gate.sh"
  "phase1 permission/runtime ownership gate|script|./scripts/phase1_stability_gate.sh"
  "crash recovery detects partial commit tearing|cargo|cargo test -p brehon-test-harness --test crash_tests crash_append_atomic::crash_tearing_detects_partial_commit -- --exact --nocapture"
  "crash recovery no eventid reuse after recovery|cargo|cargo test -p brehon-test-harness --test crash_tests crash_append_atomic::crash_no_eventid_reuse_after_recovery -- --exact --nocapture"
  "crash recovery seq counter must not reuse ids|cargo|cargo test -p brehon-test-harness --test crash_tests crash_append_atomic::crash_seq_counter_must_not_reuse_ids -- --exact --nocapture"
  "crash recovery cleans interrupted rebase state|cargo|cargo test -p brehon-test-harness --test crash_tests crash_during_rebase::crash_during_rebase_cleanup_restores_clean_state -- --exact --nocapture"
  "crash recovery cleans interrupted merge state|cargo|cargo test -p brehon-test-harness --test crash_tests crash_mid_merge::crash_mid_merge_recovery_restores_clean_state -- --exact --nocapture"
  "git recovery removes safe stale lockfiles|cargo|cargo test -p brehon-git recovery::tests::recover_removes_safe_stale_lockfiles -- --exact --nocapture"
  "git recovery reports dirty worktree cleanup requirements|cargo|cargo test -p brehon-git recovery::tests::recover_dirty_worktree_reports_files -- --exact --nocapture"
  "doctor detects orphaned queue leases|cargo|cargo test -p brehon-doctor checkers::store_search::store_search_tests::test_queue_lease_detects_orphaned_claim -- --exact --nocapture"
  "doctor detects task-view drift|cargo|cargo test -p brehon-doctor checkers::store_search::store_search_tests::test_view_drift_flags_missing_task_view -- --exact --nocapture"
  "doctor detects review-view drift|cargo|cargo test -p brehon-doctor checkers::store_search::store_search_tests::test_review_view_drift_flags_missing_review_view -- --exact --nocapture"
  "doctor detects Tantivy/Fjall drift|cargo|cargo test -p brehon-doctor checkers::store_search::store_search_tests::test_tantivy_fjall_drift_detects_extra_index_entries -- --exact --nocapture"
  "lease recovery survives restart with stale wall-clock expiry|cargo|cargo test -p brehon-store-fjall store::tests::startup_recovery_preserves_same_epoch_monotonic_claims_with_stale_wall_clock_expiry -- --exact --nocapture"
  "startup recovery rebuilds task/review views from durable log|cargo|cargo test -p brehon-store-fjall store::tests::startup_rebuilds_task_and_review_views_from_events -- --exact --nocapture"
  "retention sweep retries immediately after failed pass|cargo|cargo test -p brehon-orchestrator orchestrator::tests::retention_sweep_retries_immediately_after_failed_sweep -- --exact --nocapture"
  "soak queue claim ack cycles bounded|cargo|cargo test -p brehon-test-harness --test soak_tests queue_boundedness::soak_queue_claim_ack_cycles_bounded -- --exact --nocapture"
  "soak crash recovery no sequence reuse|cargo|cargo test -p brehon-test-harness --test soak_tests crash_recovery_cycles::soak_crash_recovery_no_sequence_reuse -- --exact --nocapture"
  "soak git conflict recovery clean state|cargo|cargo test -p brehon-test-harness --test soak_tests git_stability::soak_git_conflict_recovery_clean_state -- --exact --nocapture"
  "soak mcp panic boundary holds|cargo|cargo test -p brehon-test-harness --test soak_tests mcp_stability::soak_mcp_panic_boundary_holds -- --exact --nocapture"
  "soak pty spawn kill cycles no leak|cargo|cargo test -p brehon-test-harness --test soak_tests pty_stability::unix_tests::soak_pty_spawn_kill_cycles_no_leak -- --exact --nocapture"
  "chaos combined full chaos stability|cargo|cargo test -p brehon-test-harness --test chaos_tests chaos_combined::chaos_combined_full_chaos_stability -- --exact --nocapture"
  "chaos lease expiry recovery|cargo|cargo test -p brehon-test-harness --test chaos_tests chaos_leases::chaos_lease_expiry_recovery -- --exact --nocapture"
  "chaos mcp panic under load|cargo|cargo test -p brehon-test-harness --test chaos_tests chaos_mcp::chaos_mcp_panic_under_load -- --exact --nocapture"
  "chaos pty rapid spawn kill with delays|cargo|cargo test -p brehon-test-harness --test chaos_tests chaos_pty::unix_tests::chaos_pty_rapid_spawn_kill_with_delays -- --exact --nocapture"
)

required_scripts=(
  "scripts/phase0_stability_gate.sh"
  "scripts/phase1_stability_gate.sh"
)

required_symbols=(
  "crates/brehon-store-fjall/src/store.rs|startup_recovery_preserves_same_epoch_monotonic_claims_with_stale_wall_clock_expiry"
  "crates/brehon-store-fjall/src/store.rs|startup_rebuilds_task_and_review_views_from_events"
  "crates/brehon-orchestrator/src/orchestrator.rs|retention_sweep_retries_immediately_after_failed_sweep"
  "crates/brehon-test-harness/tests/crash/crash_append_atomic.rs|crash_tearing_detects_partial_commit"
  "crates/brehon-test-harness/tests/crash/crash_append_atomic.rs|crash_no_eventid_reuse_after_recovery"
  "crates/brehon-test-harness/tests/crash/crash_append_atomic.rs|crash_seq_counter_must_not_reuse_ids"
  "crates/brehon-test-harness/tests/crash/crash_during_rebase.rs|crash_during_rebase_cleanup_restores_clean_state"
  "crates/brehon-test-harness/tests/crash/crash_mid_merge.rs|crash_mid_merge_recovery_restores_clean_state"
  "crates/brehon-git/src/recovery.rs|recover_removes_safe_stale_lockfiles"
  "crates/brehon-git/src/recovery.rs|recover_dirty_worktree_reports_files"
  "crates/brehon-doctor/src/checkers/store_search_tests.rs|test_queue_lease_detects_orphaned_claim"
  "crates/brehon-doctor/src/checkers/store_search_tests.rs|test_view_drift_flags_missing_task_view"
  "crates/brehon-doctor/src/checkers/store_search_tests.rs|test_review_view_drift_flags_missing_review_view"
  "crates/brehon-doctor/src/checkers/store_search_tests.rs|test_tantivy_fjall_drift_detects_extra_index_entries"
  "crates/brehon-test-harness/tests/soak/queue_boundedness.rs|soak_queue_claim_ack_cycles_bounded"
  "crates/brehon-test-harness/tests/soak/crash_recovery_cycles.rs|soak_crash_recovery_no_sequence_reuse"
  "crates/brehon-test-harness/tests/soak/git_stability.rs|soak_git_conflict_recovery_clean_state"
  "crates/brehon-test-harness/tests/soak/mcp_stability.rs|soak_mcp_panic_boundary_holds"
  "crates/brehon-test-harness/tests/soak/pty_stability.rs|soak_pty_spawn_kill_cycles_no_leak"
  "crates/brehon-test-harness/tests/chaos/chaos_combined.rs|chaos_combined_full_chaos_stability"
  "crates/brehon-test-harness/tests/chaos/chaos_leases.rs|chaos_lease_expiry_recovery"
  "crates/brehon-test-harness/tests/chaos/chaos_mcp.rs|chaos_mcp_panic_under_load"
  "crates/brehon-test-harness/tests/chaos/chaos_pty.rs|chaos_pty_rapid_spawn_kill_with_delays"
)

verify_required_scripts() {
  local script
  for script in "${required_scripts[@]}"; do
    if [[ ! -f "$script" ]]; then
      echo "error: required gate script missing: $script" >&2
      return 1
    fi
    if [[ ! -x "$script" ]]; then
      echo "error: required gate script is not executable: $script" >&2
      return 1
    fi
  done
}

verify_required_symbols() {
  local entry file symbol symbol_line context_start context_end

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
  local mode="$2"
  local command="$3"
  local output_file
  local passed_total

  output_file="$(mktemp)"

  printf '\n==> %s\n' "$label"

  if ! bash -o pipefail -c "$command" 2>&1 | tee "$output_file"; then
    rm -f "$output_file"
    return 1
  fi

  if [[ "$mode" == "cargo" ]]; then
    # Parses cargo's "test result: ok. <N> passed; ..." output format.
    passed_total=$(
      sed -n 's/.*test result: ok\. \([0-9][0-9]*\) passed;.*/\1/p' "$output_file" \
        | awk '{sum += $1} END {print sum + 0}'
    )

    if [[ "$passed_total" -eq 0 ]]; then
      echo "error: check '$label' matched zero tests" >&2
      rm -f "$output_file"
      return 1
    fi
  fi

  rm -f "$output_file"
}

if [[ "$dry_run" -eq 1 ]]; then
  printf 'Phase 5 stability gate dry run (%d checks)\n' "${#checks[@]}"
  for entry in "${checks[@]}"; do
    IFS='|' read -r label mode command <<<"$entry"
    printf ' - %s [%s]\n   %s\n' "$label" "$mode" "$command"
  done
  exit 0
fi

if ! command -v rg >/dev/null 2>&1; then
  echo "error: ripgrep (rg) is required for phase5 symbol preflight checks" >&2
  exit 1
fi

verify_required_scripts
verify_required_symbols

printf 'Phase 5 stability gate: running %d checks\n' "${#checks[@]}"
for entry in "${checks[@]}"; do
  IFS='|' read -r label mode command <<<"$entry"
  run_check "$label" "$mode" "$command"
done

printf '\nPhase 5 stability gate passed.\n'
