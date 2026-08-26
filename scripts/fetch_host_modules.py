#!/usr/bin/env python3
"""Fetch and verify the fixed Host-only npm modules without a package manager."""

from __future__ import annotations

import argparse
import base64
import hashlib
import io
import json
import os
import shutil
import stat
import sys
import tarfile
import tempfile
from pathlib import Path, PurePosixPath
from urllib.request import urlopen


class HostModuleError(RuntimeError):
    pass


def fail(message: str) -> None:
    raise HostModuleError(message)


def read_json(path: Path, label: str) -> dict:
    try:
        value = json.loads(path.read_text(encoding="utf-8"))
    except (OSError, UnicodeDecodeError, json.JSONDecodeError) as error:
        fail(f"cannot read {label} {path}: {error}")
    if not isinstance(value, dict):
        fail(f"{label} {path} must be a JSON object")
    return value


def relative_path(value: object, label: str) -> PurePosixPath:
    if not isinstance(value, str) or not value or "\\" in value or "\x00" in value:
        fail(f"{label} must be a non-empty portable relative path")
    path = PurePosixPath(value)
    if not path.parts or path.is_absolute() or any(part in ("", ".", "..") for part in path.parts):
        fail(f"{label} must be a non-traversing relative path: {value!r}")
    return path


def read_manifest(path: Path) -> list[dict]:
    manifest = read_json(path, "metadata manifest")
    packages = manifest.get("packages")
    if manifest.get("format") != 1 or not isinstance(packages, list) or len(packages) != 2:
        fail("metadata manifest must contain exactly two format-1 packages")
    expected_names = {"@deepseek-ai/dsh-settings", "@deepseek-ai/schemastery"}
    seen_names: set[str] = set()
    for package in packages:
        if not isinstance(package, dict):
            fail("metadata manifest package must be an object")
        name = package.get("name")
        if name not in expected_names or name in seen_names:
            fail(f"unexpected or duplicate metadata package: {name!r}")
        seen_names.add(name)
        for key in ("version", "url", "sri", "license", "licenseFile"):
            if not isinstance(package.get(key), str) or not package[key]:
                fail(f"metadata package {name} has invalid {key}")
        if not package["url"].startswith("https://registry.npmjs.org/"):
            fail(f"metadata package {name} must use the npm registry tarball URL")
        if package["license"] != "MIT":
            fail(f"metadata package {name} must declare MIT")
        relative_path(package["licenseFile"], f"metadata license file for {name}")
        entries = package.get("runtimeEntries")
        if not isinstance(entries, list) or not entries or any(not isinstance(entry, str) for entry in entries):
            fail(f"metadata package {name} must list runtime entries")
        for entry in entries:
            relative_path(entry, f"metadata runtime entry for {name}")
        sri_bytes(package["sri"])
    if seen_names != expected_names:
        fail("metadata manifest package set is incomplete")
    return packages


def sri_bytes(sri: str) -> bytes:
    algorithm, separator, encoded = sri.partition("-")
    if algorithm != "sha512" or not separator:
        fail(f"only sha512 SRI is supported: {sri!r}")
    try:
        digest = base64.b64decode(encoded, validate=True)
    except ValueError as error:
        fail(f"invalid SRI digest: {error}")
    if len(digest) != hashlib.sha512().digest_size:
        fail("sha512 SRI digest has the wrong length")
    return digest


def package_destination(root: Path, package: dict) -> Path:
    return root.joinpath(*package["name"].split("/"))


def validate_root_layout(root: Path, packages: list[dict]) -> None:
    scope = root / "@deepseek-ai"
    try:
        scope_mode = scope.lstat().st_mode
    except OSError as error:
        fail(f"missing @deepseek-ai module scope: {error}")
    if not stat.S_ISDIR(scope_mode):
        fail(f"host module scope must be a directory: {scope}")
    expected = {package["name"].split("/", 1)[1] for package in packages}
    actual = {entry.name for entry in scope.iterdir()}
    if actual != expected:
        fail("host module scope does not contain exactly the verified packages")
    for entry in scope.iterdir():
        if not stat.S_ISDIR(entry.lstat().st_mode):
            fail(f"host module package root must be a directory: {entry}")


def regular_file(path: Path, label: str) -> None:
    try:
        mode = path.lstat().st_mode
    except OSError as error:
        fail(f"missing {label}: {path} ({error})")
    if not stat.S_ISREG(mode):
        fail(f"{label} must be a regular file: {path}")


def package_files(root: Path, package: dict) -> list[PurePosixPath]:
    package_root = package_destination(root, package)
    try:
        mode = package_root.lstat().st_mode
    except OSError as error:
        fail(f"missing package root {package['name']}: {error}")
    if not stat.S_ISDIR(mode):
        fail(f"package root must be a directory: {package_root}")
    files: list[PurePosixPath] = []
    for current, directories, names in os.walk(package_root, followlinks=False):
        current_path = Path(current)
        for directory in directories:
            candidate = current_path / directory
            if not stat.S_ISDIR(candidate.lstat().st_mode):
                fail(f"package {package['name']} contains a non-directory entry: {candidate}")
        for name in names:
            candidate = current_path / name
            if not stat.S_ISREG(candidate.lstat().st_mode):
                fail(f"package {package['name']} contains a non-regular file: {candidate}")
            files.append(PurePosixPath(candidate.relative_to(package_root).as_posix()))
    return sorted(files, key=str)


def validate_package(root: Path, package: dict) -> list[PurePosixPath]:
    package_root = package_destination(root, package)
    package_json = package_root / "package.json"
    regular_file(package_json, f"package manifest for {package['name']}")
    manifest = read_json(package_json, f"package manifest for {package['name']}")
    for key in ("name", "version", "license"):
        if manifest.get(key) != package[key]:
            fail(
                f"package manifest {package_json} has {key}={manifest.get(key)!r}; "
                f"expected {package[key]!r}"
            )
    regular_file(package_root.joinpath(*relative_path(package["licenseFile"], "license file").parts), "package license")
    for entry in package["runtimeEntries"]:
        regular_file(package_root.joinpath(*relative_path(entry, "runtime entry").parts), "runtime entry")
    return package_files(root, package)


def file_hash(path: Path) -> str:
    digest = hashlib.sha256()
    with path.open("rb") as source:
        for block in iter(lambda: source.read(1024 * 1024), b""):
            digest.update(block)
    return digest.hexdigest()


def inventory_for(root: Path, packages: list[dict]) -> dict:
    rows = []
    for package in packages:
        package_root = package_destination(root, package)
        files = [
            {
                "path": path.as_posix(),
                "sha256": file_hash(package_root.joinpath(*path.parts)),
            }
            for path in validate_package(root, package)
        ]
        rows.append(
            {
                key: package[key]
                for key in ("name", "version", "url", "sri", "license", "licenseFile", "runtimeEntries")
            }
            | {"files": files}
        )
    return {"format": 1, "packages": rows}


def write_inventory(root: Path, packages: list[dict]) -> None:
    (root / "INVENTORY.json").write_text(
        json.dumps(inventory_for(root, packages), indent=2, sort_keys=True) + "\n",
        encoding="utf-8",
    )


def verify(root: Path, packages: list[dict]) -> None:
    validate_root_layout(root, packages)
    inventory = read_json(root / "INVENTORY.json", "host module inventory")
    rows = inventory.get("packages")
    if inventory.get("format") != 1 or not isinstance(rows, list) or len(rows) != len(packages):
        fail("host module inventory has an invalid format or package count")
    expected_files: set[tuple[str, str]] = set()
    for package, row in zip(packages, rows):
        if not isinstance(row, dict):
            fail("host module inventory package must be an object")
        for key in ("name", "version", "url", "sri", "license", "licenseFile", "runtimeEntries"):
            if row.get(key) != package[key]:
                fail(f"host module inventory does not match metadata for {package['name']}: {key}")
        files = row.get("files")
        if not isinstance(files, list) or not files:
            fail(f"host module inventory has no files for {package['name']}")
        listed_paths: list[PurePosixPath] = []
        for file in files:
            if not isinstance(file, dict):
                fail(f"host module inventory has invalid file row for {package['name']}")
            path = relative_path(file.get("path"), f"inventory file for {package['name']}")
            digest = file.get("sha256")
            if not isinstance(digest, str) or len(digest) != 64 or any(c not in "0123456789abcdef" for c in digest):
                fail(f"host module inventory has invalid SHA-256 for {package['name']}: {path}")
            listed_paths.append(path)
            expected_files.add((package["name"], path.as_posix()))
            actual = package_destination(root, package).joinpath(*path.parts)
            regular_file(actual, f"inventory file for {package['name']}")
            if file_hash(actual) != digest:
                fail(f"host module file hash mismatch: {actual}")
        if listed_paths != sorted(set(listed_paths), key=str):
            fail(f"host module inventory file paths are not unique and sorted for {package['name']}")
        if listed_paths != validate_package(root, package):
            fail(f"host module inventory file set does not match {package['name']}")
    actual_files: set[tuple[str, str]] = set()
    for package in packages:
        for path in package_files(root, package):
            actual_files.add((package["name"], path.as_posix()))
    if actual_files != expected_files:
        fail("host module inventory file paths do not match the module root")
    for entry in root.iterdir():
        if entry.name not in {"@deepseek-ai", "INVENTORY.json"}:
            fail(f"host module root has an unexpected entry: {entry}")
    regular_file(root / "INVENTORY.json", "host module inventory")


def safe_tar_members(archive: tarfile.TarFile) -> list[tuple[tarfile.TarInfo, PurePosixPath]]:
    entries: list[tuple[tarfile.TarInfo, PurePosixPath]] = []
    seen: set[PurePosixPath] = set()
    for member in archive.getmembers():
        name = member.name
        path = relative_path(name, "tar member")
        if not (member.isfile() or member.isdir()):
            fail(f"tarball contains a non-file, non-directory member: {name}")
        if path.parts[0] != "package":
            fail(f"tarball member is outside the npm package root: {name}")
        relative = PurePosixPath(*path.parts[1:])
        if not relative.parts:
            if not member.isdir():
                fail(f"tarball package root must be a directory: {name}")
            continue
        if relative in seen:
            fail(f"tarball contains a duplicate member: {name}")
        seen.add(relative)
        entries.append((member, relative))
    return entries


def extract(payload: bytes, destination: Path) -> None:
    try:
        with tarfile.open(fileobj=io.BytesIO(payload), mode="r:gz") as archive:
            entries = safe_tar_members(archive)
            if not entries:
                fail("tarball has no package files")
            for member, relative in entries:
                target = destination.joinpath(*relative.parts)
                if member.isdir():
                    target.mkdir(parents=True, exist_ok=True)
                    continue
                target.parent.mkdir(parents=True, exist_ok=True)
                source = archive.extractfile(member)
                if source is None:
                    fail(f"tarball file cannot be read: {member.name}")
                with source, target.open("xb") as output:
                    shutil.copyfileobj(source, output)
                target.chmod(0o755 if member.mode & 0o111 else 0o644)
    except (OSError, tarfile.TarError) as error:
        fail(f"cannot safely extract tarball: {error}")


def download(package: dict) -> bytes:
    try:
        with urlopen(package["url"], timeout=30) as response:
            payload = response.read()
    except OSError as error:
        fail(f"cannot download {package['name']}: {error}")
    if hashlib.sha512(payload).digest() != sri_bytes(package["sri"]):
        fail(f"SHA-512 mismatch for {package['name']}")
    return payload


def fetch(manifest: Path, output: Path) -> None:
    packages = read_manifest(manifest)
    if output.exists():
        fail(f"host module output must not already exist: {output}")
    output.parent.mkdir(parents=True, exist_ok=True)
    temporary = Path(tempfile.mkdtemp(prefix=f".{output.name}.", dir=output.parent))
    try:
        for package in packages:
            destination = package_destination(temporary, package)
            destination.mkdir(parents=True)
            extract(download(package), destination)
            validate_package(temporary, package)
        write_inventory(temporary, packages)
        verify(temporary, packages)
        temporary.rename(output)
    except BaseException:
        shutil.rmtree(temporary, ignore_errors=True)
        raise


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("manifest", type=Path)
    parser.add_argument("module_root", type=Path)
    parser.add_argument("--verify", action="store_true", help="verify an existing module root without downloading")
    arguments = parser.parse_args()
    try:
        packages = read_manifest(arguments.manifest)
        if arguments.verify:
            verify(arguments.module_root, packages)
        else:
            fetch(arguments.manifest, arguments.module_root)
    except HostModuleError as error:
        print(f"host modules: {error}", file=sys.stderr)
        return 2
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
