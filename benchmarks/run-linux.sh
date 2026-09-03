#!/usr/bin/env bash
set -euo pipefail

source_root=${SOURCE_ROOT:-/source}
work_root=${WORK_ROOT:-/bench/work}
results_root=${RESULTS_ROOT:-/results}
samples=${SAMPLES:-30}

[[ $samples =~ ^[1-9][0-9]*$ ]] || { echo "SAMPLES must be a positive integer" >&2; exit 2; }
for path in tessivum tessivum-core upstream/deepseek-harness; do
  [[ -d "$source_root/$path/.git" ]] || { echo "missing source repository: $source_root/$path" >&2; exit 2; }
done

rm -rf "$work_root"
mkdir -p "$work_root/upstream" "$results_root"
rsync -a --exclude target --exclude node_modules --exclude web/dist --exclude web/client-packages --exclude dist --exclude benchmarks/results \
  "$source_root/tessivum/" "$work_root/tessivum/"
rsync -a --exclude target --exclude node_modules \
  "$source_root/tessivum-core/" "$work_root/tessivum-core/"
rsync -a --exclude node_modules \
  "$source_root/upstream/deepseek-harness/" "$work_root/upstream/deepseek-harness/"

product="$work_root/tessivum"
core="$work_root/tessivum-core"
dsh="$work_root/upstream/deepseek-harness"
cordis="$dsh/vendor"
cargo_target=${CARGO_TARGET_DIR:-$work_root/cargo-target}
core_target=${CORE_CARGO_TARGET_DIR:-$cargo_target/core}
product_target=${PRODUCT_CARGO_TARGET_DIR:-$cargo_target/product}

cd "$dsh"
pnpm install --frozen-lockfile
pnpm run build:lib:host

cd "$product/web"
bun install --frozen-lockfile
CORDIS_VENDOR_ROOT="$cordis" TESSIVUM_DEEPSEEK_SOURCE="$dsh" bun run build

market_dir="$work_root/market"
mkdir -p "$market_dir"
cd "$product/plugins/market"
bun install --frozen-lockfile
bun pm pack --ignore-scripts --destination "$market_dir"
market_tgz="$market_dir/tessivum-market-0.1.0-alpha.23.tgz"
[[ -f "$market_tgz" ]]
(
  cd "$market_dir"
  sha256sum "$(basename "$market_tgz")" > "$(basename "$market_tgz").sha256"
)
cp "$product/packaging/market-source.json" "$market_tgz.source.json"

host_modules="$work_root/host-modules"
python3 "$product/scripts/fetch_host_modules.py" "$product/packaging/host-modules.json" "$host_modules"
cp -a "$product/compat/host-modules/." "$host_modules/"

cd "$core"
CARGO_TARGET_DIR="$core_target" cargo build --locked --release -p tessivum-bench

cd "$product"
CORDIS_VENDOR_ROOT="$cordis" CARGO_TARGET_DIR="$product_target" cargo build --locked --release --bin tessivum
binary="$product_target/release/tessivum"

compat_profile="$work_root/compatibility-profile"
compat_host="$core/node/compat-host/src/index.ts"
profile_environment=(
  TESSIVUM_COMPAT_HOST="$compat_host"
  TESSIVUM_HOST_MODULE_ROOT="$host_modules"
  CORDIS_VENDOR_ROOT="$cordis"
  TESSIVUM_MARKET_TARBALL="$market_tgz"
  TESSIVUM_MARKET_SHA256_FILE="$market_tgz.sha256"
  TESSIVUM_MARKET_SOURCE_FILE="$market_tgz.source.json"
)

seed_log="$work_root/market-seed.log"
TESSIVUM_REMOTE_ACCESS=0 TESSIVUM_WEB_ADDR=127.0.0.1:0 env "${profile_environment[@]}" \
  "$binary" web --data-dir "$compat_profile" >"$seed_log" 2>&1 &
seed_pid=$!
seeded=0
for _ in {1..1200}; do
  if jq -e '.dependencies["tessivum-market"] | strings | startswith("file:")' \
    "$compat_profile/plugins/package.json" >/dev/null 2>&1; then
    seeded=1
    break
  fi
  if ! kill -0 "$seed_pid" 2>/dev/null; then
    wait "$seed_pid" || true
    cat "$seed_log" >&2
    exit 1
  fi
  sleep 0.1
done
kill -TERM "$seed_pid" 2>/dev/null || true
wait "$seed_pid" || true
if (( seeded == 0 )); then
  cat "$seed_log" >&2
  echo "timed out while installing the packaged market" >&2
  exit 1
fi
for package in dsh-better-sidebar@0.16.1 dsh-dream-skin@8.30.1; do
  env "${profile_environment[@]}" "$binary" --data-dir "$compat_profile" plugin add "$package"
done

chromium=$(cd "$product/web" && bun -e "const { chromium } = require('playwright-core'); console.log(chromium.executablePath())")
[[ -x "$chromium" ]] || { echo "Playwright Chromium is missing: $chromium" >&2; exit 2; }

python3 "$core/scripts/run_paired_benchmarks.py" \
  --rust-bin "$core_target/release/tessivum-bench" \
  --cordis-root "$cordis" \
  --workload "$core/fixtures/benchmarks/core-paired.json" \
  --samples "$samples" \
  --raw-out "$results_root/core-paired.json"

publication=()
if (( samples >= 30 )); then publication=(--publication); fi
TESSIVUM_BENCH_COMPAT_PROFILE="$compat_profile/plugins" \
TESSIVUM_BENCH_COMPAT_HOST="$compat_host" \
TESSIVUM_BENCH_HOST_MODULE_ROOT="$host_modules" \
TESSIVUM_BENCH_CORDIS_VENDOR_ROOT="$cordis" \
TESSIVUM_CHROMIUM="$chromium" \
python3 "$product/scripts/benchmark_product.py" \
  --manifest "$product/benchmarks/manifests/base.json" \
  --manifest "$product/benchmarks/manifests/compatibility.json" \
  --binary "tessivum=$binary" \
  --samples "$samples" \
  --interleave \
  "${publication[@]}" \
  --raw-out "$results_root/product.json"
