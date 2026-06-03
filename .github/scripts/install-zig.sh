#!/usr/bin/env bash
set -euo pipefail

version="${ZIG_VERSION:-}"
if [[ -z "${version}" ]]; then
  echo "ZIG_VERSION is required" >&2
  exit 1
fi

case "$(uname -s)-$(uname -m)" in
  Linux-x86_64) target="x86_64-linux" ;;
  Linux-aarch64 | Linux-arm64) target="aarch64-linux" ;;
  Darwin-x86_64) target="x86_64-macos" ;;
  Darwin-arm64) target="aarch64-macos" ;;
  *)
    echo "unsupported Zig platform: $(uname -s)-$(uname -m)" >&2
    exit 1
    ;;
esac

temp_dir="${RUNNER_TEMP:-/tmp}"
install_dir="${temp_dir}/zig-${version}"
archive="${temp_dir}/zig-${target}-${version}.tar.xz"

if [[ ! -x "${install_dir}/zig" ]]; then
  rm -rf "${install_dir}"
  mkdir -p "${install_dir}"
  curl -fsSLo "${archive}" "https://ziglang.org/download/${version}/zig-${target}-${version}.tar.xz"
  tar -xf "${archive}" -C "${install_dir}" --strip-components=1
fi

if [[ -n "${GITHUB_PATH:-}" ]]; then
  echo "${install_dir}" >> "${GITHUB_PATH}"
fi

"${install_dir}/zig" version
