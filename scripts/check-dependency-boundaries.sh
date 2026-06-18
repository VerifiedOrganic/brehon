#!/usr/bin/env bash
set -euo pipefail

repo_root="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$repo_root"

crate_name() {
  awk '
    /^\[package\]/ { in_package = 1; next }
    /^\[/ { in_package = 0 }
    in_package && /^name[[:space:]]*=/ {
      value = $0
      sub(/^[^=]*=[[:space:]]*/, "", value)
      gsub(/"/, "", value)
      print value
      exit
    }
  ' "$1"
}

dependencies() {
  awk '
    /^\[dependencies\]/ { in_dependencies = 1; next }
    /^\[/ { in_dependencies = 0 }
    in_dependencies && /^[[:space:]]*brehon-[A-Za-z0-9_-]+[[:space:]]*=/ {
      value = $0
      sub(/^[[:space:]]*/, "", value)
      sub(/[[:space:]]*=.*/, "", value)
      print value
    }
  ' "$1"
}

allowed_dependencies() {
  case "$1" in
    brehon-types) echo "" ;;
    brehon-ports) echo "brehon-types" ;;
    brehon-config) echo "brehon-types brehon-adapter-sdk" ;;
    brehon-protocol) echo "brehon-types" ;;
    brehon-recording) echo "" ;;
    brehon-adapter-sdk) echo "brehon-types" ;;
    brehon-adapter-agy) echo "brehon-adapter-sdk brehon-types brehon-ports brehon-config" ;;
    brehon-adapter-claude) echo "brehon-adapter-sdk brehon-types" ;;
    brehon-adapter-codex) echo "brehon-adapter-sdk brehon-types brehon-ports" ;;
    brehon-adapter-copilot) echo "brehon-adapter-sdk brehon-types brehon-ports" ;;
    brehon-adapter-gemini) echo "brehon-adapter-sdk brehon-types brehon-ports" ;;
    brehon-adapter-junie) echo "brehon-adapter-sdk brehon-types brehon-ports brehon-config" ;;
    brehon-adapter-kimi) echo "brehon-adapter-sdk brehon-types brehon-ports" ;;
    brehon-adapter-openai) echo "brehon-adapter-sdk brehon-types" ;;
    brehon-adapter-opencode) echo "brehon-adapter-sdk brehon-types" ;;
    brehon-store-fjall) echo "brehon-types brehon-ports" ;;
    brehon-search-tantivy) echo "brehon-types brehon-ports" ;;
    brehon-git) echo "brehon-types brehon-ports" ;;
    brehon-detect) echo "brehon-types brehon-ports" ;;
    brehon-policy) echo "brehon-types brehon-ports" ;;
    brehon-runtime) echo "brehon-types brehon-ports" ;;
    brehon-notify) echo "brehon-types" ;;
    brehon-workflow) echo "brehon-types brehon-ports" ;;
    brehon-host) echo "brehon-types brehon-ports" ;;
    brehon-supervisor) echo "brehon-types brehon-ports" ;;
    brehon-doctor) echo "brehon-types brehon-config" ;;
    brehon-daemon) echo "brehon-ports brehon-runtime brehon-types brehon-detect brehon-workflow" ;;
    brehon-acp) echo "brehon-types brehon-ports brehon-adapter-sdk brehon-adapter-opencode brehon-adapter-gemini brehon-adapter-kimi brehon-adapter-codex brehon-adapter-copilot brehon-adapter-junie brehon-adapter-agy brehon-adapter-openai" ;;
    brehon-pty) echo "brehon-adapter-agy brehon-adapter-claude brehon-adapter-copilot brehon-adapter-junie brehon-adapter-kimi brehon-adapter-sdk brehon-config brehon-types" ;;
    brehon-mux) echo "brehon-recording brehon-protocol brehon-pty brehon-ports brehon-acp brehon-adapter-sdk brehon-types" ;;
    brehon-review) echo "brehon-types brehon-ports brehon-mux brehon-test-harness" ;;
    brehon-mcp) echo "brehon-config brehon-notify brehon-mux brehon-types brehon-ports brehon-review brehon-git" ;;
    brehon-tui) echo "brehon-notify brehon-mux brehon-ports brehon-types" ;;
    brehon-native-agent) echo "brehon-adapter-sdk brehon-mcp brehon-types" ;;
    brehon-orchestrator) echo "brehon-types brehon-ports brehon-test-harness" ;;
    brehon-gatekeeper) echo "brehon-git" ;;
    brehon-test-harness) echo "brehon-types brehon-ports" ;;
    brehon-cli) echo "*" ;;
    ghostty_vt_sys|ghostty_vt|splash-demo) echo "" ;;
    *) return 1 ;;
  esac
}

contains_dependency() {
  local needle="$1"
  local haystack="$2"
  local dep
  for dep in $haystack; do
    [[ "$dep" == "$needle" ]] && return 0
  done
  return 1
}

violations=0
checked=0

while IFS= read -r cargo_toml; do
  crate="$(crate_name "$cargo_toml")"
  if [[ -z "$crate" ]]; then
    printf 'dependency-boundary violation: could not read package name from %s\n' "$cargo_toml" >&2
    violations=$((violations + 1))
    continue
  fi

  if ! allowed="$(allowed_dependencies "$crate")"; then
    printf 'dependency-boundary violation: crate %s has no category in docs/ARCHITECTURE_RULES.md\n' "$crate" >&2
    violations=$((violations + 1))
    continue
  fi

  checked=$((checked + 1))
  while IFS= read -r dep; do
    [[ -z "$dep" ]] && continue
    [[ "$allowed" == "*" ]] && continue
    if ! contains_dependency "$dep" "$allowed"; then
      printf 'dependency-boundary violation: %s depends on %s in %s\n' "$crate" "$dep" "$cargo_toml" >&2
      printf '  allowed: %s\n' "${allowed:-<none>}" >&2
      violations=$((violations + 1))
    fi
  done < <(dependencies "$cargo_toml")
done < <(find crates -mindepth 2 -maxdepth 2 -name Cargo.toml | sort)

if [[ "$violations" -ne 0 ]]; then
  printf 'Dependency boundary check failed with %d violation(s).\n' "$violations" >&2
  exit 1
fi

printf 'Dependency boundary check passed for %d crate(s).\n' "$checked"
