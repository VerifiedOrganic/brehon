#!/usr/bin/env bash
set -euo pipefail

usage() {
  cat <<'USAGE'
Usage: scripts/phase3_stability_gate.sh [--dry-run]

Run the Phase 3 stability gate harness.

Checks:
  - MCP shared-context tools use real durable/task runtime state
  - MCP calls are resilient to panics/oversized payloads with caller attribution
  - Doctor reports queue lease, view drift, and Tantivy/Fjall consistency issues
  - Tantivy reindex rebuilds from authoritative entries
  - Config validation uses typed fatal warnings for required structure

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
  "memory tools persist/search/delete real shared state|cargo test -p brehon-mcp tools::memory::memory_tests::test_create_list_search_delete_memory_round_trip -- --exact --nocapture"
  "memory tools wire attached search index and event store|cargo test -p brehon-mcp tools::memory::memory_tests::test_memory_tools_use_attached_search_index_and_event_store -- --exact --nocapture"
  "task context tool reads runtime task state|cargo test -p brehon-mcp tools::tasks::tasks_tests::test_get_task_context_tool -- --exact --nocapture"
  "task context tool rejects invalid IDs instead of path traversal|cargo test -p brehon-mcp tools::tasks::tasks_tests::test_get_task_context_rejects_invalid_task_id -- --exact --nocapture"
  "tool panics are contained with internal error attribution|cargo test -p brehon-mcp server::server_tests::tool_panic_catches_panics_and_returns_internal_error -- --exact --nocapture"
  "oversized MCP requests are rejected with caller attribution|cargo test -p brehon-mcp server::server_tests::input_size_rejects_oversized_arguments_with_caller_attribution -- --exact --nocapture"
  "MCP request-size/caller config is cached at server init|cargo test -p brehon-mcp server::server_tests::input_size_uses_cached_env_config_from_server_init -- --exact --nocapture"
  "bounded transport recovers after oversized frames|cargo test -p brehon-mcp server::server_tests::input_size_bounded_transport_recovers_after_oversized_frame -- --exact --nocapture"
  "doctor detects orphaned queue-lease claims|cargo test -p brehon-doctor checkers::store_search::store_search_tests::test_queue_lease_detects_orphaned_claim -- --exact --nocapture"
  "doctor detects task-view drift|cargo test -p brehon-doctor checkers::store_search::store_search_tests::test_view_drift_flags_missing_task_view -- --exact --nocapture"
  "doctor detects review-view drift|cargo test -p brehon-doctor checkers::store_search::store_search_tests::test_review_view_drift_flags_missing_review_view -- --exact --nocapture"
  "doctor detects Tantivy/Fjall drift|cargo test -p brehon-doctor checkers::store_search::store_search_tests::test_tantivy_fjall_drift_detects_extra_index_entries -- --exact --nocapture"
  "tantivy reindex repopulates from authoritative entries|cargo test -p brehon-search-tantivy tests::test_reindex_repopulates_from_entries -- --exact --nocapture"
  "tantivy reindex clears stale entries when authoritative set is empty|cargo test -p brehon-search-tantivy tests::test_reindex_with_empty_entries_clears_index -- --exact --nocapture"
  "config layer rejects empty worker pools as fatal typed validation|cargo test -p brehon-config tests::load_config_with_override_rejects_empty_worker_pools -- --exact --nocapture"
  "config layer rejects empty reviewer pools as fatal typed validation|cargo test -p brehon-config tests::load_config_with_override_rejects_empty_reviewer_pools -- --exact --nocapture"
  "config layer rejects empty lanes map as fatal typed validation|cargo test -p brehon-config tests::load_config_with_override_rejects_empty_lanes_map -- --exact --nocapture"
)

required_symbols=(
  "crates/brehon-mcp/src/tools/memory_tests.rs|test_create_list_search_delete_memory_round_trip"
  "crates/brehon-mcp/src/tools/memory_tests.rs|test_memory_tools_use_attached_search_index_and_event_store"
  "crates/brehon-mcp/src/tools/tasks_tests.rs|test_get_task_context_tool"
  "crates/brehon-mcp/src/tools/tasks_tests.rs|test_get_task_context_rejects_invalid_task_id"
  "crates/brehon-mcp/src/server_tests.rs|tool_panic_catches_panics_and_returns_internal_error"
  "crates/brehon-mcp/src/server_tests.rs|input_size_rejects_oversized_arguments_with_caller_attribution"
  "crates/brehon-mcp/src/server_tests.rs|input_size_uses_cached_env_config_from_server_init"
  "crates/brehon-mcp/src/server_tests.rs|input_size_bounded_transport_recovers_after_oversized_frame"
  "crates/brehon-doctor/src/checkers/store_search_tests.rs|test_queue_lease_detects_orphaned_claim"
  "crates/brehon-doctor/src/checkers/store_search_tests.rs|test_view_drift_flags_missing_task_view"
  "crates/brehon-doctor/src/checkers/store_search_tests.rs|test_review_view_drift_flags_missing_review_view"
  "crates/brehon-doctor/src/checkers/store_search_tests.rs|test_tantivy_fjall_drift_detects_extra_index_entries"
  "crates/brehon-search-tantivy/src/lib.rs|test_reindex_repopulates_from_entries"
  "crates/brehon-search-tantivy/src/lib.rs|test_reindex_with_empty_entries_clears_index"
  "crates/brehon-config/src/lib.rs|load_config_with_override_rejects_empty_worker_pools"
  "crates/brehon-config/src/lib.rs|load_config_with_override_rejects_empty_reviewer_pools"
  "crates/brehon-config/src/lib.rs|load_config_with_override_rejects_empty_lanes_map"
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
  printf 'Phase 3 stability gate dry run (%d checks)\n' "${#checks[@]}"
  for entry in "${checks[@]}"; do
    IFS='|' read -r label command <<<"$entry"
    printf ' - %s\n   %s\n' "$label" "$command"
  done
  exit 0
fi

if ! command -v rg >/dev/null 2>&1; then
  echo "error: ripgrep (rg) is required for phase3 symbol preflight checks" >&2
  exit 1
fi

verify_required_symbols

printf 'Phase 3 stability gate: running %d checks\n' "${#checks[@]}"
for entry in "${checks[@]}"; do
  IFS='|' read -r label command <<<"$entry"
  run_check "$label" "$command"
done

printf '\nPhase 3 stability gate passed.\n'
