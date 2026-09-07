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


def pair(metric: dict[str, Any], scale: float, digits: int, suffix: str) -> str:
    return f"{metric['median'] / scale:,.{digits}f} / {metric['p95'] / scale:,.{digits}f} {suffix}"


def main() -> int:
    manifest = (ROOT / "Cargo.toml").read_text(encoding="utf-8").splitlines()
    version = next(line.split('"', 2)[1] for line in manifest if line.startswith("version = "))
    require(version == "0.1.0-alpha.23", f"unexpected package version: {version}")

    core = load(CORE_PATH)
    require(core.get("schema") == "tessivum.core-benchmark-paired/v2", "invalid Core evidence schema")
    require(core.get("status") == "success" and core.get("sampleCount") == 30, "Core evidence is not a successful 30-sample run")
    require(core.get("failures") == [], "Core evidence contains failures")
    require(core.get("revisions") == {
        "core": "4674aeda870989fede1fc79fb07afbe764d3a1eb",
        "dsh": "47f943859bef60e4160492346772ded9b24f765a",
        "product": "d455d99270673be208aecc3182cbf47b9b17989e",
    }, "Core evidence revisions drifted")
    for runtime in ("rust", "typescript"):
        samples = [metric.get("samples") for metric in core["runtimes"][runtime]["benchmarks"]]
        require(all(isinstance(values, list) and len(values) == 30 for values in samples), f"{runtime} Core metrics do not contain 30 raw samples")

    rust = metrics(core, "rust")
    typescript = metrics(core, "typescript")
    scope_ratio = typescript["scope_create_dispose"]["median"] / rust["scope_create_dispose"]["median"]
    peak_ratio = typescript["process_pss_live"]["median"] / rust["process_pss_live"]["median"]
    loader_regression = rust["loader_update"]["median"] / typescript["loader_update"]["median"]
    require((round(scope_ratio, 2), round(peak_ratio, 2), round(loader_regression, 2)) == (24.05, 17.15, 40.05), "Core release ratios drifted")
    require(rust["residue_after_dispose"]["max"] == 0 and typescript["residue_after_dispose"]["max"] == 0, "Core disposal residue is non-zero")

    product = load(PRODUCT_PATH)
    require(product.get("schema") == "tessivum.product-benchmark-run/v2", "invalid product evidence schema")
    require(product.get("status") == "passed" and product.get("publication") is True, "product evidence is not publication-grade")
    require(product.get("failureCount") == 0 and product.get("diagnostics") == [], "product evidence contains failures")
    require(product.get("arguments", {}).get("samples") == 30, "product evidence does not declare 30 samples")
    repositories = product.get("provenance", {}).get("repositories", {})
    require(repositories.get("coreBenchmark") == {"clean": True, "path": "/bench/work/tessivum-core", "revision": core["revisions"]["core"]}, "product Core provenance drifted")
    require(repositories.get("product") == {"clean": True, "path": "/bench/work/tessivum", "revision": core["revisions"]["product"]}, "product source provenance drifted")
    dsh = repositories.get("deepseekHarness", {})
    require(dsh.get("revision") == core["revisions"]["dsh"] and dsh.get("trackedDiffSha256") == "9e914d5998ccb2ca1faf8315a9d9a7235407c7830a8939255cd5838acd149ccd", "patched DeepSeek Harness provenance drifted")
    require(repositories.get("deepseekHarnessUpstream") == {"clean": True, "path": "/bench/work/upstream/deepseek-harness-upstream", "revision": core["revisions"]["dsh"]}, "upstream DeepSeek Harness provenance drifted")
    summaries = {(summary["manifest"], summary["runtime"]): summary for summary in product["summaries"]}
    expected_cells = {(manifest, runtime) for manifest in ("Base", "Compatibility") for runtime in ("tessivum", "deepseek-harness")}
    require(set(summaries) == expected_cells, "product evidence matrix drifted")
    for name, summary in summaries.items():
        require(summary["successfulSamples"] == 30 and summary["failedSamples"] == 0, f"{name} is not 30/30")
    require(len(product["rawSamples"]) == 120, "product evidence does not contain 120 raw samples")
    compatibility_plugins = {"tessivum-market", "dsh-better-sidebar", "dsh-dream-skin"}
    for sample in product["rawSamples"]:
        browser = sample["web"]["browser"]
        probe = browser["result"]
        require(sample["success"] is True and sample["failures"] == [], "a product raw sample failed")
        require(probe["errors"] == [] and probe["promptSubmitted"] is True and probe["sessionsCompleted"] == 10, "a Browser probe failed")
        expected_plugins = compatibility_plugins if sample["manifest"] == "Compatibility" else set()
        require({entry["id"] for entry in probe["bootPlugins"]} == expected_plugins, "Browser plugin graph drifted")
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
        CORE_PATH.name: "a2b0b468f85c021e0943aa24fee77b7d26fd46e954a4bcaf24ebcf48e4f151f9",
        PRODUCT_PATH.name: "a3ba246f394e91175ae4a51ca766afd2a2bc7796d3a5ac1f2f85a6ec0e7d9bf5",
    }
    for path in (CORE_PATH, PRODUCT_PATH):
        require(sha256(path) == expected_hashes[path.name], f"{path.name} digest drifted")

    cells = [summaries[(manifest, runtime)]["metrics"] for manifest, runtime in (
        ("Base", "tessivum"),
        ("Base", "deepseek-harness"),
        ("Compatibility", "tessivum"),
        ("Compatibility", "deepseek-harness"),
    )]
    core_cells = [
        f"{pair(rust['scope_create_dispose'], 1_000_000, 3, 'ms')} | {pair(typescript['scope_create_dispose'], 1_000_000, 3, 'ms')}",
        f"{pair(rust['service_lookup'], 1_000_000, 3, 'M ops/s')} | {pair(typescript['service_lookup'], 1_000_000, 3, 'M ops/s')}",
        f"{pair(rust['event_emit'], 1_000_000, 3, 'M ops/s')} | {pair(typescript['event_emit'], 1_000_000, 3, 'M ops/s')}",
        f"{pair(rust['loader_load'], 1_000_000, 3, 'ms')} | {pair(typescript['loader_load'], 1_000_000, 3, 'ms')}",
        f"{pair(rust['loader_update'], 1_000_000, 3, 'ms')} | {pair(typescript['loader_update'], 1_000_000, 3, 'ms')}",
        f"{pair(rust['root_dispose'], 1_000_000, 3, 'ms')} | {pair(typescript['root_dispose'], 1_000_000, 3, 'ms')}",
        f"{pair(rust['process_pss_live'], 1024, 2, 'MiB')} | {pair(typescript['process_pss_live'], 1024, 2, 'MiB')}",
        f"{pair(rust['process_pss_residue'], 1024, 2, 'MiB')} | {pair(typescript['process_pss_residue'], 1024, 2, 'MiB')}",
    ]
    core_ratios = [
        typescript["scope_create_dispose"]["median"] / rust["scope_create_dispose"]["median"],
        rust["service_lookup"]["median"] / typescript["service_lookup"]["median"],
        rust["event_emit"]["median"] / typescript["event_emit"]["median"],
        typescript["loader_load"]["median"] / rust["loader_load"]["median"],
        rust["loader_update"]["median"] / typescript["loader_update"]["median"],
        typescript["root_dispose"]["median"] / rust["root_dispose"]["median"],
        typescript["process_pss_live"]["median"] / rust["process_pss_live"]["median"],
        typescript["process_pss_residue"]["median"] / rust["process_pss_residue"]["median"],
    ]
    product_specs = [
        ("headless.completionElapsedNs", 1_000_000, 2, "ms"),
        ("web.readyElapsedNs", 1_000_000, 2, "ms"),
        ("web.browser.composerEnabledElapsedMs", 1, 1, "ms"),
        ("web.browser.firstPromptCompletionElapsedMs", 1, 1, "ms"),
        ("web.browser.tenSessionCompletionElapsedMs", 1, 1, "ms"),
        ("web.treeIdlePssKiB", 1024, 2, "MiB"),
        ("web.treeOneSessionDeltaFromIdleKiB", 1024, 2, "MiB"),
        ("web.treeTenSessionDeltaFromIdleKiB", 1024, 2, "MiB"),
        ("web.treeTenSessionPerSessionKiB", 1024, 3, "MiB"),
        ("web.disposeElapsedNs", 1_000_000, 2, "ms"),
    ]
    product_cells = [" | ".join(pair(cell[name], scale, digits, suffix) for cell in cells)
                     for name, scale, digits, suffix in product_specs]
    base_tessivum, base_dsh, compatibility_tessivum, compatibility_dsh = cells
    product_ratios = [
        base_dsh["headless.completionElapsedNs"]["median"] / base_tessivum["headless.completionElapsedNs"]["median"],
        base_dsh["web.readyElapsedNs"]["median"] / base_tessivum["web.readyElapsedNs"]["median"],
        base_dsh["web.treeIdlePssKiB"]["median"] / base_tessivum["web.treeIdlePssKiB"]["median"],
        base_dsh["web.treeTenSessionDeltaFromIdleKiB"]["median"] / base_tessivum["web.treeTenSessionDeltaFromIdleKiB"]["median"],
        compatibility_dsh["web.treeIdlePssKiB"]["median"] / compatibility_tessivum["web.treeIdlePssKiB"]["median"],
        compatibility_dsh["web.treeTenSessionDeltaFromIdleKiB"]["median"] / compatibility_tessivum["web.treeTenSessionDeltaFromIdleKiB"]["median"],
        compatibility_tessivum["web.readyElapsedNs"]["median"] / compatibility_dsh["web.readyElapsedNs"]["median"],
    ]
    pss_stability = max(cell[name]["p95"] / cell[name]["median"]
                        for cell in cells for name in ("web.treeIdlePssKiB", "web.treeTenSessionPssKiB"))
    report_facts = [
        *(f"{ratio:.2f}×" for ratio in (*core_ratios, *product_ratios)),
        *core_cells,
        *product_cells,
        f"{pss_stability:.2f}",
        "30/30",
        "120",
        *expected_hashes.values(),
        core["workloadSha256"],
        *(item["sha256"] for item in product["manifests"]),
        *core["revisions"].values(),
        product["provenance"]["productCoreDependencyRevision"],
        core["environmentSha256"],
        product["environmentSha256"],
        *product["provenance"]["drivers"].values(),
        *(item["sha256"] for item in product["provenance"]["runtimes"]),
        product["provenance"]["replay"]["sha256"],
        *(item["sha256"] for item in product["provenance"]["runtimeInputs"]),
        *(item["sha256"] for item in product["provenance"]["dataSeeds"]),
        dsh["trackedDiffSha256"],
    ]
    for path in REPORTS:
        report = path.read_text(encoding="utf-8")
        for fact in report_facts:
            require(fact in report, f"{path.name}: missing evidence-derived fact {fact}")
    english_report = REPORTS[0].read_text(encoding="utf-8")
    chinese_report = REPORTS[1].read_text(encoding="utf-8")
    require("Update 1 of 16 loaded entries" in english_report and "Process PSS with 1,000 live scopes" in english_report,
            "English report misstates a measured Core workload")
    require("更新 16 个已加载 entry 中的 1 个" in chinese_report and "1,000 个 Scope 存活时的进程 PSS" in chinese_report,
            "Chinese report misstates a measured Core workload")

    verification = load(ROOT / "plugins/market/compatibility.json")["entries"][0]
    lifecycle_path = ROOT / verification["evidence"]
    lifecycle = load(lifecycle_path)
    product_evidence_path = lifecycle_path.with_name(lifecycle["productEvidence"]["path"])
    plugin_product = load(product_evidence_path)
    require(lifecycle["revisions"] == {
        "product": "7d23d9ec0d1b62b878762970a5c1787eee8373dc",
        "core": "4674aeda870989fede1fc79fb07afbe764d3a1eb",
        "deepseekHarness": "47f943859bef60e4160492346772ded9b24f765a",
    }, "plugin lifecycle source revisions drifted")
    require(plugin_product["provenance"]["repositories"]["product"]["revision"] == lifecycle["revisions"]["product"],
            "plugin lifecycle product provenance drifted")
    plugin_report = (ROOT / "docs/PLUGIN_VERIFICATION_REPORT.md").read_text(encoding="utf-8")
    for digest in (sha256(lifecycle_path), sha256(product_evidence_path)):
        require(digest in plugin_report, "plugin verification report evidence digest drifted")

    english = (ROOT / "README.md").read_text(encoding="utf-8")
    chinese = (ROOT / "README.zh-CN.md").read_text(encoding="utf-8")
    require("Principle and implementation, in concert." in english[:1000], "English product slogan drifted")
    require("道器相成" in chinese[:1000], "Chinese product slogan drifted")
    readme_facts = [f"{ratio:.2f}×" for ratio in (
        scope_ratio, core_ratios[1], core_ratios[2], loader_regression, peak_ratio,
        product_ratios[1], product_ratios[2], product_ratios[4], product_ratios[6],
    )]
    require(all(fact in english for fact in readme_facts) and "30/30" in english, "English README benchmark claim drifted")
    require(all(fact in chinese for fact in readme_facts) and "30/30" in chinese, "Chinese README benchmark claim drifted")
    require("PHASE9_BENCHMARK_REPORT.md" in english, "English README lost benchmark evidence link")
    require("PHASE9_BENCHMARK_REPORT.zh-CN.md" in chinese, "Chinese README lost benchmark evidence link")

    print("PASS: Alpha.23 release facts match checked benchmark evidence")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
