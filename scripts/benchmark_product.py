#!/usr/bin/env python3
"""Run frozen, process-cold Tessivum product benchmark manifests without building."""

from __future__ import annotations

import argparse
import json
import math
import statistics
import os
import platform
import shutil
import signal
import socket
import subprocess
import sys
import tempfile
import threading
import time
from datetime import datetime, timezone
from pathlib import Path
from string import Template
from typing import Any
from urllib.error import HTTPError, URLError
from urllib.request import ProxyHandler, Request, build_opener

MANIFEST_SCHEMA = "tessivum.product-benchmark-manifest/v1"
RUN_SCHEMA = "tessivum.product-benchmark-run/v1"
BROWSER_SCHEMA = "tessivum.product-benchmark-browser/v1"
PSS_INTERVAL_NS = 100_000_000
HTTP = build_opener(ProxyHandler({}))
RUN_ENVIRONMENT: dict[str, Any] = {}


def now_ns() -> int:
    return time.monotonic_ns()


def timestamp() -> str:
    return datetime.now(timezone.utc).isoformat().replace("+00:00", "Z")


def diagnostic(message: str) -> None:
    print(f"benchmark_product: {message}", file=sys.stderr, flush=True)


def command_version(command: str) -> str | None:
    try:
        completed = subprocess.run([command, "--version"], capture_output=True, check=False, text=True, timeout=5)
    except (OSError, subprocess.SubprocessError):
        return None
    output = (completed.stdout or completed.stderr).strip().splitlines()
    return output[0] if completed.returncode == 0 and output else None


def system_environment() -> dict[str, Any]:
    facts: dict[str, Any] = {
        "system": platform.system(),
        "release": platform.release(),
        "machine": platform.machine(),
        "python": platform.python_version(),
        "cpu": platform.processor() or None,
        "tools": {name: command_version(name) for name in ("rustc", "node", "bun", "pnpm")},
    }
    if sys.platform == "linux":
        try:
            cpuinfo = Path("/proc/cpuinfo").read_text(encoding="utf-8").splitlines()
            facts["cpu"] = next((line.split(":", 1)[1].strip() for line in cpuinfo if line.startswith(("model name", "Hardware"))), facts["cpu"])
            meminfo = Path("/proc/meminfo").read_text(encoding="utf-8").splitlines()
            facts["memoryKiB"] = int(next(line.split()[1] for line in meminfo if line.startswith("MemTotal:")))
        except (OSError, StopIteration, ValueError, IndexError):
            pass
    return facts

def read_json(path: Path) -> dict[str, Any]:
    with path.open(encoding="utf-8") as source:
        value = json.load(source)
    if not isinstance(value, dict):
        raise ValueError(f"manifest must be a JSON object: {path}")
    return value


def require(value: Any, description: str) -> Any:
    if value is None:
        raise ValueError(f"manifest is missing {description}")
    return value


def validate_manifest(path: Path) -> dict[str, Any]:
    manifest = read_json(path)
    if manifest.get("schema") != MANIFEST_SCHEMA:
        raise ValueError(f"unsupported manifest schema in {path}: {manifest.get('schema')!r}")
    if not isinstance(manifest.get("name"), str) or not manifest["name"].strip():
        raise ValueError(f"manifest name must be a non-empty string: {path}")
    workload = require(manifest.get("workload"), "workload")
    if not isinstance(workload, dict) or not isinstance(workload.get("replay"), str):
        raise ValueError(f"manifest workload.replay must be a path: {path}")
    for case in ("headless", "web"):
        arguments = workload.get(case, {}).get("arguments") if isinstance(workload.get(case), dict) else None
        if not isinstance(arguments, list) or not all(isinstance(argument, str) for argument in arguments):
            raise ValueError(f"manifest workload.{case}.arguments must be string arguments: {path}")
    if not isinstance(workload["web"].get("prompt"), str):
        raise ValueError(f"manifest workload.web.prompt must be a string: {path}")
    if workload["web"].get("sessions") != 10:
        raise ValueError(f"manifest workload.web.sessions must be the frozen value 10: {path}")
    environment = require(manifest.get("environment"), "environment")
    if not isinstance(environment, dict) or not isinstance(environment.get("set"), dict):
        raise ValueError(f"manifest environment.set must be an object: {path}")
    if not all(isinstance(key, str) and isinstance(value, str) for key, value in environment["set"].items()):
        raise ValueError(f"manifest environment.set must contain string pairs: {path}")
    if not isinstance(environment.get("unset"), list) or not all(isinstance(key, str) for key in environment["unset"]):
        raise ValueError(f"manifest environment.unset must be a string list: {path}")
    timeouts = require(manifest.get("timeouts"), "timeouts")
    for name in ("headlessSeconds", "webReadySeconds", "browserSeconds", "shutdownSeconds"):
        if not isinstance(timeouts.get(name), int) or timeouts[name] <= 0:
            raise ValueError(f"manifest timeouts.{name} must be a positive integer: {path}")
    readiness = require(manifest.get("readiness"), "readiness")
    if not isinstance(readiness.get("http"), dict) or not isinstance(readiness["http"].get("url"), str):
        raise ValueError(f"manifest readiness.http.url must be a string: {path}")
    if not isinstance(readiness["http"].get("status"), int):
        raise ValueError(f"manifest readiness.http.status must be an integer: {path}")
    if not isinstance(readiness.get("pssStableMilliseconds"), int) or readiness["pssStableMilliseconds"] <= 0:
        raise ValueError(f"manifest readiness.pssStableMilliseconds must be a positive integer: {path}")
    expected = require(manifest.get("expected"), "expected")
    if not isinstance(expected.get("marker"), str) or not expected["marker"]:
        raise ValueError(f"manifest expected.marker must be a non-empty string: {path}")
    surface = require(manifest.get("surface"), "surface")
    if not isinstance(surface, dict) or not isinstance(surface.get("browserPlugins"), dict):
        raise ValueError(f"manifest surface.browserPlugins must be an object: {path}")
    if not isinstance(surface["browserPlugins"].get("enabled"), bool):
        raise ValueError(f"manifest surface.browserPlugins.enabled must be boolean: {path}")
    return manifest


def expand(value: str, variables: dict[str, str]) -> str:
    try:
        return Template(value).substitute(os.environ).format_map(variables)
    except KeyError as error:
        raise ValueError(f"missing required benchmark environment variable: {error.args[0]}") from error


def resolve_path(value: str, manifest_path: Path, variables: dict[str, str]) -> Path:
    path = Path(expand(value, variables))
    return path if path.is_absolute() else (manifest_path.parent / path).resolve()


def environment_for(manifest: dict[str, Any], variables: dict[str, str]) -> tuple[dict[str, str], dict[str, Any]]:
    environment = os.environ.copy()
    declared = {"set": {}, "unset": list(manifest["environment"]["unset"])}
    for key in declared["unset"]:
        environment.pop(key, None)
    for key, value in manifest["environment"]["set"].items():
        expanded = expand(value, variables)
        environment[key] = expanded
        declared["set"][key] = expanded
    return environment, declared


def reserve_port() -> int:
    with socket.socket(socket.AF_INET, socket.SOCK_STREAM) as probe:
        probe.bind(("127.0.0.1", 0))
        return int(probe.getsockname()[1])


def group_pids(pgid: int) -> list[int]:
    if sys.platform != "linux":
        return []
    pids: list[int] = []
    for entry in Path("/proc").iterdir():
        if not entry.name.isdecimal():
            continue
        pid = int(entry.name)
        try:
            if os.getpgid(pid) == pgid:
                pids.append(pid)
        except (ProcessLookupError, PermissionError):
            continue
    return sorted(pids)


def process_tree_pids(root_pid: int) -> list[int]:
    if sys.platform != "linux" or not (Path("/proc") / str(root_pid)).exists():
        return []
    children: dict[int, list[int]] = {}
    for entry in Path("/proc").iterdir():
        if not entry.name.isdecimal():
            continue
        try:
            stat = (entry / "stat").read_text(encoding="utf-8")
            fields = stat[stat.rfind(")") + 2:].split()
            parent = int(fields[1])
        except (OSError, ValueError, IndexError):
            continue
        children.setdefault(parent, []).append(int(entry.name))
    pids = {root_pid}
    pending = [root_pid]
    while pending:
        parent = pending.pop()
        for child in children.get(parent, []):
            if child not in pids:
                pids.add(child)
                pending.append(child)
    return sorted(pids)


def managed_pids(root_pid: int, pgid: int) -> list[int]:
    return sorted(set(process_tree_pids(root_pid)).union(group_pids(pgid)))


def pss_snapshot(root_pid: int, pgid: int, observed: set[int], phase: str) -> dict[str, Any]:
    snapshot: dict[str, Any] = {"phase": phase, "atNs": now_ns()}
    if sys.platform != "linux":
        snapshot.update({"available": False, "reason": "Linux /proc/<pid>/smaps_rollup is unavailable"})
        return snapshot
    pids = managed_pids(root_pid, pgid)
    observed.update(pids)
    if not pids:
        snapshot.update({"available": False, "reason": "no live process in benchmark process tree"})
        return snapshot
    processes: list[dict[str, int]] = []
    errors: list[str] = []
    for pid in pids:
        try:
            pss: int | None = None
            for line in (Path("/proc") / str(pid) / "smaps_rollup").read_text(encoding="utf-8").splitlines():
                if line.startswith("Pss:"):
                    pss = int(line.split()[1])
                    break
            if pss is None:
                raise ValueError("Pss field missing")
            processes.append({"pid": pid, "pssKiB": pss})
        except (OSError, ValueError) as error:
            errors.append(f"{pid}: {error}")
    if errors:
        snapshot.update({"available": False, "reason": "; ".join(errors), "processes": processes})
        return snapshot
    snapshot.update({"available": True, "totalKiB": sum(item["pssKiB"] for item in processes), "processes": processes})
    return snapshot


class CapturedProcess:
    def __init__(self, command: list[str], cwd: Path, environment: dict[str, str]):
        self.command = command
        self.started_at_ns = now_ns()
        self.process = subprocess.Popen(
            command,
            cwd=cwd,
            env=environment,
            stdin=subprocess.DEVNULL,
            stdout=subprocess.PIPE,
            stderr=subprocess.PIPE,
            bufsize=0,
            start_new_session=True,
        )
        self.pgid = self.process.pid
        self.first_stdout_at_ns: int | None = None
        self._stdout = bytearray()
        self._stderr = bytearray()
        self._lock = threading.Lock()
        self._readers = [
            threading.Thread(target=self._read, args=(self.process.stdout, self._stdout, True), daemon=True),
            threading.Thread(target=self._read, args=(self.process.stderr, self._stderr, False), daemon=True),
        ]
        for reader in self._readers:
            reader.start()

    def _read(self, stream: Any, destination: bytearray, stdout: bool) -> None:
        assert stream is not None
        try:
            while True:
                chunk = os.read(stream.fileno(), 65_536)
                if not chunk:
                    return
                with self._lock:
                    if stdout and self.first_stdout_at_ns is None:
                        self.first_stdout_at_ns = now_ns()
                    destination.extend(chunk)
        finally:
            stream.close()

    def finish(self) -> None:
        for reader in self._readers:
            reader.join(timeout=1)

    def stdout(self) -> str:
        with self._lock:
            return bytes(self._stdout).decode("utf-8", "replace")

    def stderr(self) -> str:
        with self._lock:
            return bytes(self._stderr).decode("utf-8", "replace")


def monitor_exit(capture: CapturedProcess, timeout_seconds: int, snapshots: list[dict[str, Any]], observed: set[int], phase: str) -> bool:
    deadline = now_ns() + timeout_seconds * 1_000_000_000
    next_pss = now_ns() + PSS_INTERVAL_NS
    while capture.process.poll() is None:
        current = now_ns()
        if current >= deadline:
            return False
        if current >= next_pss:
            snapshots.append(pss_snapshot(capture.process.pid, capture.pgid, observed, phase))
            next_pss = current + PSS_INTERVAL_NS
        time.sleep(0.01)
    return True


def http_ready(url: str, status: int) -> bool:
    try:
        with HTTP.open(Request(url), timeout=0.5) as response:
            return response.status == status
    except (HTTPError, URLError, OSError, ValueError):
        return False


def wait_for_http(capture: CapturedProcess, url: str, status: int, timeout_seconds: int, snapshots: list[dict[str, Any]], observed: set[int]) -> bool:
    deadline = now_ns() + timeout_seconds * 1_000_000_000
    next_pss = now_ns() + PSS_INTERVAL_NS
    while now_ns() < deadline:
        if capture.process.poll() is not None:
            return False
        if http_ready(url, status):
            return True
        current = now_ns()
        if current >= next_pss:
            snapshots.append(pss_snapshot(capture.process.pid, capture.pgid, observed, "web-starting"))
            next_pss = current + PSS_INTERVAL_NS
        time.sleep(0.02)
    return False


def wait_for_stable_pss(capture: CapturedProcess, milliseconds: int, snapshots: list[dict[str, Any]], observed: set[int], phase: str) -> bool:
    deadline = now_ns() + milliseconds * 1_000_000
    next_pss = now_ns()
    while now_ns() < deadline:
        if capture.process.poll() is not None:
            return False
        current = now_ns()
        if current >= next_pss:
            snapshots.append(pss_snapshot(capture.process.pid, capture.pgid, observed, phase))
            next_pss = current + PSS_INTERVAL_NS
        time.sleep(0.01)
    return capture.process.poll() is None


def terminate_group(capture: CapturedProcess, timeout_seconds: int, observed: set[int]) -> dict[str, Any]:
    started = now_ns()
    members = sorted({*managed_pids(capture.process.pid, capture.pgid), *(pid for pid in observed if sys.platform == "linux" and (Path("/proc") / str(pid)).exists())})
    observed.update(members)
    if capture.process.poll() is None or members:
        try:
            os.killpg(capture.pgid, signal.SIGTERM)
        except ProcessLookupError:
            pass
    for pid in members:
        try:
            os.kill(pid, signal.SIGTERM)
        except ProcessLookupError:
            pass
    def active_processes() -> list[int]:
        if sys.platform != "linux":
            return [] if capture.process.poll() is not None else [capture.process.pid]
        observed.update(managed_pids(capture.process.pid, capture.pgid))
        return sorted(pid for pid in observed if (Path("/proc") / str(pid)).exists())

    deadline = now_ns() + timeout_seconds * 1_000_000_000
    while now_ns() < deadline:
        if not active_processes():
            break
        time.sleep(0.02)
    remaining = active_processes()
    if remaining:
        try:
            os.killpg(capture.pgid, signal.SIGKILL)
        except ProcessLookupError:
            pass
        for pid in remaining:
            try:
                os.kill(pid, signal.SIGKILL)
            except ProcessLookupError:
                pass
        kill_deadline = now_ns() + 2_000_000_000
        while now_ns() < kill_deadline:
            if not active_processes():
                break
            time.sleep(0.02)
    try:
        capture.process.wait(timeout=1)
    except subprocess.TimeoutExpired:
        pass
    capture.finish()
    survivors = sorted({*managed_pids(capture.process.pid, capture.pgid), *(pid for pid in observed if (Path("/proc") / str(pid)).exists())}) if sys.platform == "linux" else []
    finished = now_ns()
    return {
        "startedAtNs": started,
        "finishedAtNs": finished,
        "elapsedNs": finished - started,
        "residueAfterDispose": len(survivors) if sys.platform == "linux" else None,
        "residueStatus": "verified" if sys.platform == "linux" else "unavailable outside Linux",
        "survivors": survivors,
    }


def append_failure(sample: dict[str, Any], stage: str, reason: str) -> None:
    sample["failures"].append({"stage": stage, "reason": reason})


def require_pss(sample: dict[str, Any], snapshot: dict[str, Any], stage: str) -> int | None:
    if snapshot.get("available") is True:
        return snapshot["totalKiB"]
    if sys.platform == "linux":
        append_failure(sample, stage, str(snapshot.get("reason", "PSS snapshot unavailable")))
    return None


def pss_peak(sample: dict[str, Any], snapshots: list[dict[str, Any]], stage: str) -> int | None:
    values = [snapshot["totalKiB"] for snapshot in snapshots if snapshot.get("available") is True]
    if values:
        return max(values)
    if sys.platform == "linux":
        append_failure(sample, stage, "no usable process-tree PSS snapshot")
    return None


def validate_profile(manifest: dict[str, Any], source: Path) -> None:
    expected = manifest["surface"]["browserPlugins"]["plugins"]
    profile = read_json(source / "package.json")
    dependencies = profile.get("dependencies")
    if not isinstance(dependencies, dict) or set(dependencies) != {plugin["name"] for plugin in expected}:
        raise ValueError(f"compatibility profile dependencies do not match the frozen plugin set: {source}")
    bundles = profile.get("dsh", {}).get("profile", {}).get("bundles")
    if not isinstance(bundles, list) or set(bundles) != set(dependencies):
        raise ValueError(f"compatibility profile bundles do not match the frozen plugin set: {source}")
    for plugin in expected:
        metadata = read_json(source / "node_modules" / plugin["name"] / "package.json")
        if metadata.get("name") != plugin["name"] or metadata.get("version") != plugin["version"]:
            raise ValueError(f"compatibility profile package does not match {plugin['name']}@{plugin['version']}")




def fresh_root(manifest: dict[str, Any], manifest_path: Path, profile: bool) -> tuple[Path, Path, Path]:
    root = Path(tempfile.mkdtemp(prefix="tessivum-benchmark-"))
    try:
        workspace = root / "workspace"
        data_dir = root / "data"
        workspace.mkdir()
        data_dir.mkdir()
        if profile and isinstance(manifest["surface"].get("profileSeed"), str):
            source = resolve_path(manifest["surface"]["profileSeed"], manifest_path, {})
            if not source.is_dir():
                raise ValueError(f"prebuilt compatibility profile is missing: {source}")
            validate_profile(manifest, source)
            shutil.copytree(source, data_dir / "plugins", symlinks=True)
        return root, workspace, data_dir
    except Exception:
        shutil.rmtree(root, ignore_errors=True)
        raise


def cleanup_root(root: Path, sample: dict[str, Any], stage: str) -> None:
    try:
        shutil.rmtree(root)
    except OSError as error:
        append_failure(sample, stage, f"unable to remove isolated root {root}: {error}")


def command_record(kind: str, command: list[str], cwd: Path, environment: dict[str, Any]) -> dict[str, Any]:
    return {"kind": kind, "argv": command, "cwd": str(cwd), "environment": environment}


def measure_headless(sample: dict[str, Any], manifest: dict[str, Any], manifest_path: Path, binary: Path) -> None:
    result: dict[str, Any] = {"pssSnapshots": []}
    sample["headless"] = result
    root: Path | None = None
    capture: CapturedProcess | None = None
    observed: set[int] = set()
    try:
        root, workspace, data_dir = fresh_root(manifest, manifest_path, profile=True)
        workload = resolve_path(manifest["workload"]["replay"], manifest_path, {})
        if not workload.is_file():
            raise ValueError(f"recorded workload is missing: {workload}")
        variables = {"data_dir": str(data_dir), "workspace": str(workspace), "workload": str(workload), "port": "0"}
        environment, declared = environment_for(manifest, variables)
        command = [str(binary), *[expand(argument, variables) for argument in manifest["workload"]["headless"]["arguments"]]]
        sample["commands"].append(command_record("headless", command, workspace, declared))
        capture = CapturedProcess(command, workspace, environment)
        result["startedAtNs"] = capture.started_at_ns
        result["pssSnapshots"].append(pss_snapshot(capture.process.pid, capture.pgid, observed, "headless-started"))
        completed = monitor_exit(capture, manifest["timeouts"]["headlessSeconds"], result["pssSnapshots"], observed, "headless-running")
        result["completedAtNs"] = now_ns()
        result["completionElapsedNs"] = result["completedAtNs"] - capture.started_at_ns
        capture.finish()
        result["firstStdoutAtNs"] = capture.first_stdout_at_ns
        result["firstStdoutElapsedNs"] = None if capture.first_stdout_at_ns is None else capture.first_stdout_at_ns - capture.started_at_ns
        result["exitCode"] = capture.process.poll()
        result["stdout"] = capture.stdout()
        result["stderr"] = capture.stderr()
        result["pssSnapshots"].append(pss_snapshot(capture.process.pid, capture.pgid, observed, "headless-completed"))
        result["treePeakPssKiB"] = pss_peak(sample, result["pssSnapshots"], "headless-pss")
        if not completed:
            append_failure(sample, "headless", f"timed out after {manifest['timeouts']['headlessSeconds']} seconds")
        if capture.first_stdout_at_ns is None:
            append_failure(sample, "headless", "no stdout event")
        if capture.process.poll() != 0:
            append_failure(sample, "headless", f"exit status was {capture.process.poll()}")
        if manifest["expected"]["marker"] not in result["stdout"]:
            append_failure(sample, "headless", "expected replay marker was absent from stdout")
    except (OSError, ValueError, subprocess.SubprocessError) as error:
        append_failure(sample, "headless", str(error))
    finally:
        if capture is not None:
            cleanup = terminate_group(capture, manifest["timeouts"]["shutdownSeconds"], observed)
            result["cleanup"] = cleanup
            if cleanup["residueAfterDispose"]:
                append_failure(sample, "headless-cleanup", f"surviving descendants: {cleanup['survivors']}")
            result.setdefault("stdout", capture.stdout())
            result.setdefault("stderr", capture.stderr())
        if root is not None:
            cleanup_root(root, sample, "headless-root-cleanup")


def parse_browser_output(stdout: str) -> dict[str, Any]:
    lines = [line for line in stdout.splitlines() if line.strip()]
    if len(lines) != 1:
        raise ValueError("browser driver did not emit exactly one JSON line")
    try:
        result = json.loads(lines[0])
    except json.JSONDecodeError as error:
        raise ValueError(f"browser driver emitted invalid JSON: {error}") from error
    if not isinstance(result, dict) or result.get("schema") != BROWSER_SCHEMA:
        raise ValueError("browser driver emitted an unsupported result schema")
    if not isinstance(result.get("timestamps"), dict):
        raise ValueError("browser driver omitted timestamps")
    if not isinstance(result.get("errors"), list):
        raise ValueError("browser driver omitted errors")
    return result


def monitor_browser(
    browser: CapturedProcess,
    host: CapturedProcess,
    timeout_seconds: int,
    checkpoint: Path,
    resident_sessions: int,
    browser_snapshots: list[dict[str, Any]],
    host_snapshots: list[dict[str, Any]],
    browser_observed: set[int],
    host_observed: set[int],
) -> tuple[bool, dict[int, dict[str, Any]]]:
    deadline = now_ns() + timeout_seconds * 1_000_000_000
    next_pss = now_ns() + PSS_INTERVAL_NS
    resident: dict[int, dict[str, Any]] = {}

    def capture_resident() -> None:
        for count in (1, resident_sessions):
            if count in resident or not Path(f"{checkpoint}.{count}").is_file():
                continue
            snapshot = pss_snapshot(host.process.pid, host.pgid, host_observed, f"web-{count}-sessions")
            snapshot["residentSessions"] = count
            host_snapshots.append(snapshot)
            resident[count] = snapshot

    while browser.process.poll() is None:
        capture_resident()
        current = now_ns()
        if current >= deadline:
            return False, resident
        if current >= next_pss:
            browser_snapshots.append(pss_snapshot(browser.process.pid, browser.pgid, browser_observed, "browser-running"))
            host_snapshots.append(pss_snapshot(host.process.pid, host.pgid, host_observed, "web-browser-session"))
            next_pss = current + PSS_INTERVAL_NS
        time.sleep(0.01)
    capture_resident()
    return True, resident


def measure_web(sample: dict[str, Any], manifest: dict[str, Any], manifest_path: Path, binary: Path) -> None:
    result: dict[str, Any] = {"pssSnapshots": []}
    sample["web"] = result
    root: Path | None = None
    host: CapturedProcess | None = None
    browser: CapturedProcess | None = None
    host_observed: set[int] = set()
    browser_observed: set[int] = set()
    try:
        root, workspace, data_dir = fresh_root(manifest, manifest_path, profile=True)
        workload = resolve_path(manifest["workload"]["replay"], manifest_path, {})
        if not workload.is_file():
            raise ValueError(f"recorded workload is missing: {workload}")
        port = reserve_port()
        variables = {"data_dir": str(data_dir), "workspace": str(workspace), "workload": str(workload), "port": str(port)}
        environment, declared = environment_for(manifest, variables)
        command = [str(binary), *[expand(argument, variables) for argument in manifest["workload"]["web"]["arguments"]]]
        sample["commands"].append(command_record("web", command, workspace, declared))
        host = CapturedProcess(command, workspace, environment)
        result["startedAtNs"] = host.started_at_ns
        result["pssSnapshots"].append(pss_snapshot(host.process.pid, host.pgid, host_observed, "web-started"))
        ready_url = expand(manifest["readiness"]["http"]["url"], variables)
        if not wait_for_http(host, ready_url, manifest["readiness"]["http"]["status"], manifest["timeouts"]["webReadySeconds"], result["pssSnapshots"], host_observed):
            append_failure(sample, "web", f"HTTP readiness failed at {ready_url}")
            return
        result["httpReadyAtNs"] = now_ns()
        result["readyElapsedNs"] = result["httpReadyAtNs"] - host.started_at_ns
        result["createElapsedNs"] = result["readyElapsedNs"]
        if not wait_for_stable_pss(host, manifest["readiness"]["pssStableMilliseconds"], result["pssSnapshots"], host_observed, "web-idle-stabilizing"):
            append_failure(sample, "web", "host exited during idle PSS stabilization")
            return
        idle = pss_snapshot(host.process.pid, host.pgid, host_observed, "web-idle")
        result["pssSnapshots"].append(idle)
        result["treeIdlePssKiB"] = require_pss(sample, idle, "web-idle-pss")

        surface = manifest["surface"]
        browser_enabled = surface.get("browser", {}).get("enabled", surface["browserPlugins"]["enabled"])
        if browser_enabled:
            browser_script = Path(__file__).with_name("benchmark_browser.mjs")
            checkpoint = root / "browser-session"
            resident_sessions = manifest["workload"]["web"]["sessions"]
            browser_command = [
                os.environ.get("BUN", "bun"), str(browser_script), "--url", ready_url,
                "--prompt", manifest["workload"]["web"]["prompt"],
                "--marker", manifest["expected"]["marker"],
                "--sessions", str(resident_sessions),
                "--settle-ms", str(manifest["readiness"]["pssStableMilliseconds"]),
                "--checkpoint", str(checkpoint),
                "--timeout-ms", str(manifest["timeouts"]["browserSeconds"] * 1000),
            ]
            browser_environment = os.environ.copy()
            for key in manifest["environment"]["unset"]:
                browser_environment.pop(key, None)
            browser_declared = {"set": {"BUN": browser_command[0]}, "unset": manifest["environment"]["unset"]}
            if "TESSIVUM_CHROMIUM" in browser_environment:
                browser_declared["set"]["TESSIVUM_CHROMIUM"] = browser_environment["TESSIVUM_CHROMIUM"]
            sample["commands"].append(command_record("browser", browser_command, workspace, browser_declared))
            browser = CapturedProcess(browser_command, workspace, browser_environment)
            browser_result: dict[str, Any] = {"startedAtNs": browser.started_at_ns, "pssSnapshots": [pss_snapshot(browser.process.pid, browser.pgid, browser_observed, "browser-started")]}
            result["browser"] = browser_result
            completed, resident_snapshots = monitor_browser(
                browser, host, manifest["timeouts"]["browserSeconds"], checkpoint, resident_sessions,
                browser_result["pssSnapshots"], result["pssSnapshots"], browser_observed, host_observed,
            )
            browser_result["residentSessionSnapshots"] = {str(count): snapshot for count, snapshot in resident_snapshots.items()}
            browser_result["completedAtNs"] = now_ns()
            browser_result["completionElapsedNs"] = browser_result["completedAtNs"] - browser.started_at_ns
            browser.finish()
            browser_result["exitCode"] = browser.process.poll()
            browser_result["stdout"] = browser.stdout()
            browser_result["stderr"] = browser.stderr()
            if not completed:
                append_failure(sample, "browser", f"timed out after {manifest['timeouts']['browserSeconds']} seconds")
            if browser.process.poll() != 0:
                append_failure(sample, "browser", f"exit status was {browser.process.poll()}")
            try:
                probe = parse_browser_output(browser_result["stdout"])
                browser_result["result"] = probe
                times = probe["timestamps"]
                if not isinstance(times.get("startedMs"), int) or not isinstance(times.get("composerEnabledMs"), int):
                    raise ValueError("browser driver omitted enabled-composer timestamps")
                browser_result["composerEnabledElapsedMs"] = times["composerEnabledMs"] - times["startedMs"]
                if "markerSeenMs" in times:
                    browser_result["markerCompletionElapsedMs"] = times["markerSeenMs"] - times["startedMs"]
                if probe["errors"]:
                    append_failure(sample, "browser", f"browser driver errors: {probe['errors']}")
                if probe.get("promptSubmitted") is not True:
                    append_failure(sample, "browser", "browser driver did not submit the replay prompt")
                if "markerSeenMs" not in times:
                    append_failure(sample, "browser", "browser driver did not observe the replay marker")
                if probe.get("sessionsCompleted") != resident_sessions:
                    append_failure(sample, "browser", f"browser completed {probe.get('sessionsCompleted')} of {resident_sessions} resident sessions")
                if 1 not in resident_snapshots or resident_sessions not in resident_snapshots:
                    append_failure(sample, "browser", "browser did not expose one- and ten-session PSS checkpoints")
            except ValueError as error:
                append_failure(sample, "browser", str(error))
        if not wait_for_stable_pss(host, manifest["readiness"]["pssStableMilliseconds"], result["pssSnapshots"], host_observed, "web-session-stabilizing"):
            append_failure(sample, "web", "host exited during session PSS stabilization")
            return
        ten_sessions = pss_snapshot(host.process.pid, host.pgid, host_observed, "web-ten-sessions")
        result["pssSnapshots"].append(ten_sessions)
        result["treeTenSessionPssKiB"] = require_pss(sample, ten_sessions, "web-ten-session-pss")
        one_session = result.get("browser", {}).get("residentSessionSnapshots", {}).get("1")
        if isinstance(one_session, dict):
            result["treeOneSessionPssKiB"] = require_pss(sample, one_session, "web-one-session-pss")
        else:
            append_failure(sample, "web-one-session-pss", "one-session checkpoint is missing")
            result["treeOneSessionPssKiB"] = None
        idle_pss = result.get("treeIdlePssKiB")
        one_pss = result.get("treeOneSessionPssKiB")
        ten_pss = result.get("treeTenSessionPssKiB")
        if all(isinstance(value, int) for value in (idle_pss, one_pss, ten_pss)):
            result["treeOneSessionDeltaFromIdleKiB"] = one_pss - idle_pss
            result["treeTenSessionDeltaFromIdleKiB"] = ten_pss - idle_pss
            result["treeTenSessionPerSessionKiB"] = (ten_pss - idle_pss) / resident_sessions
        if host.process.poll() is not None:
            append_failure(sample, "web", f"host exited before disposal with status {host.process.poll()}")
    except (OSError, ValueError, subprocess.SubprocessError) as error:
        append_failure(sample, "web", str(error))
    finally:
        if browser is not None:
            browser_cleanup = terminate_group(browser, manifest["timeouts"]["shutdownSeconds"], browser_observed)
            result.setdefault("browser", {})["cleanup"] = browser_cleanup
            if browser_cleanup["residueAfterDispose"]:
                append_failure(sample, "browser-cleanup", f"surviving descendants: {browser_cleanup['survivors']}")
            result.setdefault("browser", {}).setdefault("stdout", browser.stdout())
            result.setdefault("browser", {}).setdefault("stderr", browser.stderr())
        if host is not None:
            result["pssSnapshots"].append(pss_snapshot(host.process.pid, host.pgid, host_observed, "web-before-dispose"))
            cleanup = terminate_group(host, manifest["timeouts"]["shutdownSeconds"], host_observed)
            result["cleanup"] = cleanup
            result["disposeElapsedNs"] = cleanup["elapsedNs"]
            result["residueAfterDispose"] = cleanup["residueAfterDispose"]
            if cleanup["residueAfterDispose"]:
                append_failure(sample, "web-cleanup", f"surviving descendants: {cleanup['survivors']}")
            result.setdefault("stdout", host.stdout())
            result.setdefault("stderr", host.stderr())
        if root is not None:
            cleanup_root(root, sample, "web-root-cleanup")


def run_sample(manifest: dict[str, Any], manifest_path: Path, runtime: dict[str, str], repetition: int) -> dict[str, Any]:
    sample: dict[str, Any] = {
        "schema": "tessivum.product-benchmark-sample/v1",
        "manifest": manifest["name"],
        "runtime": runtime,
        "repetition": repetition,
        "startedAt": timestamp(),
        "commands": [],
        "failures": [],
    }
    binary = Path(runtime["binary"]).resolve()
    if not binary.is_file() or not os.access(binary, os.X_OK):
        append_failure(sample, "binary", f"not an executable file: {binary}")
    else:
        try:
            measure_headless(sample, manifest, manifest_path, binary)
            measure_web(sample, manifest, manifest_path, binary)
        except Exception as error:
            append_failure(sample, "driver", f"unexpected driver error: {error}")
    sample["finishedAt"] = timestamp()
    sample["success"] = not sample["failures"]
    return sample


def percentile95(values: list[int | float]) -> int | float:
    return sorted(values)[math.ceil(len(values) * 0.95) - 1]


def summaries(samples: list[dict[str, Any]]) -> list[dict[str, Any]]:
    metric_paths = {
        "headless.treePeakPssKiB": ("headless", "treePeakPssKiB"),
        "headless.firstStdoutElapsedNs": ("headless", "firstStdoutElapsedNs"),
        "headless.completionElapsedNs": ("headless", "completionElapsedNs"),
        "web.readyElapsedNs": ("web", "readyElapsedNs"),
        "web.browser.composerEnabledElapsedMs": ("web", "browser", "composerEnabledElapsedMs"),
        "web.browser.markerCompletionElapsedMs": ("web", "browser", "markerCompletionElapsedMs"),
        "web.treeIdlePssKiB": ("web", "treeIdlePssKiB"),
        "web.treeOneSessionPssKiB": ("web", "treeOneSessionPssKiB"),
        "web.treeOneSessionDeltaFromIdleKiB": ("web", "treeOneSessionDeltaFromIdleKiB"),
        "web.treeTenSessionPssKiB": ("web", "treeTenSessionPssKiB"),
        "web.treeTenSessionDeltaFromIdleKiB": ("web", "treeTenSessionDeltaFromIdleKiB"),
        "web.treeTenSessionPerSessionKiB": ("web", "treeTenSessionPerSessionKiB"),
        "web.createElapsedNs": ("web", "createElapsedNs"),
        "web.disposeElapsedNs": ("web", "disposeElapsedNs"),
        "web.residueAfterDispose": ("web", "residueAfterDispose"),
    }
    grouped: dict[tuple[str, str], list[dict[str, Any]]] = {}
    for sample in samples:
        grouped.setdefault((sample["manifest"], sample["runtime"]["id"]), []).append(sample)
    output: list[dict[str, Any]] = []
    for (manifest, runtime), group in sorted(grouped.items()):
        metrics: dict[str, Any] = {}
        for name, path in metric_paths.items():
            values: list[int | float] = []
            for sample in group:
                value: Any = sample
                for key in path:
                    if not isinstance(value, dict) or key not in value:
                        value = None
                        break
                    value = value[key]
                if sample["success"] and isinstance(value, (int, float)) and not isinstance(value, bool):
                    values.append(value)
            if values:
                metrics[name] = {
                    "successfulSamples": len(values),
                    "median": statistics.median(values),
                    "p95": percentile95(values),
                    "min": min(values),
                    "max": max(values),
                }
        output.append({
            "manifest": manifest,
            "runtime": runtime,
            "successfulSamples": sum(sample["success"] for sample in group),
            "failedSamples": sum(not sample["success"] for sample in group),
            "metrics": metrics,
        })
    return output


def write_raw(path: Path, document: dict[str, Any]) -> None:
    path.parent.mkdir(parents=True, exist_ok=True)
    temporary = path.with_name(f".{path.name}.{os.getpid()}.tmp")
    with temporary.open("w", encoding="utf-8") as output:
        json.dump(document, output, indent=2, sort_keys=True)
        output.write("\n")
    os.replace(temporary, path)


def parse_binary(value: str) -> tuple[str, str]:
    label, separator, path = value.partition("=")
    if not separator:
        path = value
        label = Path(value).name
    if not label or not path:
        raise argparse.ArgumentTypeError("--binary must be PATH or NAME=PATH")
    return label, path


def parser() -> argparse.ArgumentParser:
    command = argparse.ArgumentParser(description="Run frozen product benchmark manifests without building or installing.")
    command.add_argument("--manifest", action="append", required=True, type=Path, help="frozen benchmark manifest (repeatable)")
    command.add_argument("--binary", action="append", required=True, type=parse_binary, help="runtime binary as PATH or NAME=PATH (repeatable)")
    command.add_argument("--samples", type=int, default=30, help="process-cold repetitions per manifest/runtime (default: 30)")
    command.add_argument("--interleave", action="store_true", help="alternate runtime launch order for each repetition")
    command.add_argument("--raw-out", type=Path, help="atomically checkpoint retained raw samples to this JSON path")
    command.add_argument("--publication", action="store_true", help="require Linux PSS data suitable for publication")
    return command


def document(arguments: argparse.Namespace, manifests: list[tuple[Path, dict[str, Any]]], runtimes: list[dict[str, str]], samples: list[dict[str, Any]], started: str, finished: str | None = None) -> dict[str, Any]:
    failures = [sample for sample in samples if not sample["success"]]
    diagnostics: list[dict[str, str]] = []
    if sys.platform != "linux":
        diagnostics.append({"code": "PSS_UNAVAILABLE", "message": "Linux /proc/<pid>/smaps_rollup is unavailable; no PSS numeric data was emitted."})
    return {
        "schema": RUN_SCHEMA,
        "status": "running" if finished is None else ("passed" if not failures else "failed"),
        "startedAt": started,
        **({"finishedAt": finished} if finished is not None else {}),
        "publication": arguments.publication,
        "arguments": {
            "manifests": [str(path) for path, _ in manifests],
            "binaries": runtimes,
            "samples": arguments.samples,
            "interleave": arguments.interleave,
        },
        "environment": RUN_ENVIRONMENT,
        "manifests": [{"path": str(path), "name": manifest["name"], "revisions": manifest.get("revisions", {})} for path, manifest in manifests],
        "diagnostics": diagnostics,
        "rawSamples": samples,
        "summaries": summaries(samples),
        "failureCount": len(failures),
    }


def main() -> int:
    arguments = parser().parse_args()
    if arguments.samples <= 0:
        parser().error("--samples must be a positive integer")
    if arguments.publication and arguments.samples < 30:
        parser().error("--publication requires at least 30 samples per manifest/runtime")
    if arguments.publication and sys.platform != "linux":
        parser().error("--publication requires Linux for /proc/<pid>/smaps_rollup PSS")
    manifests: list[tuple[Path, dict[str, Any]]] = []
    names: set[str] = set()
    for requested in arguments.manifest:
        path = requested.resolve()
        manifest = validate_manifest(path)
        if manifest["name"] in names:
            parser().error(f"duplicate manifest name: {manifest['name']}")
        names.add(manifest["name"])
        manifests.append((path, manifest))
    workload = json.dumps(manifests[0][1]["workload"], sort_keys=True, separators=(",", ":"))
    if any(json.dumps(manifest["workload"], sort_keys=True, separators=(",", ":")) != workload for _, manifest in manifests[1:]):
        parser().error("all manifests must declare the identical frozen workload")
    runtimes = [{"id": label, "binary": str(Path(binary).resolve())} for label, binary in arguments.binary]
    if len({runtime["id"] for runtime in runtimes}) != len(runtimes):
        parser().error("runtime labels from --binary must be unique")

    global RUN_ENVIRONMENT
    RUN_ENVIRONMENT = system_environment()
    started = timestamp()
    samples: list[dict[str, Any]] = []

    def checkpoint() -> None:
        if arguments.raw_out is not None:
            write_raw(arguments.raw_out, document(arguments, manifests, runtimes, samples, started))

    try:
        if arguments.interleave:
            for repetition in range(1, arguments.samples + 1):
                for manifest_index, (path, manifest) in enumerate(manifests):
                    offset = (repetition - 1 + manifest_index) % len(runtimes)
                    for runtime in runtimes[offset:] + runtimes[:offset]:
                        diagnostic(f"{manifest['name']} {runtime['id']} sample {repetition}/{arguments.samples}")
                        samples.append(run_sample(manifest, path, runtime, repetition))
                        checkpoint()
        else:
            for path, manifest in manifests:
                for runtime in runtimes:
                    for repetition in range(1, arguments.samples + 1):
                        diagnostic(f"{manifest['name']} {runtime['id']} sample {repetition}/{arguments.samples}")
                        samples.append(run_sample(manifest, path, runtime, repetition))
                        checkpoint()
    except KeyboardInterrupt:
        final = document(arguments, manifests, runtimes, samples, started, timestamp())
        if arguments.raw_out is not None:
            write_raw(arguments.raw_out, final)
        print(json.dumps(final, sort_keys=True))
        return 130

    final = document(arguments, manifests, runtimes, samples, started, timestamp())
    if arguments.raw_out is not None:
        write_raw(arguments.raw_out, final)
    print(json.dumps(final, sort_keys=True))
    return 0 if final["failureCount"] == 0 else 1


if __name__ == "__main__":
    raise SystemExit(main())

