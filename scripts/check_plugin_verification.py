#!/usr/bin/env python3
"""Validate Tessivum's exact community-plugin verification ledger."""

from __future__ import annotations

import argparse
import base64
import hashlib
import json
import re
import urllib.parse
import urllib.request
from pathlib import Path
from typing import Any

ROOT = Path(__file__).resolve().parents[1]
LEDGER = ROOT / "plugins/market/compatibility.json"
COMMUNITY = ROOT / "plugins/market/data/registry-snapshot.json"
OFFICIAL = ROOT / "plugins/market/catalog.json"
EXACT_VERSION = re.compile(r"^(?:0|[1-9]\d*)\.(?:0|[1-9]\d*)\.(?:0|[1-9]\d*)(?:-[0-9A-Za-z.-]+)?(?:\+[0-9A-Za-z.-]+)?$")
PACKAGE = re.compile(r"^(?:@[a-z0-9][a-z0-9._-]*/)?[a-z0-9][a-z0-9._-]*$")
DATE = re.compile(r"^\d{4}-\d{2}-\d{2}$")
RUNTIMES = {"native", "wasm", "legacy-node", "browser"}


def repository(value: Any) -> str | None:
    if isinstance(value, dict):
        value = value.get("url")
    if not isinstance(value, str):
        return None
    match = re.search(r"github\.com[/:]([^/]+)/([^/#?]+)", value.removeprefix("git+"), re.I)
    if match is None:
        return None
    return f"{match.group(1)}/{match.group(2).removesuffix('.git')}".lower()


def fetch_json(url: str) -> dict[str, Any]:
    request = urllib.request.Request(url, headers={"User-Agent": "tessivum-plugin-verification/1"})
    with urllib.request.urlopen(request, timeout=30) as response:
        return json.load(response)


def check_integrity(url: str, integrity: str) -> None:
    algorithm, encoded = integrity.split("-", 1)
    if algorithm != "sha512":
        raise ValueError(f"unsupported integrity algorithm: {algorithm}")
    digest = hashlib.sha512()
    request = urllib.request.Request(url, headers={"User-Agent": "tessivum-plugin-verification/1"})
    with urllib.request.urlopen(request, timeout=60) as response:
        while chunk := response.read(1024 * 1024):
            digest.update(chunk)
    actual = base64.b64encode(digest.digest()).decode()
    if actual != encoded:
        raise ValueError("release tarball integrity does not match the ledger")


def validate(network: bool) -> None:
    ledger = json.loads(LEDGER.read_text())
    if ledger.get("schema") != "tessivum.plugin-verification/v1" or not isinstance(ledger.get("entries"), list):
        raise ValueError("invalid plugin verification ledger")

    community = json.loads(COMMUNITY.read_text()).get("plugins", [])
    official = json.loads(OFFICIAL.read_text()).get("plugins", [])
    names: set[str] = set()
    for entry in ledger["entries"]:
        name = entry.get("npm")
        version = entry.get("version")
        status = entry.get("status")
        if not isinstance(name, str) or PACKAGE.fullmatch(name) is None or name in names:
            raise ValueError("verification package names must be valid and unique")
        if not isinstance(version, str) or EXACT_VERSION.fullmatch(version) is None:
            raise ValueError(f"{name}: verification version must be exact semver")
        if status not in {"verified", "revoked"}:
            raise ValueError(f"{name}@{version}: invalid verification status")
        if status == "revoked" and not entry.get("reason"):
            raise ValueError(f"{name}@{version}: revoked entries require a reason")
        for field in ("repository", "integrity", "license", "profile", "minimumTessivum", "verifiedAt", "evidence"):
            if not isinstance(entry.get(field), str) or not entry[field]:
                raise ValueError(f"{name}@{version}: missing {field}")
        if repository(entry["repository"]) is None or not entry["integrity"].startswith("sha512-"):
            raise ValueError(f"{name}@{version}: repository and integrity must identify an immutable npm release")
        if entry["profile"] not in {"web", "headless"} or EXACT_VERSION.fullmatch(entry["minimumTessivum"]) is None:
            raise ValueError(f"{name}@{version}: invalid Profile or minimum Tessivum version")
        if DATE.fullmatch(entry["verifiedAt"]) is None:
            raise ValueError(f"{name}@{version}: verifiedAt must be YYYY-MM-DD")
        runtimes = entry.get("runtimes")
        if (not isinstance(runtimes, list) or not runtimes
                or not all(isinstance(runtime, str) for runtime in runtimes)
                or not set(runtimes) <= RUNTIMES):
            raise ValueError(f"{name}@{version}: invalid runtimes")
        evidence = (ROOT / entry["evidence"]).resolve()
        if not evidence.is_relative_to(ROOT) or not evidence.is_file():
            raise ValueError(f"{name}@{version}: missing repository-local evidence file {entry['evidence']}")

        catalog = next((plugin for plugin in community if plugin.get("npm") == name), None)
        if catalog is None or repository(catalog.get("url")) != repository(entry["repository"]):
            raise ValueError(f"{name}@{version}: community catalog identity does not match")
        if any(plugin.get("npm") == name for plugin in official):
            raise ValueError(f"{name}@{version}: community verification is shadowed by the official catalog")

        if network:
            package = urllib.parse.quote(name, safe="@")
            release = urllib.parse.quote(version, safe="")
            metadata = fetch_json(f"https://registry.npmjs.org/{package}/{release}")
            comparisons = {
                "version": metadata.get("version"),
                "license": metadata.get("license"),
                "integrity": metadata.get("dist", {}).get("integrity"),
            }
            for field, actual in comparisons.items():
                if actual != entry[field]:
                    raise ValueError(f"{name}@{version}: npm {field} does not match the ledger")
            if repository(metadata.get("repository")) != repository(entry["repository"]):
                raise ValueError(f"{name}@{version}: npm repository does not match the ledger")
            check_integrity(metadata["dist"]["tarball"], entry["integrity"])

        names.add(name)
        print(f"{status}: {name}@{version} ({repository(entry['repository'])}, {entry['license']})")


if __name__ == "__main__":
    parser = argparse.ArgumentParser()
    parser.add_argument("--network", action="store_true", help="also verify npm metadata and tarball integrity")
    validate(parser.parse_args().network)
