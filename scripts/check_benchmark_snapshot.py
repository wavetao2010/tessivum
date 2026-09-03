#!/usr/bin/env python3
"""Compare benchmark medians only when the frozen measurement identity matches."""

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
    common = {
        "schema": report.get("schema"),
        "environmentSha256": report.get("environmentSha256"),
        "workloadSha256": report.get("workloadSha256"),
    }
    if report.get("schema") == "tessivum.core-benchmark-paired/v1":
        return {
            **common,
            "sampleCount": report.get("sampleCount"),
            "revisions": report.get("revisions"),
            "runtimes": {name: value.get("runtime") for name, value in report.get("runtimes", {}).items()},
        }
    if report.get("schema") == "tessivum.product-benchmark-run/v1":
        return {
            **common,
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
    raise ValueError(f"unsupported benchmark schema: {report.get('schema')!r}")


def complete(value: Any) -> bool:
    if value is None or value == "unavailable":
        return False
    if isinstance(value, dict):
        return bool(value) and all(complete(item) for item in value.values())
    if isinstance(value, list):
        return bool(value) and all(complete(item) for item in value)
    return True


def metrics(report: dict[str, Any]) -> dict[str, tuple[float, bool]]:
    values: dict[str, tuple[float, bool]] = {}
    if report["schema"] == "tessivum.core-benchmark-paired/v1":
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

    current_identity, fixture_identity = identity(current), identity(fixture)
    if not complete(current_identity) or not complete(fixture_identity):
        print("SKIP: benchmark identity is incomplete; fixture is not comparable")
        return 0
    if current_identity != fixture_identity:
        changed = [key for key in sorted(set(current_identity) | set(fixture_identity)) if current_identity.get(key) != fixture_identity.get(key)]
        print(f"SKIP: benchmark identity changed ({', '.join(changed)}); fixture is not comparable")
        return 0

    current_metrics, fixture_metrics = metrics(current), metrics(fixture)
    if current_metrics.keys() != fixture_metrics.keys():
        print("SKIP: benchmark metric set changed; fixture is not comparable")
        return 0

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
