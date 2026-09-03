#!/usr/bin/env python3
"""Compare benchmark medians only when the frozen measurement identity matches exactly."""

from __future__ import annotations

import argparse
import json
import math
import sys
from pathlib import Path
from typing import Any


def load(path: Path) -> dict[str, Any]:
    value = json.loads(path.read_text(encoding="utf-8"))
    if not isinstance(value, dict):
        raise ValueError(f"not a JSON object: {path}")
    return value


def identity(report: dict[str, Any]) -> dict[str, Any]:
    schema = report.get("schema")
    common = {"schema": schema, "environmentSha256": report.get("environmentSha256")}
    if schema in {"tessivum.core-benchmark-paired/v1", "tessivum.core-benchmark-paired/v2"}:
        return {
            **common,
            "workloadSha256": report.get("workloadSha256"),
            "sampleCount": report.get("sampleCount"),
            "revisions": report.get("revisions"),
            "runtimes": {name: value.get("runtime") for name, value in report.get("runtimes", {}).items()},
        }
    if schema == "tessivum.product-benchmark-run/v1":
        return {
            **common,
            "workloadSha256": report.get("workloadSha256"),
            "sampleCount": report.get("arguments", {}).get("samples"),
            "manifests": [
                {"name": manifest.get("name"), "sha256": manifest.get("sha256"), "revisions": manifest.get("revisions")}
                for manifest in report.get("manifests", [])
            ],
            "runtimes": [
                {"id": runtime.get("id"), "version": runtime.get("version")}
                for runtime in report.get("arguments", {}).get("binaries", [])
            ],
        }
    if schema == "tessivum.product-benchmark-run/v2":
        provenance = report.get("provenance", {})
        repositories = provenance.get("repositories", {})
        return {
            **common,
            "sampleCount": report.get("arguments", {}).get("samples"),
            "manifests": [{"name": item.get("name"), "sha256": item.get("sha256")} for item in report.get("manifests", [])],
            "repositories": {
                name: {key: value.get(key) for key in ("revision", "clean", "trackedDiffSha256") if key in value}
                for name, value in repositories.items()
            },
            "productCoreDependencyRevision": provenance.get("productCoreDependencyRevision"),
            "drivers": provenance.get("drivers"),
            "replaySha256": provenance.get("replay", {}).get("sha256"),
            "profileSha256": sorted((item.get("manifest"), item.get("sha256")) for item in provenance.get("profiles", [])),
            "hostModuleSha256": sorted(value.get("sha256") for value in provenance.get("hostModules", [])),
            "runtimes": [{key: runtime.get(key) for key in ("id", "version", "sha256")} for runtime in provenance.get("runtimes", [])],
        }
    raise ValueError(f"unsupported benchmark schema: {schema!r}")


def complete(value: Any) -> bool:
    if value is None or value == "unavailable":
        return False
    if isinstance(value, dict):
        return bool(value) and all(complete(item) for item in value.values())
    if isinstance(value, list):
        return bool(value) and all(complete(item) for item in value)
    return True


def finite_number(value: Any) -> bool:
    return isinstance(value, (int, float)) and not isinstance(value, bool) and math.isfinite(value)


def successful(report: dict[str, Any]) -> bool:
    schema = report.get("schema")
    if schema in {"tessivum.core-benchmark-paired/v1", "tessivum.core-benchmark-paired/v2"}:
        count = report.get("sampleCount")
        runtimes = report.get("runtimes")
        return (report.get("status") == "success"
                and report.get("failures") == []
                and isinstance(count, int) and count > 0
                and isinstance(runtimes, dict) and bool(runtimes)
                and all(isinstance(runtime.get("benchmarks"), list) and bool(runtime["benchmarks"])
                        and all(finite_number(metric.get("median")) and finite_number(metric.get("p95"))
                                and isinstance(metric.get("samples"), list) and len(metric["samples"]) == count
                                and all(finite_number(sample) for sample in metric["samples"])
                                for metric in runtime["benchmarks"])
                        for runtime in runtimes.values() if isinstance(runtime, dict))
                and all(isinstance(runtime, dict) for runtime in runtimes.values()))
    if schema in {"tessivum.product-benchmark-run/v1", "tessivum.product-benchmark-run/v2"}:
        arguments = report.get("arguments", {})
        count = arguments.get("samples")
        manifests = report.get("manifests")
        binaries = arguments.get("binaries")
        raw = report.get("rawSamples")
        summaries = report.get("summaries")
        if not (report.get("status") == "passed" and report.get("failureCount") == 0
                and report.get("diagnostics") == [] and isinstance(count, int) and count > 0
                and isinstance(manifests, list) and manifests and isinstance(binaries, list) and binaries
                and isinstance(raw, list) and isinstance(summaries, list)):
            return False
        manifest_names = {item.get("name") for item in manifests if isinstance(item, dict)}
        runtime_ids = {item.get("id") for item in binaries if isinstance(item, dict)}
        if (None in manifest_names or None in runtime_ids or len(manifest_names) != len(manifests)
                or len(runtime_ids) != len(binaries)):
            return False
        expected = {(manifest, runtime, repetition) for manifest in manifest_names for runtime in runtime_ids
                    for repetition in range(1, count + 1)}
        actual = {(sample.get("manifest"), sample.get("runtime", {}).get("id"), sample.get("repetition"))
                  for sample in raw if isinstance(sample, dict) and isinstance(sample.get("runtime"), dict)}
        summary_keys = {(summary.get("manifest"), summary.get("runtime"))
                        for summary in summaries if isinstance(summary, dict)}
        return (len(raw) == len(expected) and actual == expected and summary_keys == {(m, r) for m in manifest_names for r in runtime_ids}
                and all(isinstance(sample, dict) and sample.get("success") is True and sample.get("failures") == [] for sample in raw)
                and all(summary.get("successfulSamples") == count and summary.get("failedSamples") == 0
                        and isinstance(summary.get("metrics"), dict) and bool(summary["metrics"])
                        and all(finite_number(metric.get("median")) and finite_number(metric.get("p95"))
                                and metric.get("successfulSamples") == count for metric in summary["metrics"].values())
                        for summary in summaries))
    return False


def metrics(report: dict[str, Any]) -> dict[str, tuple[float, bool]]:
    values: dict[str, tuple[float, bool]] = {}
    if report["schema"] in {"tessivum.core-benchmark-paired/v1", "tessivum.core-benchmark-paired/v2"}:
        for runtime, result in report["runtimes"].items():
            for metric in result["benchmarks"]:
                if isinstance(metric.get("median"), (int, float)):
                    values[f"{runtime}.{metric['name']}"] = (float(metric["median"]), metric["unit"] == "operations/s")
    else:
        for result in report["summaries"]:
            for name, metric in result["metrics"].items():
                if isinstance(metric.get("median"), (int, float)):
                    values[f"{result['manifest']}.{result['runtime']}.{name}"] = (float(metric["median"]), False)
    return values


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("current", type=Path)
    parser.add_argument("fixture", type=Path)
    parser.add_argument("--max-regression-percent", type=float, default=20.0)
    args = parser.parse_args()
    if not math.isfinite(args.max_regression_percent) or args.max_regression_percent < 0:
        parser.error("--max-regression-percent must be non-negative")
    current, fixture = load(args.current), load(args.fixture)
    if not successful(current) or not successful(fixture):
        print("FAIL: benchmark result is incomplete or unsuccessful", file=sys.stderr)
        return 1

    current_identity, fixture_identity = identity(current), identity(fixture)
    if not complete(current_identity) or not complete(fixture_identity):
        print("FAIL: benchmark identity is incomplete", file=sys.stderr)
        return 1
    if current_identity != fixture_identity:
        changed = [key for key in sorted(set(current_identity) | set(fixture_identity)) if current_identity.get(key) != fixture_identity.get(key)]
        print(f"FAIL: benchmark identity changed ({', '.join(changed)})", file=sys.stderr)
        return 1

    current_metrics, fixture_metrics = metrics(current), metrics(fixture)
    if current_metrics.keys() != fixture_metrics.keys():
        print("FAIL: benchmark metric set changed", file=sys.stderr)
        return 1

    limit = args.max_regression_percent / 100.0
    failures = []
    for name, (actual, higher_is_better) in current_metrics.items():
        baseline = fixture_metrics[name][0]
        tolerance = abs(baseline) * limit
        regressed = actual < baseline - tolerance if higher_is_better else actual > baseline + tolerance
        if regressed:
            failures.append(f"{name}: fixture={baseline:g}, current={actual:g}")
    if failures:
        print("FAIL: benchmark median regression exceeded threshold", file=sys.stderr)
        print("\n".join(failures), file=sys.stderr)
        return 1
    print(f"PASS: {len(current_metrics)} comparable benchmark medians")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
