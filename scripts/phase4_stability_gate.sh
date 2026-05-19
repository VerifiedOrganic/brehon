#!/usr/bin/env bash
set -euo pipefail

usage() {
  cat <<'USAGE'
Usage: scripts/phase4_stability_gate.sh [--dry-run]

Run the Phase 4 stability gate harness.

Checks:
  - Git recovery survives stale locks, rebase conflicts, and dirty worktree restarts
  - PTY lifetime management uses owned cancellable tasks with proper kill/reap
  - TUI responsiveness holds under non-blocking delivery and mux backpressure

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

# Use Zig 0.15.2 if available (required for ghostty_vt_sys build compatibility)
if [[ -z "${ZIG:-}" ]] && [[ -f /tmp/zig-aarch64-macos-0.15.2/zig ]]; then
  export ZIG=/tmp/zig-aarch64-macos-0.15.2/zig
fi

checks=(
  "brehon-git unit tests|cargo test -p brehon-git"
  "brehon-pty unit tests|cargo test -p brehon-pty"
  "ghostty_vt unit tests|cargo test -p ghostty_vt"
  "brehon-tui unit tests|cargo test -p brehon-tui"
  "brehon-mux unit tests|cargo test -p brehon-mux"
  "git integration tests|cargo test -p brehon-test-harness --test git_tests"
  "crash recovery tests|cargo test -p brehon-test-harness --test crash_tests"
  "scenario tests|cargo test -p brehon-test-harness --test scenarios_tests"
  "stress tests|cargo test -p brehon-test-harness --test stress_tests"
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
  printf 'Phase 4 stability gate dry run (%d checks)\n' "${#checks[@]}"
  for entry in "${checks[@]}"; do
    IFS='|' read -r label command <<<"$entry"
    printf ' - %s\n   %s\n' "$label" "$command"
  done
  exit 0
fi

printf 'Phase 4 stability gate: running %d checks\n' "${#checks[@]}"
for entry in "${checks[@]}"; do
  IFS='|' read -r label command <<<"$entry"
  run_check "$label" "$command"
done

printf '\nPhase 4 stability gate passed.\n'
