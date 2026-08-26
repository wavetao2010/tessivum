#!/usr/bin/env bash
set -euo pipefail

if [[ $# -ne 8 ]]; then
  echo "usage: $0 <tag> <target> <binary> <compat-host-dir> <host-module-root> <vendor-dir> <agent-presets-dir> <output-dir>" >&2
  exit 2
fi

tag=$1
target=$2
binary=$3
compat_host=$4
host_modules=$5
vendor=$6
agent_presets=$7
output=$8
version=${tag#v}
script_dir=$(CDPATH= cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd)
host_module_manifest="$script_dir/../packaging/host-modules.json"
compat_modules="$script_dir/../compat/host-modules"
deepseek_root=$(CDPATH= cd -- "$agent_presets/../../../.." && pwd)

[[ $tag == v?* ]] || { echo "release tag must start with v and include a version: $tag" >&2; exit 2; }
case "$target" in
  x86_64-unknown-linux-gnu|aarch64-unknown-linux-gnu|x86_64-apple-darwin|aarch64-apple-darwin) ;;
  *) echo "unsupported release target: $target" >&2; exit 2 ;;
esac
[[ -x $binary ]] || { echo "release binary is not executable: $binary" >&2; exit 2; }
[[ -f $compat_host/src/index.ts ]] || { echo "compat host entry is missing: $compat_host/src/index.ts" >&2; exit 2; }
[[ -f $vendor/cordis/lib/index.js && -f $vendor/cosmokit/lib/index.js && -f $vendor/loader/lib/index.js ]] || { echo "compiled Cordis vendor entries are missing under: $vendor" >&2; exit 2; }
for preset in standard code minimal cordis; do
  [[ -f $agent_presets/$preset/agent.cordis.yml && -f $agent_presets/$preset/preset.yml ]] || {
    echo "shipped agent preset is incomplete: $agent_presets/$preset" >&2
    exit 2
  }
done
[[ -f $deepseek_root/LICENSE ]] || { echo "DeepSeek Harness license is missing: $deepseek_root/LICENSE" >&2; exit 2; }
[[ -f $host_module_manifest ]] || { echo "Host module metadata manifest is missing: $host_module_manifest" >&2; exit 2; }
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
  "$stage/share/tessivum/host-modules" "$stage/share/tessivum/vendor" \
  "$stage/share/tessivum/agent-presets" "$stage/share/licenses/deepseek-harness" \
  "$stage/share/licenses/@deepseek-ai-dsh-settings-0.1.0-rc.7" \
  "$stage/share/licenses/@deepseek-ai-schemastery-3.18.1"

cp "$binary" "$stage/libexec/tessivum"
cp LICENSE "$stage/LICENSE"
cp "$compat_host/package.json" "$compat_host/bun.lock" "$stage/share/tessivum/compat-host/"
cp -R "$compat_host/src" "$stage/share/tessivum/compat-host/"
cp -R "$host_modules"/. "$stage/share/tessivum/host-modules/"
cp -R "$compat_modules"/. "$stage/share/tessivum/host-modules/"
cp -R "$vendor"/. "$stage/share/tessivum/vendor/"
mkdir -p "$stage/share/tessivum/vendor/node_modules/@deepseek-ai"
ln -s ../../cordis "$stage/share/tessivum/vendor/node_modules/@deepseek-ai/cordis"
ln -s ../../cosmokit "$stage/share/tessivum/vendor/node_modules/@deepseek-ai/cosmokit"
ln -s ../../loader "$stage/share/tessivum/vendor/node_modules/@deepseek-ai/cordis-plugin-loader"
mkdir -p "$stage/share/tessivum/host-modules/node_modules/@deepseek-ai"
ln -s ../../../vendor/cordis "$stage/share/tessivum/host-modules/node_modules/@deepseek-ai/cordis"
ln -s ../../../vendor/cosmokit "$stage/share/tessivum/host-modules/node_modules/@deepseek-ai/cosmokit"
cp -R "$agent_presets"/. "$stage/share/tessivum/agent-presets/"
cp "$deepseek_root/LICENSE" "$stage/share/licenses/deepseek-harness/LICENSE"
cp "$host_modules/@deepseek-ai/dsh-settings/LICENSE" \
  "$stage/share/licenses/@deepseek-ai-dsh-settings-0.1.0-rc.7/LICENSE"
cp "$host_modules/@deepseek-ai/schemastery/LICENSE" \
  "$stage/share/licenses/@deepseek-ai-schemastery-3.18.1/LICENSE"

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
: "${CORDIS_VENDOR_ROOT:=$root/share/tessivum/vendor}"
: "${TESSIVUM_AGENT_PRESET_ROOT:=$root/share/tessivum/agent-presets}"
export TESSIVUM_COMPAT_HOST TESSIVUM_HOST_MODULE_ROOT CORDIS_VENDOR_ROOT TESSIVUM_AGENT_PRESET_ROOT
exec "$root/libexec/tessivum" "$@"
LAUNCHER
chmod +x "$stage/bin/tessivum"

cat > "$stage/README.txt" <<EOF
Tessivum $version ($target)

Run from the unpacked archive:
  ./bin/tessivum --version
  ./bin/tessivum web

The launcher points Agent Presets, Legacy Node compatibility, and the pinned
Cordis vendor at the packaged assets. Bun 1.3.14+ is needed only when Legacy
Node plugins run; pnpm is needed only by plugin add/remove. The Web shell is
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
