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
cargo build --locked --release -p tessivum-bench

cd "$product"
CORDIS_VENDOR_ROOT="$cordis" cargo build --locked --release --bin tessivum
binary="$product/target/release/tessivum"

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
env "${profile_environment[@]}" "$binary" --data-dir "$compat_profile" plugin add tessivum-market
for package in dsh-better-sidebar@0.16.1 dsh-dream-skin@8.30.1; do
  env "${profile_environment[@]}" "$binary" --data-dir "$compat_profile" plugin add "$package"
done

chromium=$(cd "$product/web" && bun -e "const { chromium } = require('playwright-core'); console.log(chromium.executablePath())")
[[ -x "$chromium" ]] || { echo "Playwright Chromium is missing: $chromium" >&2; exit 2; }

python3 "$core/scripts/run_paired_benchmarks.py" \
  --rust-bin "$core/target/release/tessivum-bench" \
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
