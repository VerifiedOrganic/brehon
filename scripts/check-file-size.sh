#!/usr/bin/env bash
set -euo pipefail

repo_root="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$repo_root"

max_lines="${BREHON_FILE_SIZE_MAX_LINES:-900}"
baseline_file="${BREHON_FILE_SIZE_BASELINE:-docs/FILE_SIZE_BASELINE.txt}"

case "$max_lines" in
  ''|*[!0-9]*)
    printf 'Invalid BREHON_FILE_SIZE_MAX_LINES: %s\n' "$max_lines" >&2
    exit 2
    ;;
esac

if [[ ! -f "$baseline_file" ]]; then
  printf 'File-size baseline not found: %s\n' "$baseline_file" >&2
  exit 2
fi

baseline_cap() {
  local target="$1"
  awk -v target="$target" '
    /^[[:space:]]*#/ || /^[[:space:]]*$/ { next }
    $2 == target { print $1; found = 1; exit }
    END { if (!found) exit 1 }
  ' "$baseline_file"
}

violations=0
checked=0
oversized=0

while IFS= read -r file; do
  checked=$((checked + 1))
  lines="$(wc -l < "$file" | tr -d ' ')"
  path="${file#./}"
  if [[ "$lines" -le "$max_lines" ]]; then
    continue
  fi
  oversized=$((oversized + 1))

  if ! cap="$(baseline_cap "$path")"; then
    printf 'file-size violation: %s has %s line(s), above max %s, and is not in %s\n' "$path" "$lines" "$max_lines" "$baseline_file" >&2
    violations=$((violations + 1))
    continue
  fi

  if [[ "$lines" -gt "$cap" ]]; then
    printf 'file-size violation: %s grew to %s line(s), above baseline cap %s\n' "$path" "$lines" "$cap" >&2
    violations=$((violations + 1))
  fi
done < <(find . \
  -path './target' -prune -o \
  -path './.git' -prune -o \
  -path './.brehon' -prune -o \
  -name '*.rs' -type f -print | sort)

if [[ "$violations" -ne 0 ]]; then
  printf 'File-size check failed with %d violation(s).\n' "$violations" >&2
  exit 1
fi

printf 'File-size check passed for %d Rust file(s); %d file(s) are tracked in the oversized baseline.\n' "$checked" "$oversized"
