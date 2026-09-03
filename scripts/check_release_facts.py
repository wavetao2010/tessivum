#!/usr/bin/env python3
"""Reject drift between Alpha.23 release claims and checked benchmark evidence."""

from __future__ import annotations

import hashlib
import json
from pathlib import Path
from typing import Any

ROOT = Path(__file__).resolve().parents[1]
CORE_PATH = ROOT / "benchmarks/fixtures/phase9-alpha23/core-paired-30.json"
PRODUCT_PATH = ROOT / "benchmarks/fixtures/phase9-alpha23/product-30.json"
REPORTS = [
    ROOT / "docs/PHASE9_BENCHMARK_REPORT.md",
    ROOT / "docs/PHASE9_BENCHMARK_REPORT.zh-CN.md",
]


def require(condition: bool, message: str) -> None:
    if not condition:
        raise ValueError(message)


def load(path: Path) -> dict[str, Any]:
    value = json.loads(path.read_text(encoding="utf-8"))
    require(isinstance(value, dict), f"{path}: expected a JSON object")
    return value


def sha256(path: Path) -> str:
    return hashlib.sha256(path.read_bytes()).hexdigest()


def metrics(report: dict[str, Any], runtime: str) -> dict[str, dict[str, Any]]:
    return {metric["name"]: metric for metric in report["runtimes"][runtime]["benchmarks"]}


def main() -> int:
    manifest = (ROOT / "Cargo.toml").read_text(encoding="utf-8").splitlines()
    version = next(line.split('"', 2)[1] for line in manifest if line.startswith("version = "))
    require(version == "0.1.0-alpha.23", f"unexpected package version: {version}")

    core = load(CORE_PATH)
    require(core.get("schema") == "tessivum.core-benchmark-paired/v1", "invalid Core evidence schema")
    require(core.get("status") == "success" and core.get("sampleCount") == 30, "Core evidence is not a successful 30-sample run")
    require(core.get("failures") == [], "Core evidence contains failures")
    require(core.get("revisions") == {
        "core": "cedbeb9e1607056845b69e09b825eb7f5be67a69",
        "dsh": "47f943859bef60e4160492346772ded9b24f765a",
        "product": "4d2bd09573ff9f9b027cee4c0d14a4784309e164",
    }, "Core evidence revisions drifted")
    for runtime in ("rust", "typescript"):
        samples = [metric.get("samples") for metric in core["runtimes"][runtime]["benchmarks"]]
        require(all(isinstance(values, list) and len(values) == 30 for values in samples), f"{runtime} Core metrics do not contain 30 raw samples")

    rust = metrics(core, "rust")
    typescript = metrics(core, "typescript")
    scope_ratio = typescript["scope_create_dispose"]["median"] / rust["scope_create_dispose"]["median"]
    peak_ratio = typescript["process_pss_peak"]["median"] / rust["process_pss_peak"]["median"]
    loader_regression = rust["loader_update"]["median"] / typescript["loader_update"]["median"]
    require((round(scope_ratio, 2), round(peak_ratio, 2), round(loader_regression, 2)) == (24.02, 17.43, 39.49), "Core release ratios drifted")
    require(rust["residue_after_dispose"]["max"] == 0 and typescript["residue_after_dispose"]["max"] == 0, "Core disposal residue is non-zero")

    product = load(PRODUCT_PATH)
    require(product.get("schema") == "tessivum.product-benchmark-run/v1", "invalid product evidence schema")
    require(product.get("status") == "passed" and product.get("publication") is True, "product evidence is not publication-grade")
    require(product.get("failureCount") == 0 and product.get("diagnostics") == [], "product evidence contains failures")
    require(product.get("arguments", {}).get("samples") == 30, "product evidence does not declare 30 samples")
    summaries = {summary["manifest"]: summary for summary in product["summaries"]}
    require(set(summaries) == {"Base", "Compatibility"}, "product evidence manifest set drifted")
    for name, summary in summaries.items():
        require(summary["successfulSamples"] == 30 and summary["failedSamples"] == 0, f"{name} is not 30/30")
    require(len(product["rawSamples"]) == 60, "product evidence does not contain 60 raw samples")
    for sample in product["rawSamples"]:
        browser = sample["web"]["browser"]
        probe = browser["result"]
        require(sample["success"] is True and sample["failures"] == [], "a product raw sample failed")
        require(probe["errors"] == [] and probe["promptSubmitted"] is True and probe["sessionsCompleted"] == 10, "a Browser probe failed")
        cleanup_rows = {
            "headless": sample["headless"]["cleanup"],
            "browser": browser["cleanup"],
            "web": sample["web"]["cleanup"],
        }
        for stage, cleanup in cleanup_rows.items():
            require(cleanup["residueAfterDispose"] == 0, f"{stage} process residue is non-zero")
            require(cleanup["forcedCleanupRequired"] is False, f"{stage} required forced cleanup")
            require(cleanup["residueAfterForcedCleanup"] == 0, f"{stage} cleanup left process residue")

    expected_hashes = {
        CORE_PATH.name: "325f9b16352263f17d0b04b629cc22a1c6ec73adbde0eacb6882caf51485d69c",
        PRODUCT_PATH.name: "6ae6f1b7a897ff7395e63121926a7e61378a251df3a411a37d48e202eae0cf80",
    }
    for path in (CORE_PATH, PRODUCT_PATH):
        require(sha256(path) == expected_hashes[path.name], f"{path.name} digest drifted")

    for path in REPORTS:
        report = path.read_text(encoding="utf-8")
        for fact in ("24.02×", "17.43×", "39.49×", "30/30", *expected_hashes.values()):
            require(fact in report, f"{path.name}: missing release fact {fact}")

    english = (ROOT / "README.md").read_text(encoding="utf-8")
    chinese = (ROOT / "README.zh-CN.md").read_text(encoding="utf-8")
    require("Principle and implementation, in concert." in english[:1000], "English product slogan drifted")
    require("道器相成" in chinese[:1000], "Chinese product slogan drifted")
    require("24.02×" in english and "30/30" in english, "English README benchmark claim drifted")
    require("24.02×" in chinese and "30/30" in chinese, "Chinese README benchmark claim drifted")
    require("PHASE9_BENCHMARK_REPORT.md" in english, "English README lost benchmark evidence link")
    require("PHASE9_BENCHMARK_REPORT.zh-CN.md" in chinese, "Chinese README lost benchmark evidence link")

    print("PASS: Alpha.23 release facts match checked benchmark evidence")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
