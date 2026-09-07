#!/usr/bin/env bash
set -euo pipefail

product=$(CDPATH= cd -- "$(dirname -- "${BASH_SOURCE[0]}")/.." && pwd)
workspace=$(CDPATH= cd -- "$product/.." && pwd)
results="$product/benchmarks/results"
image=tessivum-benchmark:ubuntu-24.04-arm64

rm -rf "$results"
mkdir -p "$results"
docker build --platform linux/arm64 --tag "$image" --file "$product/benchmarks/Dockerfile" "$product"
container=$(docker create --init --platform linux/arm64 --shm-size 2g \
  --env "SAMPLES=${SAMPLES:-30}" \
  --env "VERIFY_PLUGIN=${VERIFY_PLUGIN:-0}" \
  --env "CARGO_TARGET_DIR=/opt/cargo/target" \
  --mount type=volume,source=tessivum-benchmark-cargo-target,target=/opt/cargo/target \
  --mount type=volume,source=tessivum-benchmark-pnpm-store,target=/opt/pnpm/store \
  --volume "$workspace:/source:ro" \
  "$image")
trap 'docker rm --force "$container" >/dev/null 2>&1 || true' EXIT
set +e
docker start --attach "$container"
status=$?
set -e
docker cp "$container:/results/." "$results/"
exit "$status"
