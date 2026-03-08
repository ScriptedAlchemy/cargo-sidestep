#!/bin/sh

if [ "$1" = "-V" ]; then
  echo "cargo 1.91.0 (fake 2026-03-08)"
  exit 0
fi

attempts_file="${FAKE_CARGO_ATTEMPTS_FILE:?missing FAKE_CARGO_ATTEMPTS_FILE}"
base_home="${FAKE_BASE_CARGO_HOME:?missing FAKE_BASE_CARGO_HOME}"
mkdir -p "$(dirname "$attempts_file")"
if [ ! -f "$attempts_file" ]; then
  echo 0 >"$attempts_file"
fi
attempts=$(cat "$attempts_file")
attempts=$((attempts + 1))
echo "$attempts" >"$attempts_file"

mode="${FAKE_CARGO_MODE:-package-cache}"

if [ "$mode" = "build-dir" ] && [ "$attempts" -eq 1 ]; then
  echo "Blocking waiting for file lock on build directory" >&2
  sleep 2
  exit 99
fi

if [ "$mode" = "package-cache" ] && [ "$attempts" -eq 1 ]; then
  echo "Blocking waiting for file lock on package cache" >&2
  sleep 2
  exit 98
fi

if [ "$mode" = "package-cache" ] && [ "$attempts" -eq 2 ] && [ "${CARGO_NET_OFFLINE:-}" = "true" ]; then
  echo "attempting to make an HTTP request, but --offline was specified" >&2
  exit 97
fi

printf 'plan=%s\n' "${CARGO_SIDESTEP_PLAN:-unknown}"
printf 'subcommand=%s\n' "${1:-unset}"
printf 'cargo_home=%s\n' "${CARGO_HOME:-unset}"
printf 'target_dir=%s\n' "${CARGO_TARGET_DIR:-unset}"
printf 'build_dir=%s\n' "${CARGO_BUILD_BUILD_DIR:-unset}"
printf 'base_home=%s\n' "$base_home"
exit 0
