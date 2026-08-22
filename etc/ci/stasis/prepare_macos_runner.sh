#!/usr/bin/env bash

set -euo pipefail

mode=${1:-prepare}
minimum_free_kib=${2:-10485760}

if [[ "$mode" != 'prepare' && "$mode" != 'check' ]]; then
  echo "usage: $0 prepare|check [minimum-free-kib]" >&2
  exit 2
fi
if [[ ! "$minimum_free_kib" =~ ^[1-9][0-9]*$ ]]; then
  echo 'minimum-free-kib must be a positive integer' >&2
  exit 2
fi
if [[ "${GITHUB_ACTIONS:-}" != 'true' || "${RUNNER_ENVIRONMENT:-}" != 'github-hosted' ]]; then
  echo 'runner preparation is restricted to an ephemeral GitHub-hosted runner' >&2
  exit 1
fi
if [[ -z "${GITHUB_WORKSPACE:-}" || -z "${RUNNER_TEMP:-}" ]]; then
  echo 'GitHub runner paths are unavailable' >&2
  exit 1
fi
if [[ "$(uname -s)" != 'Darwin' || "$(uname -m)" != 'arm64' ]]; then
  echo 'runner preparation requires native macOS Apple Silicon' >&2
  exit 1
fi
if [[ "$(sysctl -n hw.optional.arm64)" != '1' ]]; then
  echo 'runner does not report native Arm64 support' >&2
  exit 1
fi

echo 'Selected macOS build tools:'
sw_vers
xcode-select -p
xcrun --find clang
xcrun clang --version
echo 'Runner disk before preparation/check:'
df -h "$GITHUB_WORKSPACE" "$RUNNER_TEMP"

if [[ "$mode" == 'prepare' ]]; then
  # This runner is ephemeral. Remove only disposable simulator devices and
  # package-manager leftovers; retain the selected Xcode and compiler toolchain.
  xcrun simctl shutdown all || true
  xcrun simctl delete unavailable || true
  xcrun simctl delete all || true
  if command -v brew >/dev/null 2>&1; then
    brew cleanup --prune=all
  fi
  echo 'Runner disk after safe ephemeral cleanup:'
  df -h "$GITHUB_WORKSPACE" "$RUNNER_TEMP"
fi

available_kib=$(LC_ALL=C df -Pk "$GITHUB_WORKSPACE" | awk 'NR == 2 { print $4 }')
if [[ ! "$available_kib" =~ ^[0-9]+$ ]]; then
  echo 'could not determine available runner disk space' >&2
  exit 1
fi
if (( available_kib < minimum_free_kib )); then
  printf 'runner has %s KiB free; at least %s KiB is required\n' \
    "$available_kib" "$minimum_free_kib" >&2
  exit 1
fi
printf 'runner disk gate passed: %s KiB free (minimum %s KiB)\n' \
  "$available_kib" "$minimum_free_kib"
