#!/usr/bin/env python3
"""Render the Homebrew Formula from a complete local release artifact set."""

from __future__ import annotations

import argparse
import hashlib
import re
from pathlib import Path

TARGETS = (
    "x86_64-unknown-linux-gnu",
    "aarch64-unknown-linux-gnu",
    "x86_64-apple-darwin",
    "aarch64-apple-darwin",
)
CHECKSUM = re.compile(r"^([0-9a-f]{64}) [ *](\S+)$")
PLACEHOLDER = re.compile(r"@[A-Z0-9_]+@")
ROOT = Path(__file__).resolve().parent.parent
TEMPLATE = ROOT / "packaging" / "homebrew" / "tessivum.rb.in"


def sha256(path: Path) -> str:
    digest = hashlib.sha256()
    with path.open("rb") as archive:
        for chunk in iter(lambda: archive.read(1024 * 1024), b""):
            digest.update(chunk)
    return digest.hexdigest()


def release_files(tag: str, directory: Path) -> tuple[str, dict[str, str]]:
    if not tag.startswith("v") or tag == "v":
        raise ValueError(f"release tag must be v-prefixed and nonempty: {tag}")
    version = tag[1:]
    expected_archives = {
        f"tessivum-{version}-{target}.tar.gz": target for target in TARGETS
    }
    archives = {path.name for path in directory.glob(f"tessivum-{version}-*.tar.gz")}
    if archives != set(expected_archives):
        raise ValueError("release archives must contain exactly the four supported targets")

    expected_checksums = {f"{archive}.sha256" for archive in expected_archives}
    checksums = {path.name for path in directory.glob("*.sha256")}
    if checksums != expected_checksums:
        raise ValueError("release checksums must contain exactly one file for each supported target")

    digests: dict[str, str] = {}
    for archive_name, target in expected_archives.items():
        archive = directory / archive_name
        checksum_path = directory / f"{archive_name}.sha256"
        lines = checksum_path.read_text(encoding="utf-8").splitlines()
        if len(lines) != 1:
            raise ValueError(f"checksum must contain exactly one entry: {checksum_path}")
        match = CHECKSUM.fullmatch(lines[0])
        if match is None or match.group(2) != archive_name:
            raise ValueError(f"checksum entry must name its archive: {checksum_path}")
        if match.group(1) != sha256(archive):
            raise ValueError(f"checksum does not match archive: {checksum_path}")
        digests[target] = match.group(1)
    return version, digests


def render(template: str, tag: str, version: str, digests: dict[str, str]) -> str:
    values = {"TAG": tag, "VERSION": version}
    values.update(
        {
            f"SHA256_{target.upper().replace('-', '_')}": digest
            for target, digest in digests.items()
        }
    )
    for name, value in values.items():
        template = template.replace(f"@{name}@", value)
    if PLACEHOLDER.search(template):
        raise ValueError("Formula template contains an unreplaced placeholder")
    return template


def main() -> None:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("tag", help="GitHub release tag, such as v0.1.0")
    parser.add_argument("archive_dir", type=Path, help="directory containing the four archives and checksums")
    parser.add_argument("output", type=Path, help="Formula path to write")
    args = parser.parse_args()

    version, digests = release_files(args.tag, args.archive_dir)
    formula = render(TEMPLATE.read_text(encoding="utf-8"), args.tag, version, digests)
    args.output.write_text(formula, encoding="utf-8")


if __name__ == "__main__":
    main()
