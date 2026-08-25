#!/usr/bin/env bash
set -euo pipefail

if [[ $# -ne 6 ]]; then
  echo "usage: $0 <tag> <target> <binary> <compat-host-dir> <vendor-dir> <output-dir>" >&2
  exit 2
fi

tag=$1
target=$2
binary=$3
compat_host=$4
vendor=$5
output=$6
version=${tag#v}

[[ $tag == v* ]] || { echo "release tag must start with v: $tag" >&2; exit 2; }
[[ -x $binary ]] || { echo "release binary is not executable: $binary" >&2; exit 2; }
[[ -f $compat_host/src/index.ts ]] || { echo "compat host entry is missing: $compat_host/src/index.ts" >&2; exit 2; }
[[ -f $vendor/cordis/src/index.ts && -f $vendor/cosmokit/src/index.ts ]] || { echo "Cordis vendor sources are missing under: $vendor" >&2; exit 2; }
[[ $("$binary" --version) == "tessivum $version" ]] || {
  echo "binary version does not match release tag $tag" >&2
  exit 2
}

name="tessivum-$version-$target"
stage="$output/$name"
rm -rf "$stage"
mkdir -p "$stage/bin" "$stage/libexec" "$stage/share/tessivum/compat-host" \
  "$stage/share/tessivum/vendor" "$stage/share/licenses"

cp "$binary" "$stage/libexec/tessivum"
cp LICENSE "$stage/LICENSE"
cp "$compat_host/package.json" "$compat_host/bun.lock" "$stage/share/tessivum/compat-host/"
cp -R "$compat_host/src" "$stage/share/tessivum/compat-host/"
cp -R "$vendor"/. "$stage/share/tessivum/vendor/"

cat > "$stage/bin/tessivum" <<'LAUNCHER'
#!/usr/bin/env sh
set -eu
root=$(CDPATH= cd -- "$(dirname -- "$0")/.." && pwd)
: "${TESSIVUM_COMPAT_HOST:=$root/share/tessivum/compat-host/src/index.ts}"
: "${CORDIS_VENDOR_ROOT:=$root/share/tessivum/vendor}"
export TESSIVUM_COMPAT_HOST CORDIS_VENDOR_ROOT
exec "$root/libexec/tessivum" "$@"
LAUNCHER
chmod +x "$stage/bin/tessivum"

cat > "$stage/README.txt" <<EOF
Tessivum $version ($target)

Run from the unpacked archive:
  ./bin/tessivum --version
  ./bin/tessivum web

The launcher points Legacy Node compatibility at the packaged host and pinned
Cordis vendor. Bun 1.3.14+ is needed only when Legacy Node plugins run; npm is
needed only by plugin add/remove. The Web shell is embedded in the executable.

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
