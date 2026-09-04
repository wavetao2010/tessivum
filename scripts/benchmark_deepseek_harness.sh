#!/bin/sh
set -eu

: "${TESSIVUM_BENCH_DSH_BIN:?}"
: "${TESSIVUM_BENCH_DSH_PATCH:?}"
: "${TESSIVUM_BENCH_DSH_REPLAY:?}"
: "${TESSIVUM_BENCH_DSH_REPLAY_PLUGIN:?}"

if [ "$#" -eq 1 ] && [ "$1" = "--version" ]; then
  exec node "$TESSIVUM_BENCH_DSH_BIN" --version
fi

export DSH_PERMISSION_MODE=danger-full-access
export DSH_SNAPSHOT_FILE="$TESSIVUM_BENCH_DSH_REPLAY"

if [ "${1-}" = "web" ]; then
  shift
  [ "${1-}" = "--data-dir" ] || { echo "expected web --data-dir" >&2; exit 2; }
  export DSH_HOME=$2
  shift 2
  : "${TESSIVUM_BENCH_DSH_REPLAY_CHILD_FILES:?}"
  export DSH_SNAPSHOT_CHILD_FILES="$TESSIVUM_BENCH_DSH_REPLAY_CHILD_FILES"
  exec node "$TESSIVUM_BENCH_DSH_BIN" web --patch "$TESSIVUM_BENCH_DSH_PATCH" "$@"
fi

[ "${1-}" = "--data-dir" ] || { echo "expected --data-dir" >&2; exit 2; }
export DSH_HOME=$2
shift 2
[ "${1-}" = "--replay" ] || { echo "expected --replay" >&2; exit 2; }
shift 2
[ "${1-}" = "--trusted-bash" ] || { echo "expected --trusted-bash" >&2; exit 2; }
shift
unset DSH_SNAPSHOT_CHILD_FILES
exec node "$TESSIVUM_BENCH_DSH_BIN" --profile headless --patch "$TESSIVUM_BENCH_DSH_PATCH" "$@"
