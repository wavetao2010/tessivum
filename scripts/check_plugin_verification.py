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


def validate_lifecycle_evidence(path: Path, entry: dict[str, Any]) -> None:
    evidence = json.loads(path.read_text(encoding="utf-8"))
    verification = entry["verification"]
    if (evidence.get("schema") != "tessivum.plugin-lifecycle-verification/v1"
            or evidence.get("plugin") != entry["npm"]
            or evidence.get("verifiedVersion") != entry["version"]
            or evidence.get("updateVersion") != verification["updateVersion"]
            or evidence.get("failureVersion") != verification["failureVersion"]):
        raise ValueError(f"{entry['npm']}@{entry['version']}: lifecycle evidence identity does not match")
    revisions = evidence.get("revisions")
    if (not isinstance(revisions, dict)
            or any(not isinstance(revisions.get(name), str) or re.fullmatch(r"[0-9a-f]{40}", revisions[name]) is None
                   for name in ("product", "core", "deepseekHarness"))
            or not isinstance(evidence.get("binarySha256"), str)
            or re.fullmatch(r"[0-9a-f]{64}", evidence["binarySha256"]) is None):
        raise ValueError(f"{entry['npm']}@{entry['version']}: lifecycle evidence provenance is incomplete")
    product_link = evidence.get("productEvidence")
    if not isinstance(product_link, dict) or not isinstance(product_link.get("path"), str):
        raise ValueError(f"{entry['npm']}@{entry['version']}: product evidence link is missing")
    product_path = (path.parent / product_link["path"]).resolve()
    if (not product_path.is_relative_to(ROOT) or not product_path.is_file()
            or product_link.get("sha256") != hashlib.sha256(product_path.read_bytes()).hexdigest()):
        raise ValueError(f"{entry['npm']}@{entry['version']}: product evidence hash does not match")
    product = json.loads(product_path.read_text(encoding="utf-8"))
    repositories = product.get("provenance", {}).get("repositories", {})
    runtimes = product.get("provenance", {}).get("runtimes", [])
    samples = product.get("rawSamples", [])
    if (product.get("schema") != "tessivum.product-benchmark-run/v2"
            or product.get("status") != "passed" or product.get("failureCount") != 0
            or {name: repositories.get(name, {}).get("revision") for name in ("product", "coreBenchmark", "deepseekHarness")} != {
                "product": revisions["product"], "coreBenchmark": revisions["core"], "deepseekHarness": revisions["deepseekHarness"]}
            or len(runtimes) != 1 or runtimes[0].get("sha256") != evidence["binarySha256"]
            or len(samples) != 1):
        raise ValueError(f"{entry['npm']}@{entry['version']}: product evidence provenance does not match")
    sample = samples[0]
    browser = sample.get("web", {}).get("browser", {})
    boot = browser.get("result", {}).get("bootPlugins", [])
    feature = browser.get("result", {}).get("browserFeature", {})
    cleanups = [sample.get("headless", {}).get("cleanup"), browser.get("cleanup"), sample.get("web", {}).get("cleanup")]
    if (not sample.get("success") or sample.get("failures") != []
            or not any(isinstance(plugin, dict) and plugin.get("id") == verification["browserBootEntry"] for plugin in boot)
            or feature.get("selector") != verification["browserFeatureSelector"]
            or not isinstance(feature.get("count"), int) or feature["count"] < 1 or feature.get("visible") is not True
            or any(not isinstance(cleanup, dict) or cleanup.get("residueAfterDispose") != 0
                   or cleanup.get("forcedCleanupRequired") is not False
                   or cleanup.get("residueAfterForcedCleanup") != 0 for cleanup in cleanups)):
        raise ValueError(f"{entry['npm']}@{entry['version']}: product evidence did not prove clean Browser activation")
    checks = evidence.get("checks", {})
    if (checks.get("exactInstall", {}).get("installedVersion") != entry["version"]
            or checks.get("browserBootEntry", {}).get("id") != verification["browserBootEntry"]
            or checks.get("browserFeature") != {
                "name": verification["browserFeature"], "selector": verification["browserFeatureSelector"], "visible": True}
            or checks.get("update", {}).get("installedVersion") != verification["updateVersion"]
            or checks.get("remove", {}).get("dependencyAbsent") is not True
            or checks.get("remove", {}).get("bundleAbsent") is not True
            or checks.get("remove", {}).get("moduleAbsent") is not True
            or not isinstance(checks.get("failedInstallRollback", {}).get("exitCode"), int)
            or checks["failedInstallRollback"]["exitCode"] == 0
            or checks["failedInstallRollback"].get("manifestBeforeSha256") != checks["failedInstallRollback"].get("manifestAfterSha256")
            or checks["failedInstallRollback"].get("lockfileBeforeSha256") != checks["failedInstallRollback"].get("lockfileAfterSha256")
            or checks["failedInstallRollback"].get("moduleAbsent") is not True
            or checks.get("gracefulResidue") != {"headless": 0, "browser": 0, "webHost": 0, "forcedCleanupRequired": False}):
        raise ValueError(f"{entry['npm']}@{entry['version']}: lifecycle checks are incomplete")

def validate(network: bool) -> None:
    ledger = json.loads(LEDGER.read_text())
    current = ledger.get("current")
    if (ledger.get("schema") != "tessivum.plugin-verification/v2"
            or not isinstance(current, dict)
            or not all(isinstance(name, str) and isinstance(version, str) for name, version in current.items())
            or not isinstance(ledger.get("entries"), list)):
        raise ValueError("invalid plugin verification ledger")

    community = json.loads(COMMUNITY.read_text()).get("plugins", [])
    official = json.loads(OFFICIAL.read_text()).get("plugins", [])
    pairs: set[tuple[str, str]] = set()
    entries: dict[tuple[str, str], dict[str, Any]] = {}
    verified_packages: set[str] = set()
    for entry in ledger["entries"]:
        name = entry.get("npm")
        version = entry.get("version")
        status = entry.get("status")
        if not isinstance(name, str) or PACKAGE.fullmatch(name) is None:
            raise ValueError("verification package names must be valid")
        if not isinstance(version, str) or EXACT_VERSION.fullmatch(version) is None:
            raise ValueError(f"{name}: verification version must be exact semver")
        pair = (name, version)
        if pair in pairs:
            raise ValueError(f"duplicate verification release: {name}@{version}")
        if status not in {"verified", "revoked"}:
            raise ValueError(f"{name}@{version}: invalid verification status")
        if status == "revoked" and not entry.get("reason"):
            raise ValueError(f"{name}@{version}: revoked entries require a reason")
        for field in ("repository", "integrity", "license", "profile", "minimumTessivum", "verifiedAt", "evidence", "sha256"):
            if not isinstance(entry.get(field), str) or not entry[field]:
                raise ValueError(f"{name}@{version}: missing {field}")
        if re.fullmatch(r"[0-9a-f]{64}", entry["sha256"]) is None:
            raise ValueError(f"{name}@{version}: invalid evidence sha256")
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
        verification = entry.get("verification")
        if (not isinstance(verification, dict)
                or not isinstance(verification.get("browserFeature"), str) or not verification["browserFeature"]
                or not isinstance(verification.get("browserFeatureSelector"), str) or not verification["browserFeatureSelector"]
                or verification.get("browserBootEntry") != name
                or not isinstance(verification.get("updateVersion"), str)
                or EXACT_VERSION.fullmatch(verification["updateVersion"]) is None
                or not isinstance(verification.get("failureVersion"), str)
                or EXACT_VERSION.fullmatch(verification["failureVersion"]) is None
                or version in {verification["updateVersion"], verification["failureVersion"]}):
            raise ValueError(f"{name}@{version}: invalid lifecycle verification matrix")
        evidence = (ROOT / entry["evidence"]).resolve()
        if not evidence.is_relative_to(ROOT) or not evidence.is_file():
            raise ValueError(f"{name}@{version}: missing repository-local evidence file {entry['evidence']}")
        if hashlib.sha256(evidence.read_bytes()).hexdigest() != entry.get("sha256"):
            raise ValueError(f"{name}@{version}: lifecycle evidence hash does not match")
        if status == "verified":
            validate_lifecycle_evidence(evidence, entry)

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

        pairs.add(pair)
        entries[pair] = entry
        if status == "verified":
            verified_packages.add(name)
        print(f"{status}: {name}@{version} ({repository(entry['repository'])}, {entry['license']})")

    if set(current) != verified_packages:
        raise ValueError("current verification selections must exactly match packages with verified releases")
    for name, version in current.items():
        if entries.get((name, version), {}).get("status") != "verified":
            raise ValueError(f"current verification selection is not verified: {name}@{version}")


if __name__ == "__main__":
    parser = argparse.ArgumentParser()
    parser.add_argument("--network", action="store_true", help="also verify npm metadata and tarball integrity")
    validate(parser.parse_args().network)
