#!/usr/bin/env bash
set -euo pipefail

usage() {
  cat <<'USAGE'
Usage: scripts/refactor-check.sh [--dry-run]

Fast-feedback compilation check for the refactor.

Runs cargo check on the three core crates (brehon-tui, brehon-mux, brehon-acp)
and exits non-zero on any failure.

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

crates=(
  "brehon-tui"
  "brehon-mux"
  "brehon-acp"
)

args=()
for crate in "${crates[@]}"; do
  args+=(-p "$crate")
done

if [[ "$dry_run" -eq 1 ]]; then
  printf 'Refactor check dry run (%d crates)\n' "${#crates[@]}"
  for crate in "${crates[@]}"; do
    printf ' - cargo check -p %s\n' "$crate"
  done
  exit 0
fi

printf 'Refactor check: running cargo check on %d crates\n' "${#crates[@]}"
cargo check "${args[@]}"
printf '\nRefactor check passed.\n'
