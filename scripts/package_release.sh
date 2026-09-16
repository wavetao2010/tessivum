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
sidebar_patch="$script_dir/../packaging/patches/dsh-better-sidebar-0.17.1.patch"
sidebar_patch_sha256="e7cdcbc409fe3a211acf7d5098066d3c6da9b84d3bb5b52cc7e34a2dbab15e65"
compat_modules="$script_dir/../compat/host-modules"
deepseek_root=$(CDPATH= cd -- "$vendor/.." && pwd)
market_filename="tessivum-market-$version.tgz"
market_checksum="$market_tgz.sha256"

[[ $tag == v?* ]] || { echo "release tag must start with v and include a version: $tag" >&2; exit 2; }
is_windows=0
case "$target" in
  x86_64-unknown-linux-gnu|aarch64-unknown-linux-gnu|x86_64-apple-darwin|aarch64-apple-darwin) ;;
  x86_64-pc-windows-msvc) is_windows=1 ;;
  *) echo "unsupported release target: $target" >&2; exit 2 ;;
esac
if (( is_windows )); then
  [[ $(basename "$binary") == tessivum.exe ]] || { echo "Windows release binary must be tessivum.exe: $binary" >&2; exit 2; }
  [[ -f $binary ]] || { echo "Windows release binary is missing: $binary" >&2; exit 2; }
  command -v cygpath >/dev/null 2>&1 || { echo "Windows packaging requires Git Bash cygpath" >&2; exit 2; }
  # GNU tar treats a drive-letter colon as a remote host unless paths use MSYS form.
  binary=$(cygpath -u "$binary")
  compat_host=$(cygpath -u "$compat_host")
  host_modules=$(cygpath -u "$host_modules")
  vendor=$(cygpath -u "$vendor")
  market_tgz=$(cygpath -u "$market_tgz")
  output=$(cygpath -u "$output")
  market_checksum="$market_tgz.sha256"
else
  [[ -x $binary ]] || { echo "release binary is not executable: $binary" >&2; exit 2; }
fi
if (( is_windows )); then
  command -v python >/dev/null 2>&1 || { echo "Windows packaging requires a working Python command" >&2; exit 2; }
  python_bin=python
elif command -v python3 >/dev/null 2>&1; then
  python_bin=python3
elif command -v python >/dev/null 2>&1; then
  python_bin=python
else
  echo "Python 3 is required to package a release" >&2
  exit 2
fi
[[ -f $compat_host/src/index.ts ]] || { echo "compat host entry is missing: $compat_host/src/index.ts" >&2; exit 2; }
[[ -f $vendor/cordis/lib/index.js && -f $vendor/cosmokit/lib/index.js && -f $vendor/loader/lib/index.js ]] || { echo "compiled Cordis vendor entries are missing under: $vendor" >&2; exit 2; }
[[ -f $deepseek_root/LICENSE ]] || { echo "DeepSeek Harness license is missing: $deepseek_root/LICENSE" >&2; exit 2; }
[[ -f $host_module_manifest ]] || { echo "Host module metadata manifest is missing: $host_module_manifest" >&2; exit 2; }
[[ -f $market_source_inventory ]] || { echo "market source inventory is missing: $market_source_inventory" >&2; exit 2; }
[[ -f $market_license ]] || { echo "market upstream license is missing: $market_license" >&2; exit 2; }
[[ -f $sidebar_patch ]] || { echo "fixed sidebar patch is missing: $sidebar_patch" >&2; exit 2; }
actual_sidebar_patch_sha256=$(if command -v sha256sum >/dev/null 2>&1; then sha256sum "$sidebar_patch"; else shasum -a 256 "$sidebar_patch"; fi | awk '{print $1}')
[[ $actual_sidebar_patch_sha256 == "$sidebar_patch_sha256" ]] || { echo "fixed sidebar patch checksum does not match: $sidebar_patch" >&2; exit 2; }
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
"$python_bin" "$script_dir/fetch_host_modules.py" "$host_module_manifest" "$host_modules" --verify
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
  "$stage/share/tessivum/host-modules" "$stage/share/tessivum/patches" "$stage/share/tessivum/plugins" "$stage/share/tessivum/vendor" \
  "$stage/share/licenses/deepseek-harness" "$stage/share/licenses/tessivum-market-$version" \
  "$stage/share/licenses/@deepseek-ai-dsh-settings-0.1.0-rc.7" \
  "$stage/share/licenses/@deepseek-ai-schemastery-3.18.1"

binary_name=tessivum
if (( is_windows )); then
  binary_name=tessivum.exe
fi
cp "$binary" "$stage/libexec/$binary_name"
cp LICENSE "$stage/LICENSE"
cp "$compat_host/package.json" "$compat_host/bun.lock" "$stage/share/tessivum/compat-host/"
if (( is_windows )); then
  cp -RL "$compat_host/src" "$stage/share/tessivum/compat-host/"
  cp -RL "$host_modules"/. "$stage/share/tessivum/host-modules/"
  cp -RL "$compat_modules"/. "$stage/share/tessivum/host-modules/"
  "$python_bin" - "$vendor" "$stage/share/tessivum/vendor" <<'PY'
import shutil
import sys

shutil.copytree(sys.argv[1], sys.argv[2], dirs_exist_ok=True,
                ignore=shutil.ignore_patterns("node_modules"))
PY
else
  cp -R "$compat_host/src" "$stage/share/tessivum/compat-host/"
  cp -R "$host_modules"/. "$stage/share/tessivum/host-modules/"
  cp -R "$compat_modules"/. "$stage/share/tessivum/host-modules/"
  cp -R "$vendor"/. "$stage/share/tessivum/vendor/"
fi
cp "$market_tgz" "$stage/share/tessivum/plugins/$market_filename"
cp "$market_checksum" "$stage/share/tessivum/plugins/$market_filename.sha256"
cp "$market_source_inventory" "$stage/share/tessivum/plugins/$market_filename.source.json"
cp "$sidebar_patch" "$stage/share/tessivum/patches/dsh-better-sidebar-0.17.1.patch"
mkdir -p "$stage/share/tessivum/vendor/node_modules/@deepseek-ai"
mkdir -p "$stage/share/tessivum/host-modules/node_modules/@deepseek-ai"
if (( is_windows )); then
  cp -RL "$stage/share/tessivum/vendor/cordis" "$stage/share/tessivum/vendor/node_modules/@deepseek-ai/"
  cp -RL "$stage/share/tessivum/vendor/cosmokit" "$stage/share/tessivum/vendor/node_modules/@deepseek-ai/"
  cp -RL "$stage/share/tessivum/vendor/loader" "$stage/share/tessivum/vendor/node_modules/@deepseek-ai/cordis-plugin-loader"
  cp -RL "$stage/share/tessivum/vendor/cordis" "$stage/share/tessivum/host-modules/node_modules/@deepseek-ai/"
  cp -RL "$stage/share/tessivum/vendor/cosmokit" "$stage/share/tessivum/host-modules/node_modules/@deepseek-ai/"
else
  ln -s ../../cordis "$stage/share/tessivum/vendor/node_modules/@deepseek-ai/cordis"
  ln -s ../../cosmokit "$stage/share/tessivum/vendor/node_modules/@deepseek-ai/cosmokit"
  ln -s ../../loader "$stage/share/tessivum/vendor/node_modules/@deepseek-ai/cordis-plugin-loader"
  ln -s ../../../vendor/cordis "$stage/share/tessivum/host-modules/node_modules/@deepseek-ai/cordis"
  ln -s ../../../vendor/cosmokit "$stage/share/tessivum/host-modules/node_modules/@deepseek-ai/cosmokit"
fi
cp "$deepseek_root/LICENSE" "$stage/share/licenses/deepseek-harness/LICENSE"
cp "$host_modules/@deepseek-ai/dsh-settings/LICENSE" \
  "$stage/share/licenses/@deepseek-ai-dsh-settings-0.1.0-rc.7/LICENSE"
cp "$host_modules/@deepseek-ai/schemastery/LICENSE" \
  "$stage/share/licenses/@deepseek-ai-schemastery-3.18.1/LICENSE"
cp "$market_license" "$stage/share/licenses/tessivum-market-$version/LICENSE"

if (( is_windows )); then
  "$python_bin" - "$stage/bin" "$market_filename" <<'PY'
from pathlib import Path
import sys

bin_dir = Path(sys.argv[1])
market_filename = sys.argv[2]
lines = [
    "@echo off",
    "setlocal EnableExtensions DisableDelayedExpansion",
    'for %%I in ("%~dp0..") do set "TESSIVUM_ROOT=%%~fI"',
    'if not defined TESSIVUM_COMPAT_HOST set "TESSIVUM_COMPAT_HOST=%TESSIVUM_ROOT%\\share\\tessivum\\compat-host\\src\\index.ts"',
    'if not defined TESSIVUM_HOST_MODULE_ROOT set "TESSIVUM_HOST_MODULE_ROOT=%TESSIVUM_ROOT%\\share\\tessivum\\host-modules"',
    rf'if not defined TESSIVUM_MARKET_TARBALL set "TESSIVUM_MARKET_TARBALL=%TESSIVUM_ROOT%\share\tessivum\plugins\{market_filename}"',
    rf'if not defined TESSIVUM_MARKET_SHA256_FILE set "TESSIVUM_MARKET_SHA256_FILE=%TESSIVUM_ROOT%\share\tessivum\plugins\{market_filename}.sha256"',
    rf'if not defined TESSIVUM_MARKET_SOURCE_FILE set "TESSIVUM_MARKET_SOURCE_FILE=%TESSIVUM_ROOT%\share\tessivum\plugins\{market_filename}.source.json"',
    'if not defined CORDIS_VENDOR_ROOT set "CORDIS_VENDOR_ROOT=%TESSIVUM_ROOT%\\share\\tessivum\\vendor"',
    '"%TESSIVUM_ROOT%\\libexec\\tessivum.exe" %*',
    "exit /b %ERRORLEVEL%",
]
contents = ("\r\n".join(lines) + "\r\n").encode("utf-8")
for launcher in ("tessivum.cmd", "tsv.cmd"):
    (bin_dir / launcher).write_bytes(contents)
PY
else
  sed "s|@MARKET_FILENAME@|$market_filename|g" > "$stage/bin/tessivum" <<'LAUNCHER'
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
: "${TESSIVUM_MARKET_TARBALL:=$root/share/tessivum/plugins/@MARKET_FILENAME@}"
: "${TESSIVUM_MARKET_SHA256_FILE:=$root/share/tessivum/plugins/@MARKET_FILENAME@.sha256}"
: "${TESSIVUM_MARKET_SOURCE_FILE:=$root/share/tessivum/plugins/@MARKET_FILENAME@.source.json}"
: "${CORDIS_VENDOR_ROOT:=$root/share/tessivum/vendor}"
export TESSIVUM_COMPAT_HOST TESSIVUM_HOST_MODULE_ROOT TESSIVUM_MARKET_TARBALL TESSIVUM_MARKET_SHA256_FILE TESSIVUM_MARKET_SOURCE_FILE CORDIS_VENDOR_ROOT
exec "$root/libexec/tessivum" "$@"
LAUNCHER
  chmod +x "$stage/bin/tessivum"
  ln -s tessivum "$stage/bin/tsv"
fi

if (( is_windows )); then
  launcher_command='.\bin\tessivum.cmd'
else
  launcher_command='./bin/tessivum'
fi

cat > "$stage/README.txt" <<EOF
Tessivum $version ($target)

Run from the unpacked archive:
  $launcher_command --version
  $launcher_command web

The launcher points Legacy Node compatibility and the pinned Cordis vendor at
the packaged assets. Native modes are built into Tessivum; user modes live in
the selected data directory under modes/. Bun 1.3.14+ is required for PTC and
Legacy Node plugins; pnpm is needed only by plugin add/remove. The Web shell is
embedded in the executable.

Sidebar terminals are local-only. Paired Remote Access devices cannot access
Legacy plugin HTTP routes or WebSocket upgrades.

This archive is not code-signed or notarized. Verify its adjacent SHA-256 file
before use. Source and documentation: https://github.com/wavetao2010/tessivum
EOF

cargo metadata --locked --format-version 1 \
  | jq -r '.packages[] | [.name, .version, .manifest_path, (.license // "UNKNOWN"), (.repository // "")] | @tsv' \
  | while IFS=$'\t' read -r package package_version manifest license repository; do
      [[ $package == tessivum ]] && continue
      destination="$stage/share/licenses/$package-$package_version"
      mkdir -p "$destination"
      if (( is_windows )); then
        manifest=$(cygpath -u "$manifest")
      fi

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

if (( is_windows )); then
  archive="$output/$name.zip"
  "$python_bin" - "$stage" "$archive" "$name" <<'PY'
from __future__ import annotations

import os
from pathlib import Path
import stat
import sys
import zipfile

stage = Path(sys.argv[1])
archive = Path(sys.argv[2])
name = sys.argv[3]


def fail(message: str) -> None:
    raise SystemExit(f"Windows release package: {message}")


def is_link_or_reparse(path: Path) -> bool:
    metadata = os.lstat(path)
    attributes = getattr(metadata, "st_file_attributes", 0)
    reparse_point = getattr(stat, "FILE_ATTRIBUTE_REPARSE_POINT", 0x0400)
    return stat.S_ISLNK(metadata.st_mode) or bool(attributes & reparse_point)


def assert_no_links(root: Path) -> None:
    failures: list[str] = []
    if is_link_or_reparse(root):
        failures.append(".")
    for directory, directories, files in os.walk(root, topdown=True, followlinks=False):
        current = Path(directory)
        for child in [*directories, *files]:
            candidate = current / child
            if is_link_or_reparse(candidate):
                failures.append(candidate.relative_to(root).as_posix())
                if child in directories:
                    directories.remove(child)
    if failures:
        fail("payload contains symlink or reparse point: " + ", ".join(sorted(failures)))


if not stage.is_dir() or stage.name != name:
    fail(f"staging root is not {name}: {stage}")
assert_no_links(stage)
archive.parent.mkdir(parents=True, exist_ok=True)
with zipfile.ZipFile(archive, "w", compression=zipfile.ZIP_DEFLATED) as package:
    package.write(stage, arcname=f"{name}/")
    for path in sorted(stage.rglob("*"), key=lambda value: value.relative_to(stage).as_posix()):
        if is_link_or_reparse(path):
            fail(f"payload changed into a symlink or reparse point: {path}")
        arcname = f"{name}/{path.relative_to(stage).as_posix()}"
        if path.is_dir():
            package.write(path, arcname=f"{arcname}/")
        elif path.is_file():
            package.write(path, arcname=arcname)
        else:
            fail(f"payload entry is neither a file nor directory: {path}")

import hashlib

digest = hashlib.sha256(archive.read_bytes()).hexdigest()
archive.with_name(f"{archive.name}.sha256").write_bytes(
    f"{digest}  {archive.name}\n".encode("utf-8")
)
PY

  smoke_parent="$output/Windows 包 smoke"
  rm -rf "$smoke_parent"
  mkdir -p "$smoke_parent"
  "$python_bin" - "$archive" "$smoke_parent" "$name" "$market_filename" "$version" <<'PY'
from __future__ import annotations

import hashlib
import json
import os
from pathlib import Path, PurePosixPath
import re
import stat
import subprocess
import sys
import zipfile

archive = Path(sys.argv[1])
smoke_parent = Path(sys.argv[2])
name = sys.argv[3]
market_filename = sys.argv[4]
version = sys.argv[5]


def fail(message: str) -> None:
    raise SystemExit(f"Windows release package validation: {message}")


def is_link_or_reparse(path: Path) -> bool:
    metadata = os.lstat(path)
    attributes = getattr(metadata, "st_file_attributes", 0)
    reparse_point = getattr(stat, "FILE_ATTRIBUTE_REPARSE_POINT", 0x0400)
    return stat.S_ISLNK(metadata.st_mode) or bool(attributes & reparse_point)


def assert_no_links(root: Path) -> None:
    failures: list[str] = []
    if is_link_or_reparse(root):
        failures.append(".")
    for directory, directories, files in os.walk(root, topdown=True, followlinks=False):
        current = Path(directory)
        for child in [*directories, *files]:
            candidate = current / child
            if is_link_or_reparse(candidate):
                failures.append(candidate.relative_to(root).as_posix())
                if child in directories:
                    directories.remove(child)
    if failures:
        fail("extracted payload contains symlink or reparse point: " + ", ".join(sorted(failures)))


def safe_relative(value: object, label: str) -> PurePosixPath:
    if not isinstance(value, str) or not value or "\\" in value:
        fail(f"invalid {label}: {value!r}")
    path = PurePosixPath(value)
    if path.is_absolute() or any(part in ("", ".", "..") for part in path.parts):
        fail(f"unsafe {label}: {value!r}")
    return path


def assert_inventory(root: Path) -> None:
    required = [
        "LICENSE",
        "README.txt",
        "bin/tessivum.cmd",
        "bin/tsv.cmd",
        "libexec/tessivum.exe",
        "share/tessivum/compat-host/package.json",
        "share/tessivum/compat-host/bun.lock",
        "share/tessivum/compat-host/src/index.ts",
        "share/tessivum/host-modules/INVENTORY.json",
        "share/tessivum/host-modules/@deepseek-ai/dsh-settings/LICENSE",
        "share/tessivum/host-modules/@deepseek-ai/schemastery/LICENSE",
        "share/tessivum/host-modules/@deepseek-ai/dsh-tools/index.js",
        "share/tessivum/host-modules/@deepseek-ai/dsh-llm/index.js",
        "share/tessivum/host-modules/@deepseek-ai/dsh-subagent/descriptor.js",
        "share/tessivum/vendor/cordis/lib/index.js",
        "share/tessivum/vendor/cosmokit/lib/index.js",
        "share/tessivum/vendor/loader/lib/index.js",
        "share/tessivum/vendor/node_modules/@deepseek-ai/cordis/lib/index.js",
        "share/tessivum/vendor/node_modules/@deepseek-ai/cosmokit/lib/index.js",
        "share/tessivum/vendor/node_modules/@deepseek-ai/cordis-plugin-loader/lib/index.js",
        "share/tessivum/host-modules/node_modules/@deepseek-ai/cordis/lib/index.js",
        "share/tessivum/host-modules/node_modules/@deepseek-ai/cosmokit/lib/index.js",
        f"share/tessivum/plugins/{market_filename}",
        f"share/tessivum/plugins/{market_filename}.sha256",
        f"share/tessivum/plugins/{market_filename}.source.json",
        "share/tessivum/patches/dsh-better-sidebar-0.17.1.patch",
        "share/licenses/deepseek-harness/LICENSE",
        "share/licenses/@deepseek-ai-dsh-settings-0.1.0-rc.7/LICENSE",
        "share/licenses/@deepseek-ai-schemastery-3.18.1/LICENSE",
        f"share/licenses/tessivum-market-{version}/LICENSE",
    ]
    missing = [relative for relative in required if not (root / relative).is_file()]
    if missing:
        fail("extracted payload is missing required inventory: " + ", ".join(missing))

    module_root = root / "share/tessivum/host-modules"
    inventory_path = module_root / "INVENTORY.json"
    try:
        inventory = json.loads(inventory_path.read_text(encoding="utf-8"))
    except (OSError, ValueError) as error:
        fail(f"cannot read copied host module inventory: {error}")
    packages = inventory.get("packages") if isinstance(inventory, dict) else None
    if not isinstance(packages, list) or not packages:
        fail("copied host module inventory has no packages")
    for package in packages:
        if not isinstance(package, dict):
            fail("copied host module inventory has a malformed package")
        package_path = safe_relative(package.get("name"), "package name")
        files = package.get("files")
        if not isinstance(files, list) or not files:
            fail(f"copied host module inventory has no files for {package_path}")
        for file_entry in files:
            if not isinstance(file_entry, dict):
                fail(f"copied host module inventory has a malformed file for {package_path}")
            relative = safe_relative(file_entry.get("path"), "inventory file path")
            expected = file_entry.get("sha256")
            if not isinstance(expected, str) or not re.fullmatch(r"[0-9a-f]{64}", expected):
                fail(f"copied host module inventory has an invalid digest for {package_path}/{relative}")
            copied = module_root.joinpath(*package_path.parts, *relative.parts)
            if not copied.is_file():
                fail(f"copied host module inventory file is missing: {package_path}/{relative}")
            actual = hashlib.sha256(copied.read_bytes()).hexdigest()
            if actual != expected:
                fail(f"copied host module inventory digest mismatch: {package_path}/{relative}")


def archive_member_parts(info: zipfile.ZipInfo) -> tuple[str, list[str]]:
    if "\\" in info.filename:
        fail(f"archive member uses a backslash path: {info.filename!r}")
    normalized = info.filename
    if not normalized or normalized.startswith("/") or normalized.startswith("//") or re.match(r"^[A-Za-z]:", normalized):
        fail(f"archive member has an absolute path: {info.filename!r}")
    if "//" in normalized:
        fail(f"archive member has an empty path component: {info.filename!r}")
    normalized = normalized.rstrip("/")
    if not normalized:
        fail(f"archive member has no path: {info.filename!r}")
    parts = normalized.split("/")
    if any(part in ("", ".", "..") for part in parts):
        fail(f"archive member has an unsafe path: {info.filename!r}")
    if parts[0] != name:
        fail(f"archive member escapes the expected root {name}: {info.filename!r}")
    if len(parts) == 1 and not info.is_dir():
        fail(f"archive root is not a directory: {info.filename!r}")
    mode = (info.external_attr >> 16) & 0xFFFF
    if stat.S_IFMT(mode) == stat.S_IFLNK:
        fail(f"archive member is a symlink: {info.filename!r}")
    return normalized, parts


if not archive.is_file():
    fail(f"archive is missing: {archive}")
checksum = archive.with_name(f"{archive.name}.sha256")
actual_checksum = hashlib.sha256(archive.read_bytes()).hexdigest()
if not checksum.is_file() or checksum.read_text(encoding="utf-8").strip("\r\n") != f"{actual_checksum}  {archive.name}":
    fail(f"archive checksum is invalid: {checksum}")

with zipfile.ZipFile(archive) as package:
    members = package.infolist()
    if not members:
        fail("archive is empty")
    seen: set[str] = set()
    roots: set[str] = set()
    for member in members:
        normalized, parts = archive_member_parts(member)
        if normalized in seen:
            fail(f"archive contains a duplicate member: {member.filename!r}")
        seen.add(normalized)
        roots.add(parts[0])
    if roots != {name}:
        fail(f"archive must contain exactly one root {name}, found {sorted(roots)}")
    package.extractall(smoke_parent)

root = (smoke_parent / name).resolve()
if not root.is_dir():
    fail(f"archive did not extract root {name}")
assert_no_links(root)
assert_inventory(root)

launcher_markers = [
    b'for %%I in ("%~dp0..")',
    b"TESSIVUM_COMPAT_HOST=%TESSIVUM_ROOT%\\share\\tessivum\\compat-host\\src\\index.ts",
    b"TESSIVUM_HOST_MODULE_ROOT=%TESSIVUM_ROOT%\\share\\tessivum\\host-modules",
    f"TESSIVUM_MARKET_TARBALL=%TESSIVUM_ROOT%\\share\\tessivum\\plugins\\{market_filename}".encode("utf-8"),
    f"TESSIVUM_MARKET_SHA256_FILE=%TESSIVUM_ROOT%\\share\\tessivum\\plugins\\{market_filename}.sha256".encode("utf-8"),
    f"TESSIVUM_MARKET_SOURCE_FILE=%TESSIVUM_ROOT%\\share\\tessivum\\plugins\\{market_filename}.source.json".encode("utf-8"),
    b"CORDIS_VENDOR_ROOT=%TESSIVUM_ROOT%\\share\\tessivum\\vendor",
    b'"%TESSIVUM_ROOT%\\libexec\\tessivum.exe" %*',
]
environment = os.environ.copy()
for variable in (
    "TESSIVUM_COMPAT_HOST",
    "TESSIVUM_HOST_MODULE_ROOT",
    "TESSIVUM_MARKET_TARBALL",
    "TESSIVUM_MARKET_SHA256_FILE",
    "TESSIVUM_MARKET_SOURCE_FILE",
    "CORDIS_VENDOR_ROOT",
):
    environment.pop(variable, None)
for launcher_name in ("tessivum.cmd", "tsv.cmd"):
    launcher = root / "bin" / launcher_name
    launcher_bytes = launcher.read_bytes()
    if not launcher_bytes.endswith(b"\r\n") or b"\n" in launcher_bytes.replace(b"\r\n", b""):
        fail(f"{launcher_name} does not use CRLF line endings")
    if any(marker not in launcher_bytes for marker in launcher_markers):
        fail(f"{launcher_name} does not set every packaged resource default")
    result = subprocess.run(
        f'cmd.exe /d /s /c ""{launcher}" --version"',
        cwd=root,
        env=environment,
        capture_output=True,
        text=True,
        encoding="utf-8",
        errors="replace",
    )
    if result.returncode != 0 or result.stdout.strip() != f"tessivum {version}":
        fail(
            f"{launcher_name} version smoke failed (exit {result.returncode}): "
            f"stdout={result.stdout!r} stderr={result.stderr!r}"
        )
PY
  rm -rf "$smoke_parent"
else
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
fi
printf '%s\n' "$archive"
