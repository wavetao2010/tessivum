#!/usr/bin/env bash
set -euo pipefail

if [[ $# -ne 8 ]]; then
  echo "usage: $0 <tag> <target> <binary> <compat-host-dir> <host-module-root> <vendor-dir> <market-tgz> <output-dir>" >&2
  exit 2
fi

tag=$1
target=$2
binary=$3
compat_host=$4
host_modules=$5
vendor=$6
market_tgz=$7
output=$8
version=${tag#v}
script_dir=$(CDPATH= cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd)
host_module_manifest="$script_dir/../packaging/host-modules.json"
market_source_inventory="$script_dir/../packaging/market-source.json"
market_license="$script_dir/../packaging/licenses/dsh-market/LICENSE"
compat_modules="$script_dir/../compat/host-modules"
deepseek_root=$(CDPATH= cd -- "$vendor/.." && pwd)
market_filename="tessivum-market-$version.tgz"
market_checksum="$market_tgz.sha256"

[[ $tag == v?* ]] || { echo "release tag must start with v and include a version: $tag" >&2; exit 2; }
case "$target" in
  x86_64-unknown-linux-gnu|aarch64-unknown-linux-gnu|x86_64-apple-darwin|aarch64-apple-darwin) ;;
  *) echo "unsupported release target: $target" >&2; exit 2 ;;
esac
[[ -x $binary ]] || { echo "release binary is not executable: $binary" >&2; exit 2; }
[[ -f $compat_host/src/index.ts ]] || { echo "compat host entry is missing: $compat_host/src/index.ts" >&2; exit 2; }
[[ -f $vendor/cordis/lib/index.js && -f $vendor/cosmokit/lib/index.js && -f $vendor/loader/lib/index.js ]] || { echo "compiled Cordis vendor entries are missing under: $vendor" >&2; exit 2; }
[[ -f $deepseek_root/LICENSE ]] || { echo "DeepSeek Harness license is missing: $deepseek_root/LICENSE" >&2; exit 2; }
[[ -f $host_module_manifest ]] || { echo "Host module metadata manifest is missing: $host_module_manifest" >&2; exit 2; }
[[ -f $market_source_inventory ]] || { echo "market source inventory is missing: $market_source_inventory" >&2; exit 2; }
[[ -f $market_license ]] || { echo "market upstream license is missing: $market_license" >&2; exit 2; }
[[ -f $market_tgz ]] || { echo "market package is missing: $market_tgz" >&2; exit 2; }
[[ $(basename "$market_tgz") == "$market_filename" ]] || { echo "market package filename must be $market_filename" >&2; exit 2; }
[[ -f $market_checksum ]] || { echo "market package checksum is missing: $market_checksum" >&2; exit 2; }
market_sha256=$(if command -v sha256sum >/dev/null 2>&1; then sha256sum "$market_tgz"; else shasum -a 256 "$market_tgz"; fi | awk '{print $1}')
[[ $(tr -d '\r\n' < "$market_checksum") == "$market_sha256  $market_filename" ]] || {
  echo "market package checksum does not match: $market_checksum" >&2
  exit 2
}
market_provenance=$(jq -ce --arg version "$version" '
  if . == {
    "format": 1,
    "package": {"name": "tessivum-market", "version": $version, "license": "MIT"},
    "upstream": {
      "repository": "https://github.com/dsh-market/dsh-market",
      "version": "1.38.1",
      "commit": "df2a16b1ed2dfaf1f2505e184e738c0d6d428945",
      "tarballIntegrity": "sha512-Z9VleLtCXwk5OlbSJKayWtbMaKACL8JUMyb/JHpErS4N3q//GJS+cgOhhxNkZYmXxB8/lv9IbhX1CBzlMhJeJg==",
      "license": "MIT"
    }
  } then .upstream else error("invalid market source inventory") end
' "$market_source_inventory") || { echo "market source inventory is invalid: $market_source_inventory" >&2; exit 2; }
market_manifest=$(tar -xOzf "$market_tgz" package/package.json) || { echo "market package manifest is missing" >&2; exit 2; }
market_upstream=$(tar -xOzf "$market_tgz" package/UPSTREAM.json) || { echo "market package provenance is missing" >&2; exit 2; }
jq -e --arg version "$version" --argjson provenance "$market_provenance" '
  .name == "tessivum-market"
  and .version == $version
  and .license == "MIT"
  and .tessivum.provenance == $provenance
' <<<"$market_manifest" >/dev/null || { echo "market package manifest provenance is invalid" >&2; exit 2; }
jq -e --argjson provenance "$market_provenance" '. == $provenance' <<<"$market_upstream" >/dev/null || {
  echo "market package provenance does not match the source inventory" >&2
  exit 2
}
tar -xOzf "$market_tgz" package/LICENSE.upstream | cmp -s - "$market_license" || {
  echo "market package upstream license does not match" >&2
  exit 2
}
python3 "$script_dir/fetch_host_modules.py" "$host_module_manifest" "$host_modules" --verify
[[ -f $compat_modules/@deepseek-ai/dsh-tools/index.js && -f $compat_modules/@deepseek-ai/dsh-llm/index.js && -f $compat_modules/@deepseek-ai/dsh-subagent/descriptor.js ]] || {
  echo "Host module compatibility sources are incomplete: $compat_modules" >&2
  exit 2
}
[[ $("$binary" --version) == "tessivum $version" ]] || {
  echo "binary version does not match release tag $tag" >&2
  exit 2
}

name="tessivum-$version-$target"
stage="$output/$name"
rm -rf "$stage"
mkdir -p "$stage/bin" "$stage/libexec" "$stage/share/tessivum/compat-host" \
  "$stage/share/tessivum/host-modules" "$stage/share/tessivum/plugins" "$stage/share/tessivum/vendor" \
  "$stage/share/licenses/deepseek-harness" "$stage/share/licenses/tessivum-market-$version" \
  "$stage/share/licenses/@deepseek-ai-dsh-settings-0.1.0-rc.7" \
  "$stage/share/licenses/@deepseek-ai-schemastery-3.18.1"

cp "$binary" "$stage/libexec/tessivum"
cp LICENSE "$stage/LICENSE"
cp "$compat_host/package.json" "$compat_host/bun.lock" "$stage/share/tessivum/compat-host/"
cp -R "$compat_host/src" "$stage/share/tessivum/compat-host/"
cp -R "$host_modules"/. "$stage/share/tessivum/host-modules/"
cp -R "$compat_modules"/. "$stage/share/tessivum/host-modules/"
cp -R "$vendor"/. "$stage/share/tessivum/vendor/"
cp "$market_tgz" "$stage/share/tessivum/plugins/$market_filename"
cp "$market_checksum" "$stage/share/tessivum/plugins/$market_filename.sha256"
cp "$market_source_inventory" "$stage/share/tessivum/plugins/$market_filename.source.json"
mkdir -p "$stage/share/tessivum/vendor/node_modules/@deepseek-ai"
ln -s ../../cordis "$stage/share/tessivum/vendor/node_modules/@deepseek-ai/cordis"
ln -s ../../cosmokit "$stage/share/tessivum/vendor/node_modules/@deepseek-ai/cosmokit"
ln -s ../../loader "$stage/share/tessivum/vendor/node_modules/@deepseek-ai/cordis-plugin-loader"
mkdir -p "$stage/share/tessivum/host-modules/node_modules/@deepseek-ai"
ln -s ../../../vendor/cordis "$stage/share/tessivum/host-modules/node_modules/@deepseek-ai/cordis"
ln -s ../../../vendor/cosmokit "$stage/share/tessivum/host-modules/node_modules/@deepseek-ai/cosmokit"
cp "$deepseek_root/LICENSE" "$stage/share/licenses/deepseek-harness/LICENSE"
cp "$host_modules/@deepseek-ai/dsh-settings/LICENSE" \
  "$stage/share/licenses/@deepseek-ai-dsh-settings-0.1.0-rc.7/LICENSE"
cp "$host_modules/@deepseek-ai/schemastery/LICENSE" \
  "$stage/share/licenses/@deepseek-ai-schemastery-3.18.1/LICENSE"
cp "$market_license" "$stage/share/licenses/tessivum-market-$version/LICENSE"

cat > "$stage/bin/tessivum" <<'LAUNCHER'
#!/usr/bin/env sh
set -eu
launcher=$0
while [ -L "$launcher" ]; do
  launcher_dir=$(CDPATH= cd -- "$(dirname -- "$launcher")" && pwd)
  launcher_link=$(readlink "$launcher")
  case "$launcher_link" in
    /*) launcher=$launcher_link ;;
    *) launcher=$launcher_dir/$launcher_link ;;
  esac
done
root=$(CDPATH= cd -- "$(dirname -- "$launcher")/.." && pwd)
: "${TESSIVUM_COMPAT_HOST:=$root/share/tessivum/compat-host/src/index.ts}"
: "${TESSIVUM_HOST_MODULE_ROOT:=$root/share/tessivum/host-modules}"
: "${TESSIVUM_MARKET_TARBALL:=$root/share/tessivum/plugins/tessivum-market-0.1.0-alpha.17.tgz}"
: "${TESSIVUM_MARKET_SHA256_FILE:=$root/share/tessivum/plugins/tessivum-market-0.1.0-alpha.17.tgz.sha256}"
: "${CORDIS_VENDOR_ROOT:=$root/share/tessivum/vendor}"
export TESSIVUM_COMPAT_HOST TESSIVUM_HOST_MODULE_ROOT TESSIVUM_MARKET_TARBALL TESSIVUM_MARKET_SHA256_FILE CORDIS_VENDOR_ROOT
exec "$root/libexec/tessivum" "$@"
LAUNCHER
chmod +x "$stage/bin/tessivum"
ln -s tessivum "$stage/bin/tsv"

cat > "$stage/README.txt" <<EOF
Tessivum $version ($target)

Run from the unpacked archive:
  ./bin/tessivum --version
  ./bin/tessivum web

The launcher points Legacy Node compatibility and the pinned Cordis vendor at
the packaged assets. Native modes are built into Tessivum; user modes live in
the selected data directory under modes/. Bun 1.3.14+ is required for PTC and
Legacy Node plugins; pnpm is needed only by plugin add/remove. The Web shell is
embedded in the executable.

This archive is not code-signed or notarized. Verify its adjacent SHA-256 file
before use. Source and documentation: https://github.com/wavetao2010/tessivum
EOF

cargo metadata --locked --format-version 1 \
  | jq -r '.packages[] | [.name, .version, .manifest_path, (.license // "UNKNOWN"), (.repository // "")] | @tsv' \
  | while IFS=$'\t' read -r package package_version manifest license repository; do
      [[ $package == tessivum ]] && continue
      destination="$stage/share/licenses/$package-$package_version"
      mkdir -p "$destination"
      package_dir=$(dirname "$manifest")
      found=false
      for source_dir in "$package_dir" "$(dirname "$package_dir")" "$(dirname "$(dirname "$package_dir")")"; do
        for candidate in "$source_dir"/LICENSE* "$source_dir"/COPYING* "$source_dir"/NOTICE*; do
          [[ -f $candidate ]] || continue
          cp "$candidate" "$destination/$(basename "$candidate")"
          found=true
        done
        $found && break
      done
      if ! $found; then
        printf 'Package: %s %s\nSPDX license: %s\nRepository: %s\n' \
          "$package" "$package_version" "$license" "$repository" > "$destination/METADATA.txt"
      fi
    done

archive="$output/$name.tar.gz"
tar -C "$output" -czf "$archive" "$name"
(
  cd "$output"
  if command -v sha256sum >/dev/null 2>&1; then
    sha256sum "$(basename "$archive")" > "$(basename "$archive").sha256"
  else
    shasum -a 256 "$(basename "$archive")" > "$(basename "$archive").sha256"
  fi
)
printf '%s\n' "$archive"
