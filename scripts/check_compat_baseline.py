#!/usr/bin/env python3
"""Verify the frozen compatibility inventory and immutable README facts."""

from __future__ import annotations

from pathlib import Path
import json
import os
import re
import sys

PROJECT = Path(__file__).resolve().parents[1]
WORKSPACE = PROJECT.parent
UPSTREAM = Path(os.environ.get(
    "TESSIVUM_DEEPSEEK_SOURCE", WORKSPACE / "upstream/deepseek-harness"
))
CORDIS = Path(os.environ.get("TESSIVUM_CORDIS_SOURCE", WORKSPACE / "upstream/cordis"))
CORE = Path(os.environ.get("TESSIVUM_CORE_SOURCE", WORKSPACE / "tessivum-core"))
HARNESS_SHA = "47f943859bef60e4160492346772ded9b24f765a"
CORDIS_SHA = "8cc9e33fab69e2d0476d126baaf2acb24e6a6ab4"
CORE_SHA = "eb1bc9fd320f880f0939689adf9af1b666a4b682"
PRODUCT_VERSION = "v0.1.0-alpha.31"
CORE_VERSION = "v0.1.8"
HARNESS_VERSION = "0.1.0-rc.5"
BASELINE = PROJECT / "docs/COMPATIBILITY_BASELINE.md"
CHECKLIST = PROJECT / "docs/WEB_E2E_PORT_CHECKLIST.md"
README_EN = PROJECT / "README.md"
README_ZH = PROJECT / "README.zh-CN.md"
PLAN = PROJECT / "docs/DEVELOPMENT_PLAN.md"

README_COMMAND_TOKENS = (
    "brew tap wavetao2010/tap",
    "brew install tessivum",
    "sh install.sh 0.1.0-alpha.31",
    "brew upgrade tessivum",
    "tessivum web",
    "cargo run --release -- web",
)

EXPECTED_REMOTES = {
    "commands/list", "commands/execute",
    "goals/edit", "goals/pause", "goals/resume", "goals/complete", "goals/clear", "goals/create",
    "dynamicCordisRunner/undefineFromPanel", "dynamicCordisRunner/runHostHalf",
    "dynamicCordisRunner/getClientCode", "dynamicCordisRunner/resolveRequestRun",
    "dynamicCordisRunner/settleUserRun", "dynamicCordisRunner/stopFromPanel",
    "dynamicCordisRunner/syncInspectManifest", "dynamicCordisRunner/resolveInspectQuery",
    "dynamicCordisRunner/inventory", "dynamicCordisRunner/reportRenderFailure",
    "dynamicCordisRunner/reportClientGuardFailure", "dynamicCordisRunner/invoke",
    "pluginInventory/list", "messageFeedback/list", "messageFeedback/put", "messageFeedback/delete",
}
EXPECTED_HOST_EVENTS = {
    "agent-preset/selected", "commands/change", "credentials/updated", "cordis/request-run",
    "cordis/request-run-resolved", "cordis/dynamic-package", "cordis/dynamic-retract",
    "cordis/inspect-query", "cordis/inspect-query-resolved", "llm/adapters-updated",
    "settings/document-updated",
}
EXPECTED_NODE_KINDS = {
    "hello", "ready", "response", "error", "cancel", "heartbeat", "exit", "log",
    "plugin.load", "plugin.update", "plugin.dispose", "plugin.snapshot",
    "service.call", "service.provide", "service.remove",
    "event.subscribe", "event.emit", "event.callback", "registration.dispose",
    "web.route.register", "web.route.unregister", "web.route.request",
    "web.upgrade.register", "web.upgrade.unregister", "pnpm.run", "pnpm.output",
}


def fenced(text: str, heading: str) -> set[str]:
    match = re.search(rf"{re.escape(heading)}.*?```text\n(.*?)```", text, re.S)
    if match is None:
        raise AssertionError(f"missing text fence after {heading}")
    return {line.strip() for line in match.group(1).splitlines() if line.strip()}


def check(condition: bool, message: str, failures: list[str]) -> None:
    if not condition:
        failures.append(message)


def replace_workflow_once(workflow: str, current: str, replacement: str) -> str:
    anchor_count = workflow.count(current)
    if anchor_count != 1:
        raise AssertionError(
            "workflow fixture must contain exactly one anchor: "
            f"{current} (found {anchor_count})"
        )
    return workflow.replace(current, replacement, 1)


def strip_yaml_comment(value: str) -> str:
    quote: str | None = None
    for index, character in enumerate(value):
        if quote is None and character in {"'", '"'}:
            quote = character
        elif character == quote and (index == 0 or value[index - 1] != "\\"):
            quote = None
        elif (quote is None and character == "#"
              and (index == 0 or value[index - 1].isspace())):
            return value[:index].rstrip()
    return value.rstrip()


def workflow_job_key(line: str) -> str | None:
    match = re.fullmatch(r"  (['\"]?)([A-Za-z0-9_-]+)\1:\s*", line)
    return match.group(2) if match else None


def parse_workflow_step(item_lines: list[str]) -> dict[str, object]:
    step: dict[str, object] = {
        "keys": [], "withKeys": [], "with": {}, "run": [], "unconsumed": [],
    }
    lines = ["        " + item_lines[0][8:], *item_lines[1:]]
    mode: str | None = None
    for raw_line in lines:
        indent = len(raw_line) - len(raw_line.lstrip())
        if mode == "run" and indent >= 10:
            command = raw_line[10:].strip()
            if command:
                step["run"].append(command)
            continue
        line = strip_yaml_comment(raw_line).strip()
        if not line:
            continue
        if mode == "with" and indent == 10:
            match = re.fullmatch(
                r"(repository|ref|node-version|bun-version|version):\s*(.*)", line
            )
            if match:
                step["withKeys"].append(match.group(1))
                step["with"][match.group(1)] = unquote(match.group(2))
            else:
                step["unconsumed"].append(raw_line)
            continue
        if indent != 8:
            step["unconsumed"].append(raw_line)
            continue
        mode = None
        match = re.fullmatch(
            r"(['\"]?)(uses|name|with|run|if|continue-on-error)\1:\s*(.*)", line
        )
        if match is None:
            step["unconsumed"].append(raw_line)
            continue
        _, key, value = match.groups()
        step["keys"].append(key)
        if key == "with":
            mode = "with"
        elif key == "run":
            if value == "|":
                mode = "run"
            elif not value.startswith(("|", ">")):
                step["run"].append(unquote(value))
        else:
            step[key] = unquote(value)
    return step


def unquote(value: str) -> str:
    value = value.strip()
    if len(value) >= 2 and value[0] == value[-1] and value[0] in {"'", '"'}:
        return value[1:-1]
    return value


def parse_windows_workflow(
    ci_workflow: str,
) -> dict[str, object]:
    lines = ci_workflow.replace("\r\n", "\n").replace("\r", "\n").split("\n")
    visible = [strip_yaml_comment(line) for line in lines]
    jobs_indices = [
        index for index, line in enumerate(lines)
        if visible[index] == "jobs:"
    ]
    if len(jobs_indices) != 1:
        raise AssertionError("workflow must define jobs exactly once")

    windows_indices: list[int] = []
    for index in range(jobs_indices[0] + 1, len(lines)):
        line = visible[index]
        indent = len(line) - len(line.lstrip())
        if line.strip() and indent == 0:
            break
        if indent == 2 and workflow_job_key(line) == "windows":
            windows_indices.append(index)
    if len(windows_indices) != 1:
        raise AssertionError("workflow must define jobs.windows exactly once")

    job_start = windows_indices[0]
    job_end = len(lines)
    for index in range(job_start + 1, len(lines)):
        structural = visible[index]
        indent = len(structural) - len(structural.lstrip())
        if (workflow_job_key(structural) is not None
                or (structural.strip() and indent == 0)):
            job_end = index
            break
    job_lines = lines[job_start + 1:job_end]
    job_keys: list[str] = []
    job_unconsumed: list[str] = []
    runs_on_values = [
        unquote(match.group(1))
        for line in job_lines
        if (match := re.fullmatch(
            r"    runs-on:\s*(.*)", strip_yaml_comment(line)
        ))
    ]
    timeout_minutes_values = [
        unquote(match.group(1))
        for line in job_lines
        if (match := re.fullmatch(
            r"    timeout-minutes:\s*(.*)", strip_yaml_comment(line)
        ))
    ]
    steps_indices = [
        index for index, line in enumerate(job_lines)
        if strip_yaml_comment(line).rstrip() == "    steps:"
    ]
    steps_index = steps_indices[0] if steps_indices else -1
    step_body_end = len(job_lines)
    for index in range(steps_index + 1, len(job_lines)) if steps_index >= 0 else ():
        structural = strip_yaml_comment(job_lines[index])
        indent = len(structural) - len(structural.lstrip())
        if structural.strip() and indent <= 4:
            step_body_end = index
            break
    for index, raw_line in enumerate(job_lines):
        structural = strip_yaml_comment(raw_line)
        if not structural.strip():
            continue
        if steps_index >= 0 and steps_index < index < step_body_end:
            continue
        match = re.fullmatch(r"    ([A-Za-z0-9_-]+):\s*(.*)", structural)
        if match:
            job_keys.append(match.group(1))
            if match.group(1) not in {"runs-on", "timeout-minutes", "steps"}:
                job_unconsumed.append(raw_line)
        else:
            job_unconsumed.append(raw_line)

    step_lines = job_lines[steps_index + 1:step_body_end]
    item_starts = [
        index for index, line in enumerate(step_lines)
        if re.match(r"^ {6}-(?:\s|$)", strip_yaml_comment(line))
    ]
    steps = [
        parse_workflow_step(step_lines[start:item_starts[item_index + 1]
                                       if item_index + 1 < len(item_starts)
                                       else len(step_lines)])
        for item_index, start in enumerate(item_starts)
    ]
    return {
        "jobKeys": job_keys,
        "jobUnconsumed": job_unconsumed,
        "runsOnValues": runs_on_values,
        "timeoutMinutesValues": timeout_minutes_values,
        "stepsCount": len(steps_indices),
        "steps": steps,
    }


def parse_windows_workflow_steps(ci_workflow: str) -> list[dict[str, object]]:
    parsed = parse_windows_workflow(ci_workflow)
    if parsed["stepsCount"] != 1:
        raise AssertionError("jobs.windows must define steps exactly once")
    return parsed["steps"]


def check_windows_ci_prerequisite_order(ci_workflow: str, failures: list[str]) -> None:
    try:
        parsed = parse_windows_workflow(ci_workflow)
    except AssertionError as error:
        failures.append(f"Windows CI structure invalid: {error}")
        return

    if parsed["jobUnconsumed"]:
        failures.append("Windows CI windows job contains unconsumed syntax")
        return
    canonical_job = {
        "jobKeys": ["runs-on", "timeout-minutes", "steps"],
        "runsOnValues": ["windows-2025"],
        "timeoutMinutesValues": ["60"],
        "stepsCount": 1,
    }
    if any(parsed[key] != value for key, value in canonical_job.items()):
        failures.append("Windows CI windows job changed or moved")
        return
    steps = parsed["steps"]

    checkout = "actions/checkout@fbc6f3992d24b796d5a048ff273f7fcc4a7b6c09"
    expected_prefix = (
        ({"uses": checkout, "with": {}}, "primary checkout"),
        ({"uses": "actions/setup-node@249970729cb0ef3589644e2896645e5dc5ba9c38",
          "with": {"node-version": "24.20.0"}}, "setup-node"),
        ({"name": "Verify Windows Node filesystem prerequisite", "run": [
            "$ErrorActionPreference = 'Stop'",
            "$PSNativeCommandUseErrorActionPreference = $true",
            "node --test scripts/check-windows-node-unicode-fs.test.mjs",
            "node scripts/check-windows-node-unicode-fs.mjs",
        ]}, "Node prerequisite commands"),
        ({"uses": checkout, "with": {
            "repository": "deepseek-ai/deepseek-harness", "ref": HARNESS_SHA,
        }}, "DeepSeek checkout"),
        ({"uses": checkout, "with": {
            "repository": "cordiverse/cordis", "ref": CORDIS_SHA,
        }}, "Cordis checkout"),
        ({"uses": checkout, "with": {
            "repository": "wavetao2010/tessivum-core", "ref": CORE_SHA,
        }}, "tessivum-core checkout"),
    )
    for index, (expected, label) in enumerate(expected_prefix):
        if index in {1, 2}:
            continue
        actual = steps[index] if len(steps) > index else {}
        check(all(actual.get(key) == value for key, value in expected.items()),
              f"Windows CI {label} changed or moved", failures)
    protected_steps = (
        (1, "setup-node", {
            "keys": ["uses", "with"],
            "withKeys": ["node-version"],
            "uses": "actions/setup-node@249970729cb0ef3589644e2896645e5dc5ba9c38",
            "with": {"node-version": "24.20.0"},
            "run": [],
            "unconsumed": [],
        }),
        (2, "Node prerequisite", {
            "keys": ["name", "run"],
            "withKeys": [],
            "name": "Verify Windows Node filesystem prerequisite",
            "with": {},
            "run": [
                "$ErrorActionPreference = 'Stop'",
                "$PSNativeCommandUseErrorActionPreference = $true",
                "node --test scripts/check-windows-node-unicode-fs.test.mjs",
                "node scripts/check-windows-node-unicode-fs.mjs",
            ],
            "unconsumed": [],
        }),
    )
    for index, label, expected in protected_steps:
        step = steps[index] if len(steps) > index else {}
        if "if" in step or "continue-on-error" in step:
            failures.append(f"Windows CI {label} has bypass semantics")
        elif step.get("unconsumed") != []:
            failures.append(f"Windows CI {label} contains unconsumed syntax")
        elif step != expected:
            failures.append(f"Windows CI {label} changed or moved")

    later_requirements = (
        lambda step: step.get("uses")
        == "dtolnay/rust-toolchain@032958afbdc797a9164d3bc0b56325c1308924a5",
        lambda step: step.get("uses")
        == "oven-sh/setup-bun@0c5077e51419868618aeaa5fe8019c62421857d6"
        and isinstance(step.get("with"), dict)
        and step["with"].get("bun-version") == "1.4.0",
        lambda step: step.get("uses")
        == "pnpm/action-setup@b906affcce14559ad1aafd4ab0e942779e9f58b1"
        and isinstance(step.get("with"), dict)
        and step["with"].get("version") == "11.7.0",
        lambda step: step.get("name") == "Install pinned DeepSeek build dependencies"
        and step.get("run") == ["pnpm install --frozen-lockfile"],
    )
    cursor = 6
    for requirement in later_requirements:
        index = next(
            (index for index in range(cursor, len(steps)) if requirement(steps[index])),
            -1,
        )
        check(index >= 0, "Windows CI later prerequisite step missing or out of order",
              failures)
        if index < 0:
            break
        cursor = index + 1


def check_windows_ci_parser_self_checks(
    ci_workflow: str, failures: list[str]
) -> None:
    decoy_workflow = """jobs:
  windows:
    runs-on: windows-2025
    timeout-minutes: 60
    # uses: actions/checkout@fbc6f3992d24b796d5a048ff273f7fcc4a7b6c09
    steps:
      - name: "actions/setup-node@249970729cb0ef3589644e2896645e5dc5ba9c38"
        notes: |
          uses: actions/checkout@fbc6f3992d24b796d5a048ff273f7fcc4a7b6c09
  browser-e2e:
    steps: []
"""
    inserted_workflow = """jobs:
  windows:
    runs-on: windows-2025
    timeout-minutes: 60
    steps:
      - uses: actions/checkout@fbc6f3992d24b796d5a048ff273f7fcc4a7b6c09
  inserted-job:
    steps:
      - uses: actions/setup-node@249970729cb0ef3589644e2896645e5dc5ba9c38
  browser-e2e:
    steps: []
"""
    quoted_following_workflow = """jobs:
  windows:
    runs-on: windows-2025
  'quoted-job':
    steps:
      - uses: actions/checkout@fbc6f3992d24b796d5a048ff273f7fcc4a7b6c09
"""
    folded_workflow = """jobs:
  windows:
    runs-on: windows-2025
    steps:
      - run: >
          node scripts/check-windows-node-unicode-fs.mjs
  next-job:
    steps: []
"""
    following_workflow_field = """jobs:
  windows:
    runs-on: windows-2025
    timeout-minutes: 60
    steps:
      - run: echo windows
concurrency:
  group: ci
"""
    fixtures = (
        (decoy_workflow, "accepted decoy comments or block scalar content"),
        (inserted_workflow, "crossed into an inserted same-indent job"),
    )
    for workflow, message in fixtures:
        fixture_failures: list[str] = []
        check_windows_ci_prerequisite_order(workflow, fixture_failures)
        check(bool(fixture_failures), f"Windows CI parser {message}", failures)
    folded_steps = parse_windows_workflow_steps(folded_workflow)
    check(folded_steps[0].get("run") == [],
          "Windows CI parser accepted a folded run scalar", failures)
    quoted_boundary_error = None
    try:
        parse_windows_workflow_steps(quoted_following_workflow)
    except AssertionError as error:
        quoted_boundary_error = str(error)
    check(quoted_boundary_error == "jobs.windows must define steps exactly once",
          "Windows CI parser crossed into a quoted following job", failures)
    parsed_following_field = parse_windows_workflow(following_workflow_field)
    check(parsed_following_field["jobUnconsumed"] == [],
          "Windows CI parser included following workflow fields in windows job: "
          f"{parsed_following_field['jobUnconsumed']!r}", failures)
    check(len(parsed_following_field["steps"]) == 1
          and parsed_following_field["steps"][0].get("run") == ["echo windows"],
          "Windows CI parser included steps outside windows job", failures)

    semantic_variants = (
        ("disabled Windows job", "  windows:",
         "  windows:\n    if: ${{ false }}",
         "Windows CI windows job contains unconsumed syntax"),
        ("Windows job default shell override", "  windows:",
         '  windows:\n    defaults:\n      run:\n'
         '        shell: cmd /d /c "call {0} & exit /b 0"',
         "Windows CI windows job contains unconsumed syntax"),
        ("non-Windows runner", "    runs-on: windows-2025",
         "    runs-on: ubuntu-latest",
         "Windows CI windows job changed or moved"),
        ("setup-node if", "      - uses: actions/setup-node@249970729cb0ef3589644e2896645e5dc5ba9c38",
         "      - uses: actions/setup-node@249970729cb0ef3589644e2896645e5dc5ba9c38\n        if: false",
         "Windows CI setup-node has bypass semantics"),
        ("setup-node continue-on-error",
         "      - uses: actions/setup-node@249970729cb0ef3589644e2896645e5dc5ba9c38",
         "      - uses: actions/setup-node@249970729cb0ef3589644e2896645e5dc5ba9c38\n        continue-on-error: true",
         "Windows CI setup-node has bypass semantics"),
        ("Node prerequisite if",
         "      - name: Verify Windows Node filesystem prerequisite",
         "      - name: Verify Windows Node filesystem prerequisite\n        if: false",
         "Windows CI Node prerequisite has bypass semantics"),
        ("Node prerequisite continue-on-error",
         "      - name: Verify Windows Node filesystem prerequisite",
         "      - name: Verify Windows Node filesystem prerequisite\n        continue-on-error: true",
         "Windows CI Node prerequisite has bypass semantics"),
        ("setup-node explicit mapping if",
         "      - uses: actions/setup-node@249970729cb0ef3589644e2896645e5dc5ba9c38",
         "      - uses: actions/setup-node@249970729cb0ef3589644e2896645e5dc5ba9c38\n        ? if\n        : false",
         "Windows CI setup-node contains unconsumed syntax"),
        ("Node prerequisite explicit mapping continue-on-error",
         "      - name: Verify Windows Node filesystem prerequisite",
         "      - name: Verify Windows Node filesystem prerequisite\n        ? continue-on-error\n        : true",
         "Windows CI Node prerequisite contains unconsumed syntax"),
        ("setup-node shell",
         "      - uses: actions/setup-node@249970729cb0ef3589644e2896645e5dc5ba9c38",
         "      - uses: actions/setup-node@249970729cb0ef3589644e2896645e5dc5ba9c38\n        shell: pwsh",
         "Windows CI setup-node contains unconsumed syntax"),
        ("Node prerequisite env and NODE_OPTIONS",
         "      - name: Verify Windows Node filesystem prerequisite",
         "      - name: Verify Windows Node filesystem prerequisite\n        env:\n          NODE_OPTIONS: --no-addons",
         "Windows CI Node prerequisite contains unconsumed syntax"),
        ("Node prerequisite working-directory",
         "      - name: Verify Windows Node filesystem prerequisite",
         "      - name: Verify Windows Node filesystem prerequisite\n        working-directory: scripts",
         "Windows CI Node prerequisite contains unconsumed syntax"),
        ("setup-node unknown with input",
         "          node-version: 24.20.0",
         "          node-version: 24.20.0\n          cache: npm",
         "Windows CI setup-node contains unconsumed syntax"),
        ("Node prerequisite empty with block",
         "      - name: Verify Windows Node filesystem prerequisite",
         "      - name: Verify Windows Node filesystem prerequisite\n        with:",
         "Windows CI Node prerequisite changed or moved"),
        ("setup-node empty run block",
         "          node-version: 24.20.0",
         "          node-version: 24.20.0\n        run: |",
         "Windows CI setup-node changed or moved"),
        ("setup-node duplicate same-value uses key",
         "      - uses: actions/setup-node@249970729cb0ef3589644e2896645e5dc5ba9c38",
         "      - uses: actions/setup-node@249970729cb0ef3589644e2896645e5dc5ba9c38\n        uses: actions/setup-node@249970729cb0ef3589644e2896645e5dc5ba9c38",
         "Windows CI setup-node changed or moved"),
        ("setup-node duplicate node-version input",
         "          node-version: 24.20.0",
         "          node-version: 24.20.0\n          node-version: 24.20.0",
         "Windows CI setup-node changed or moved"),
        ("setup-node single-quoted if",
         "      - uses: actions/setup-node@249970729cb0ef3589644e2896645e5dc5ba9c38",
         "      - uses: actions/setup-node@249970729cb0ef3589644e2896645e5dc5ba9c38\n        'if': false",
         "Windows CI setup-node has bypass semantics"),
        ("setup-node double-quoted if",
         "      - uses: actions/setup-node@249970729cb0ef3589644e2896645e5dc5ba9c38",
         '      - uses: actions/setup-node@249970729cb0ef3589644e2896645e5dc5ba9c38\n        "if": false',
         "Windows CI setup-node has bypass semantics"),
        ("setup-node single-quoted continue-on-error",
         "      - uses: actions/setup-node@249970729cb0ef3589644e2896645e5dc5ba9c38",
         "      - uses: actions/setup-node@249970729cb0ef3589644e2896645e5dc5ba9c38\n        'continue-on-error': true",
         "Windows CI setup-node has bypass semantics"),
        ("setup-node double-quoted continue-on-error",
         "      - uses: actions/setup-node@249970729cb0ef3589644e2896645e5dc5ba9c38",
         '      - uses: actions/setup-node@249970729cb0ef3589644e2896645e5dc5ba9c38\n        "continue-on-error": true',
         "Windows CI setup-node has bypass semantics"),
        ("Node prerequisite single-quoted if",
         "      - name: Verify Windows Node filesystem prerequisite",
         "      - name: Verify Windows Node filesystem prerequisite\n        'if': false",
         "Windows CI Node prerequisite has bypass semantics"),
        ("Node prerequisite double-quoted if",
         "      - name: Verify Windows Node filesystem prerequisite",
         '      - name: Verify Windows Node filesystem prerequisite\n        "if": false',
         "Windows CI Node prerequisite has bypass semantics"),
        ("Node prerequisite single-quoted continue-on-error",
         "      - name: Verify Windows Node filesystem prerequisite",
         "      - name: Verify Windows Node filesystem prerequisite\n        'continue-on-error': true",
         "Windows CI Node prerequisite has bypass semantics"),
        ("Node prerequisite double-quoted continue-on-error",
         "      - name: Verify Windows Node filesystem prerequisite",
         '      - name: Verify Windows Node filesystem prerequisite\n        "continue-on-error": true',
         "Windows CI Node prerequisite has bypass semantics"),
    )
    for label, current, replacement, expected_failure in semantic_variants:
        variant_failures: list[str] = []
        check_windows_ci_prerequisite_order(
            replace_workflow_once(ci_workflow, current, replacement),
            variant_failures,
        )
        check(variant_failures == [expected_failure],
              f"Windows CI {label} mutation reported {variant_failures!r}, "
              f"expected {[expected_failure]!r}", failures)

    prerequisite_anchor = "      - name: Verify Windows Node filesystem prerequisite"
    ambiguous_workflow = ci_workflow.replace(
        prerequisite_anchor,
        f"      # decoy: {prerequisite_anchor}\n{prerequisite_anchor}",
        1,
    )
    ambiguous_error = None
    try:
        replace_workflow_once(
            ambiguous_workflow,
            prerequisite_anchor,
            f"{prerequisite_anchor}\n        if: false",
        )
    except AssertionError as error:
        ambiguous_error = str(error)
    check(ambiguous_error is not None
          and "must contain exactly one anchor" in ambiguous_error,
          "Windows CI mutation accepted an ambiguous prerequisite anchor", failures)


def repo_head(repo: Path) -> str:
    head = (repo / ".git/HEAD").read_text(encoding="utf-8").strip()
    if head.startswith("ref: "):
        return (repo / ".git" / head.removeprefix("ref: ")).read_text(encoding="utf-8").strip()
    return head


def main() -> int:
    failures: list[str] = []
    baseline = BASELINE.read_text(encoding="utf-8")
    checklist = CHECKLIST.read_text(encoding="utf-8")
    readmes = {
        "English": README_EN.read_text(encoding="utf-8"),
        "Simplified Chinese": README_ZH.read_text(encoding="utf-8"),
    }
    plan = PLAN.read_text(encoding="utf-8")

    check(repo_head(UPSTREAM) == HARNESS_SHA, "DeepSeek Harness checkout is not pinned", failures)
    check(repo_head(CORDIS) == CORDIS_SHA, "Cordis checkout is not pinned", failures)
    check(repo_head(CORE) == CORE_SHA, "tessivum-core checkout is not pinned", failures)
    check(HARNESS_SHA in baseline and HARNESS_SHA in checklist and HARNESS_SHA in plan,
          "DeepSeek Harness commit is not frozen consistently", failures)
    check(CORDIS_SHA in plan, "Cordis commit is not frozen in the development plan", failures)
    cargo_toml = (PROJECT / "Cargo.toml").read_text(encoding="utf-8")
    check(CORE_SHA in cargo_toml,
          "tessivum-core dependency revision changed", failures)
    check(f'version = "{PRODUCT_VERSION.removeprefix("v")}"' in cargo_toml,
          "Tessivum package version changed", failures)
    check(f'version = "={CORE_VERSION.removeprefix("v")}"' in cargo_toml,
          "tessivum-core package version changed", failures)
    ci_workflow = (PROJECT / ".github/workflows/ci.yml").read_text(encoding="utf-8")
    release_workflow = (PROJECT / ".github/workflows/release.yml").read_text(encoding="utf-8")
    check_windows_ci_parser_self_checks(ci_workflow, failures)
    check_windows_ci_prerequisite_order(ci_workflow, failures)
    check(ci_workflow.count(f"ref: {CORE_SHA}") == 3,
          "CI tessivum-core checkout revision changed", failures)
    check(release_workflow.count(f"ref: {CORE_SHA}") == 2,
          "release tessivum-core checkout revision changed", failures)
    harness_package = (UPSTREAM / "package.json").read_text(encoding="utf-8")
    check(f'"version": "{HARNESS_VERSION}"' in harness_package,
          "DeepSeek Harness package version changed", failures)

    for language, readme in readmes.items():
        for fact in (PRODUCT_VERSION, CORE_VERSION, CORE_SHA, HARNESS_VERSION, HARNESS_SHA):
            check(fact in readme, f"{language} README is missing {fact}", failures)
        for command in README_COMMAND_TOKENS:
            check(command in readme, f"{language} README is missing `{command}`", failures)
        for posture in ("disabled by default", "loopback-only"):
            check(posture in readme, f"{language} README Remote Access posture is stale", failures)
    check("English | [简体中文](README.zh-CN.md)" in readmes["English"],
          "English README language link is stale", failures)
    check("[English](README.md) | 简体中文" in readmes["Simplified Chinese"],
          "Simplified Chinese README language link is stale", failures)

    rpc_source = (UPSTREAM / "packages/host/apiproxy/src/api/rpc-map.ts").read_text(encoding="utf-8")
    upstream_rpc = set(re.findall(r"^\s*'([^']+)':", rpc_source, re.M))
    documented_rpc = fenced(baseline, "## 5. Core RPC 面")
    check(len(upstream_rpc) == 52, f"upstream RPC count changed: {len(upstream_rpc)}", failures)
    check(documented_rpc == upstream_rpc, "Core RPC inventory differs from upstream rpc-map.ts", failures)

    current_api = (PROJECT / "src/api.rs").read_text(encoding="utf-8")
    current_routes = set(re.findall(
        r'^\s*"([A-Za-z][A-Za-z0-9]*(?:\.[A-Za-z][A-Za-z0-9]*)+)"\s*=>',
        current_api,
        re.M,
    ))
    implemented = current_routes & upstream_rpc
    missing = upstream_rpc - current_routes
    check(implemented == upstream_rpc,
          f"current Core RPC routes differ: missing={sorted(missing)}", failures)

    check(fenced(baseline, "## 6. Typert Remote contributions") == EXPECTED_REMOTES,
          "Remote contribution inventory changed", failures)
    check(fenced(baseline, "## 7. 转发 Host 事件") == EXPECTED_HOST_EVENTS,
          "forwarded Host event inventory changed", failures)

    protocol = (CORE / "crates/tessivum-node-bridge/src/protocol.rs").read_text(encoding="utf-8")
    enum_body = re.search(r"pub enum FrameKind \{(.*?)\n\}", protocol, re.S)
    node_kinds = set(re.findall(r'#\[serde\(rename = "([^"]+)"\)\]', enum_body.group(1))) if enum_body else set()
    check(node_kinds == EXPECTED_NODE_KINDS, "cordis.node/v1 FrameKind inventory changed", failures)
    check('pub const PROTOCOL_VERSION: &str = "cordis.node/v1";' in protocol,
          "Node protocol version changed", failures)
    check("u32` 大端长度" in baseline and "默认单帧上限 `1 MiB`" in baseline,
          "Node framing limits missing from baseline", failures)

    boot = baseline.split("### 3.2 `window.__DSH_BOOT__`", 1)[1].split("## 4.", 1)[0]
    check("url: string" in boot and "package: string" not in boot and "name: string" not in boot,
          "WebBootEntry shape is not the upstream id/url/rev contract", failures)
    check("entries 数组的发布顺序不承载激活语义" in boot,
          "boot graph activation-order rule missing", failures)

    profile = (UPSTREAM / "packages/bundle/web-app/cordis.patch.yml").read_text(encoding="utf-8")
    roster_section = profile.split("browser plugin roster", 1)[1].split("the agent plane", 1)[0]
    profile_roster = set(re.findall(r"name: '([^']+)'", roster_section))
    web_package = json.loads((PROJECT / "web/package.json").read_text(encoding="utf-8"))
    vite_config = (PROJECT / "web/vite.config.ts").read_text(encoding="utf-8")
    source_audit = (PROJECT / "web/scripts/audit-deepseek-source.mjs").read_text(encoding="utf-8")
    registry_dsh_dependencies = [
        name for name in web_package["dependencies"] if name.startswith("@deepseek-ai/dsh-")
    ]
    check(len(profile_roster) == 33, f"Web profile roster changed: {len(profile_roster)}", failures)
    check(not registry_dsh_dependencies,
          "Browser shell still declares published DSH artifacts", failures)
    check("createDeepSeekSourceResolver" in vite_config and "deepSeekSourcePlugin" in vite_config,
          "Browser shell does not use the frozen source resolver", failures)
    check("@deepseek-ai/dsh-client-ui-theme/lib/styles" not in vite_config,
          "Browser shell still aliases registry theme styles", failures)
    check("auditSourceGraph" in source_audit and "published DSH dependencies remain" in source_audit,
          "Browser source audit no longer checks resolver and registry exclusion", failures)
    bundle_builder = (PROJECT / "web/scripts/build-deepseek-client-bundles.mjs").read_text(encoding="utf-8")
    web_lock = (PROJECT / "web/bun.lock").read_text(encoding="utf-8")
    binary = (PROJECT / "src/bin/tessivum.rs").read_text(encoding="utf-8")
    asset_builder = (PROJECT / "build.rs").read_text(encoding="utf-8")
    check("selected.size !== 38" in bundle_builder and "build:lib" in bundle_builder
          and "applyDeepSeekPatch" in bundle_builder,
          "pinned source contract build, compatibility patch, or 38-package gate is missing", failures)
    check("window.__ModuleLoader__" in bundle_builder and "createHash('sha1')" in bundle_builder,
          "source bundle handoff or content hash gate is missing", failures)
    check("EmbeddedWebAssets" in binary and "TESSIVUM_WEB_DIST" in binary
          and "include_bytes!" in asset_builder and "web/client-packages" in asset_builder,
          "Rust Web command does not embed built static and client assets", failures)
    check(not re.search(r'"@deepseek-ai/dsh-[^"]+": \["@deepseek-ai/dsh-', web_lock),
          "bun.lock retains published DSH artifacts", failures)
    frontend_source = (PROJECT / "src/frontend.rs").read_text(encoding="utf-8")
    check("Sha1::digest" in frontend_source and "/plugins/{id}/client.js?rev={rev}" in frontend_source,
          "Rust boot graph hash or bundle URL differs from the frozen source wire", failures)
    check((PROJECT / "web/src/main.ts").read_text(encoding="utf-8") == (UPSTREAM / "apps/web/src/main.ts").read_text(encoding="utf-8"),
          "Tessivum Web entry differs from pinned upstream source", failures)

    for token in (
        "ContentBlockMap", "FinishReasonMap", "reasoningTokens", "assistant/chunk",
        "assistant/message", "request/header", "llm/retry-started", "Session JSONL Replay",
    ):
        check(token in baseline, f"LLM/Agent contract token missing: {token}", failures)

    upstream_e2e = {path.name for path in (UPSTREAM / "apps/web/tests").glob("*.e2e.ts")}
    # Tessivum replaces these upstream first-run scenarios with neutral flows.
    upstream_e2e |= {"onboarding-deepseek-config.e2e.ts", "remote-welcome.e2e.ts"}
    ported_e2e = {path.name for path in (PROJECT / "web/tests").glob("*.e2e.ts")}
    listed_e2e = set(re.findall(r"\| \[[ x]\] \| \d+ \| `([^`]+\.e2e\.ts)` \|", checklist))
    completed_e2e = set(re.findall(r"\| \[x\] \| \d+ \| `([^`]+\.e2e\.ts)` \|", checklist))
    check(len(upstream_e2e) == 69, f"upstream Web E2E count changed: {len(upstream_e2e)}", failures)
    check(listed_e2e == upstream_e2e, "Web E2E checklist differs from pinned upstream files", failures)
    check(upstream_e2e <= ported_e2e, "ported Web E2E files omit pinned upstream files", failures)
    product_e2e = {"market.e2e.ts", "remote-access.e2e.ts", "image-input.e2e.ts", "history-recovery.e2e.ts"}
    check(ported_e2e - upstream_e2e == product_e2e,
          "product Web E2E inventory differs from approved first-party scenarios", failures)
    check(completed_e2e == upstream_e2e, "Web E2E checklist still contains unverified scenarios", failures)
    if failures:
        for failure in failures:
            print(f"FAIL: {failure}", file=sys.stderr)
        return 1

    print(
        "compat baseline OK: "
        f"RPC {len(implemented)}/{len(upstream_rpc)} implemented, "
        f"Remote {len(EXPECTED_REMOTES)}, Host events {len(EXPECTED_HOST_EVENTS)}, "
        f"Node kinds {len(node_kinds)}, Web source graph 38 (profile {len(profile_roster)}), "
        f"Web E2E {len(ported_e2e)} ({len(upstream_e2e)} upstream + {len(product_e2e)} first-party)"
    )
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
