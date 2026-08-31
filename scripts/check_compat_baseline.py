#!/usr/bin/env python3
"""Verify the pinned DeepSeek compatibility inventory and current migration counts."""

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
CORE_SHA = "4c3d7b7769e43e2eb228ebf43d46bef6119c4574"
BASELINE = PROJECT / "docs/COMPATIBILITY_BASELINE.md"
CHECKLIST = PROJECT / "docs/WEB_E2E_PORT_CHECKLIST.md"
README = PROJECT / "README.md"
PLAN = PROJECT / "docs/DEVELOPMENT_PLAN.md"

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

def repo_head(repo: Path) -> str:
    head = (repo / ".git/HEAD").read_text().strip()
    if head.startswith("ref: "):
        return (repo / ".git" / head.removeprefix("ref: ")).read_text().strip()
    return head


def main() -> int:
    failures: list[str] = []
    baseline = BASELINE.read_text()
    checklist = CHECKLIST.read_text()
    readme = README.read_text()
    plan = PLAN.read_text()

    check(repo_head(UPSTREAM) == HARNESS_SHA, "DeepSeek Harness checkout is not pinned", failures)
    check(repo_head(CORDIS) == CORDIS_SHA, "Cordis checkout is not pinned", failures)
    check(repo_head(CORE) == CORE_SHA, "tessivum-core checkout is not pinned", failures)
    check(HARNESS_SHA in baseline and HARNESS_SHA in checklist and HARNESS_SHA in plan,
          "DeepSeek Harness commit is not frozen consistently", failures)
    check(CORDIS_SHA in plan, "Cordis commit is not frozen in the development plan", failures)
    check(CORE_SHA in (PROJECT / "Cargo.toml").read_text(),
          "tessivum-core dependency revision changed", failures)
    ci_workflow = (PROJECT / ".github/workflows/ci.yml").read_text()
    release_workflow = (PROJECT / ".github/workflows/release.yml").read_text()
    check(ci_workflow.count(f"ref: {CORE_SHA}") == 2,
          "CI tessivum-core checkout revision changed", failures)
    check(release_workflow.count(f"ref: {CORE_SHA}") == 1,
          "release tessivum-core checkout revision changed", failures)
    harness_package = (UPSTREAM / "package.json").read_text()
    check('"version": "0.1.0-rc.5"' in harness_package,
          "DeepSeek Harness package version changed", failures)

    rpc_source = (UPSTREAM / "packages/host/apiproxy/src/api/rpc-map.ts").read_text()
    upstream_rpc = set(re.findall(r"^\s*'([^']+)':", rpc_source, re.M))
    documented_rpc = fenced(baseline, "## 5. Core RPC 面")
    check(len(upstream_rpc) == 52, f"upstream RPC count changed: {len(upstream_rpc)}", failures)
    check(documented_rpc == upstream_rpc, "Core RPC inventory differs from upstream rpc-map.ts", failures)

    current_api = (PROJECT / "src/api.rs").read_text()
    current_routes = set(re.findall(
        r'^\s*"([A-Za-z][A-Za-z0-9]*(?:\.[A-Za-z][A-Za-z0-9]*)+)"\s*=>',
        current_api,
        re.M,
    ))
    implemented = current_routes & upstream_rpc
    missing = upstream_rpc - current_routes
    check(implemented == upstream_rpc,
          f"current Core RPC routes differ: missing={sorted(missing)}", failures)
    check("all 52" in readme,
          "README Core RPC route count is stale", failures)

    check(fenced(baseline, "## 6. Typert Remote contributions") == EXPECTED_REMOTES,
          "Remote contribution inventory changed", failures)
    check(fenced(baseline, "## 7. 转发 Host 事件") == EXPECTED_HOST_EVENTS,
          "forwarded Host event inventory changed", failures)

    protocol = (CORE / "crates/tessivum-node-bridge/src/protocol.rs").read_text()
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

    profile = (UPSTREAM / "packages/bundle/web-app/cordis.patch.yml").read_text()
    roster_section = profile.split("browser plugin roster", 1)[1].split("the agent plane", 1)[0]
    profile_roster = set(re.findall(r"name: '([^']+)'", roster_section))
    web_package = json.loads((PROJECT / "web/package.json").read_text())
    vite_config = (PROJECT / "web/vite.config.ts").read_text()
    source_audit = (PROJECT / "web/scripts/audit-deepseek-source.mjs").read_text()
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
    bundle_builder = (PROJECT / "web/scripts/build-deepseek-client-bundles.mjs").read_text()
    web_lock = (PROJECT / "web/bun.lock").read_text()
    binary = (PROJECT / "src/bin/tessivum.rs").read_text()
    asset_builder = (PROJECT / "build.rs").read_text()
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
    frontend_source = (PROJECT / "src/frontend.rs").read_text()
    check("Sha1::digest" in frontend_source and "/plugins/{id}/client.js?rev={rev}" in frontend_source,
          "Rust boot graph hash or bundle URL differs from the frozen source wire", failures)
    check((PROJECT / "web/src/main.ts").read_text() == (UPSTREAM / "apps/web/src/main.ts").read_text(),
          "Tessivum Web entry differs from pinned upstream source", failures)

    for token in (
        "ContentBlockMap", "FinishReasonMap", "reasoningTokens", "assistant/chunk",
        "assistant/message", "request/header", "llm/retry-started", "Session JSONL Replay",
    ):
        check(token in baseline, f"LLM/Agent contract token missing: {token}", failures)

    upstream_e2e = {path.name for path in (UPSTREAM / "apps/web/tests").glob("*.e2e.ts")}
    ported_e2e = {path.name for path in (PROJECT / "web/tests").glob("*.e2e.ts")}
    listed_e2e = set(re.findall(r"\| \[[ x]\] \| \d+ \| `([^`]+\.e2e\.ts)` \|", checklist))
    completed_e2e = set(re.findall(r"\| \[x\] \| \d+ \| `([^`]+\.e2e\.ts)` \|", checklist))
    check(len(upstream_e2e) == 69, f"upstream Web E2E count changed: {len(upstream_e2e)}", failures)
    check(listed_e2e == upstream_e2e, "Web E2E checklist differs from pinned upstream files", failures)
    check(upstream_e2e <= ported_e2e, "ported Web E2E files omit pinned upstream files", failures)
    check(ported_e2e - upstream_e2e == {"market.e2e.ts"},
          "product Web E2E inventory differs from the first-party market scenario", failures)
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
        f"Web E2E {len(ported_e2e)} ({len(upstream_e2e)} upstream + first-party market)"
    )
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
