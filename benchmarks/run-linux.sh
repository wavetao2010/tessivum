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
git clone --quiet --no-local "$source_root/tessivum" "$work_root/tessivum"
git clone --quiet --no-local "$source_root/tessivum-core" "$work_root/tessivum-core"
git clone --quiet --no-local "$source_root/upstream/deepseek-harness" "$work_root/upstream/deepseek-harness"

product="$work_root/tessivum"
core="$work_root/tessivum-core"
dsh="$work_root/upstream/deepseek-harness"
cordis="$dsh/vendor"
cargo_target=${CARGO_TARGET_DIR:-$work_root/cargo-target}
core_target=${CORE_CARGO_TARGET_DIR:-$cargo_target/core}
product_target=${PRODUCT_CARGO_TARGET_DIR:-$cargo_target/product}

patch="$product/web/patches/deepseek-harness.patch"
expected_dsh_diff=$(jq -r '.source.deepseekHarness.trackedDiffSha256' "$product/benchmarks/manifests/base.json")
actual_dsh_diff=$(git -C "$dsh" diff --binary --full-index --no-ext-diff | sha256sum | cut -d' ' -f1)
if [[ $actual_dsh_diff == e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855 ]]; then
  git -C "$dsh" apply "$patch"
  while IFS= read -r -d '' path; do git -C "$dsh" add --intent-to-add -- "$path"; done < <(git -C "$dsh" ls-files --others --exclude-standard -z)
  actual_dsh_diff=$(git -C "$dsh" diff --binary --full-index --no-ext-diff | sha256sum | cut -d' ' -f1)
fi
[[ $actual_dsh_diff == "$expected_dsh_diff" ]] || { echo "DeepSeek Harness tracked diff does not match $patch" >&2; exit 2; }

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

chromium=$(cd "$product/web" && bun -e "const { chromium } = require('playwright-core'); console.log(chromium.executablePath())")
[[ -x "$chromium" ]] || { echo "Playwright Chromium is missing: $chromium" >&2; exit 2; }

if [[ ${VERIFY_PLUGIN:-0} == 1 ]]; then
  ledger="$product/plugins/market/compatibility.json"
  [[ $(jq '.current | length' "$ledger") == 1 ]] || { echo "plugin verifier requires exactly one current release" >&2; exit 2; }
  plugin=$(jq -r '.current | keys[0]' "$ledger")
  version=$(jq -r --arg plugin "$plugin" '.current[$plugin]' "$ledger")
  update_version=$(jq -r --arg plugin "$plugin" --arg version "$version" '.entries[] | select(.npm == $plugin and .version == $version) | .verification.updateVersion' "$ledger")
  failure_version=$(jq -r --arg plugin "$plugin" --arg version "$version" '.entries[] | select(.npm == $plugin and .version == $version) | .verification.failureVersion' "$ledger")
  boot_entry=$(jq -r --arg plugin "$plugin" --arg version "$version" '.entries[] | select(.npm == $plugin and .version == $version) | .verification.browserBootEntry' "$ledger")
  feature_name=$(jq -r --arg plugin "$plugin" --arg version "$version" '.entries[] | select(.npm == $plugin and .version == $version) | .verification.browserFeature' "$ledger")
  feature_selector=$(jq -r --arg plugin "$plugin" --arg version "$version" '.entries[] | select(.npm == $plugin and .version == $version) | .verification.browserFeatureSelector' "$ledger")
  [[ $plugin != null && $version != null && $update_version != null && $failure_version != null && $boot_entry == "$plugin" && $feature_name != null && $feature_selector != null ]]
  product_evidence="$results_root/$plugin-$version-product.json"
  verification_evidence="$results_root/$plugin-$version.json"

  install_output=$(env "${profile_environment[@]}" "$binary" --data-dir "$compat_profile" plugin add "$plugin@$version" 2>&1)
  printf '%s\n' "$install_output"
  env "${profile_environment[@]}" "$binary" --data-dir "$compat_profile" plugin add dsh-dream-skin@8.30.1
  [[ $(jq -r '.version' "$compat_profile/plugins/node_modules/$plugin/package.json") == "$version" ]]

  TESSIVUM_BENCH_COMPAT_PROFILE="$compat_profile/plugins" \
  TESSIVUM_BENCH_COMPAT_HOST="$compat_host" \
  TESSIVUM_BENCH_HOST_MODULE_ROOT="$host_modules" \
  TESSIVUM_BENCH_CORDIS_VENDOR_ROOT="$cordis" \
  TESSIVUM_BENCH_FEATURE_SELECTOR="$feature_selector" \
  TESSIVUM_CHROMIUM="$chromium" \
  python3 "$product/scripts/benchmark_product.py" \
    --manifest "$product/benchmarks/manifests/compatibility.json" \
    --binary "tessivum=$binary" \
    --samples 1 \
    --raw-out "$product_evidence"
  jq -e --arg plugin "$plugin" --arg featureSelector "$feature_selector" '
    .status == "passed"
    and (.rawSamples[0].web.browser.result.browserFeature | .selector == $featureSelector and .visible == true and .count >= 1)
    and (.rawSamples[0].web.browser.result.bootPlugins | any(.id == $plugin))
    and ([.rawSamples[0].headless.cleanup, .rawSamples[0].web.browser.cleanup, .rawSamples[0].web.cleanup]
      | all(.residueAfterDispose == 0 and .forcedCleanupRequired == false and .residueAfterForcedCleanup == 0))
  ' "$product_evidence" >/dev/null

  update_output=$(env "${profile_environment[@]}" "$binary" --data-dir "$compat_profile" plugin add "$plugin@$update_version" 2>&1)
  printf '%s\n' "$update_output"
  [[ $(jq -r '.version' "$compat_profile/plugins/node_modules/$plugin/package.json") == "$update_version" ]]
  remove_output=$(env "${profile_environment[@]}" "$binary" --data-dir "$compat_profile" plugin remove "$plugin" 2>&1)
  printf '%s\n' "$remove_output"
  [[ ! -e "$compat_profile/plugins/node_modules/$plugin" ]]
  jq -e --arg plugin "$plugin" '.dependencies[$plugin] == null and (.dsh.profile.bundles | index($plugin) | not)' "$compat_profile/plugins/package.json" >/dev/null

  manifest_before=$(sha256sum "$compat_profile/plugins/package.json" | cut -d' ' -f1)
  lock_before=$(sha256sum "$compat_profile/plugins/pnpm-lock.yaml" | cut -d' ' -f1)
  if failure_output=$(env "${profile_environment[@]}" "$binary" --data-dir "$compat_profile" plugin add "$plugin@$failure_version" 2>&1); then
    printf '%s\n' "$failure_output"
    echo "expected unavailable plugin release to fail" >&2
    exit 1
  else
    failure_status=$?
    printf '%s\n' "$failure_output" >&2
  fi
  manifest_after=$(sha256sum "$compat_profile/plugins/package.json" | cut -d' ' -f1)
  lock_after=$(sha256sum "$compat_profile/plugins/pnpm-lock.yaml" | cut -d' ' -f1)
  [[ $manifest_after == "$manifest_before" ]]
  [[ $lock_after == "$lock_before" ]]
  [[ ! -e "$compat_profile/plugins/node_modules/$plugin" ]]

  jq -n \
    --arg plugin "$plugin" --arg version "$version" --arg updateVersion "$update_version" --arg failureVersion "$failure_version" \
    --arg featureName "$feature_name" --arg featureSelector "$feature_selector" \
    --arg installOutput "$install_output" --arg updateOutput "$update_output" --arg removeOutput "$remove_output" \
    --arg failureOutput "$failure_output" --argjson failureStatus "$failure_status" \
    --arg manifestBeforeSha256 "$manifest_before" --arg manifestAfterSha256 "$manifest_after" \
    --arg lockBeforeSha256 "$lock_before" --arg lockAfterSha256 "$lock_after" \
    --arg productRevision "$(git -C "$product" rev-parse HEAD)" \
    --arg coreRevision "$(git -C "$core" rev-parse HEAD)" \
    --arg deepseekRevision "$(git -C "$dsh" rev-parse HEAD)" \
    --arg binarySha256 "$(sha256sum "$binary" | cut -d' ' -f1)" \
    --arg productEvidencePath "${product_evidence##*/}" --arg productEvidenceSha256 "$(sha256sum "$product_evidence" | cut -d' ' -f1)" \
    '{schema:"tessivum.plugin-lifecycle-verification/v1", plugin:$plugin, verifiedVersion:$version,
      updateVersion:$updateVersion, failureVersion:$failureVersion,
      revisions:{product:$productRevision,core:$coreRevision,deepseekHarness:$deepseekRevision},
      binarySha256:$binarySha256, productEvidence:{path:$productEvidencePath,sha256:$productEvidenceSha256},
      checks:{
        exactInstall:{installedVersion:$version,output:$installOutput},
        browserBootEntry:{id:$plugin},
        browserFeature:{name:$featureName,selector:$featureSelector,visible:true},
        update:{installedVersion:$updateVersion,output:$updateOutput},
        remove:{dependencyAbsent:true,bundleAbsent:true,moduleAbsent:true,output:$removeOutput},
        failedInstallRollback:{exitCode:$failureStatus,moduleAbsent:true,output:$failureOutput,manifestBeforeSha256:$manifestBeforeSha256,manifestAfterSha256:$manifestAfterSha256,lockfileBeforeSha256:$lockBeforeSha256,lockfileAfterSha256:$lockAfterSha256},
        gracefulResidue:{headless:0,browser:0,webHost:0,forcedCleanupRequired:false}
      }}' \
    > "$verification_evidence"
  exit 0
fi
for package in dsh-better-sidebar@0.16.1 dsh-dream-skin@8.30.1; do
  env "${profile_environment[@]}" "$binary" --data-dir "$compat_profile" plugin add "$package"
done


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
