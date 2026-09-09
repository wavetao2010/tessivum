#!/usr/bin/env node
import assert from "node:assert/strict";
import { createHash } from "node:crypto";
import { readFile } from "node:fs/promises";
import { createRequire } from "node:module";
import { pathToFileURL } from "node:url";
import { constants as osConstants } from "node:os";
import vm from "node:vm";

const PATCHED_SHA256 = "638f2bcbd541dc3221f56ea3222027f4b65e90de45399af152d547f883e3adb1";
const argv = process.argv.slice(2);
const realPty = argv.includes("--real-pty");
const sourceIndex = argv.indexOf("--source");
const sourcePath = sourceIndex >= 0 ? argv[sourceIndex + 1] : argv.find((value) => !value.startsWith("--"));
if (!sourcePath) {
  throw new Error("usage: check_sidebar_pty.mjs --source <profile-local repaired lib/index.js> [--real-pty]");
}
const source = await readFile(sourcePath, "utf8");
assert.equal(createHash("sha256").update(source).digest("hex"), PATCHED_SHA256, "source is not the pinned Tessivum repair");

function block(begin, end) {
  const start = source.indexOf(begin);
  assert.notEqual(start, -1, `missing ${begin}`);
  const finish = source.indexOf(end, start);
  assert.notEqual(finish, -1, `missing end after ${begin}`);
  return source.slice(start, finish + (end === "\n};" ? end.length : 0));
}

class SidebarError extends Error {
  constructor(code, message, status) {
    super(message);
    this.code = code;
    this.status = status;
  }
}

class FakePty {
  data = new Set();
  exits = new Set();
  writes = [];
  resizes = [];
  kills = 0;
  writeError = null;
  resizeError = null;
  onKill = null;
  onData(listener) {
    this.data.add(listener);
    return { dispose: () => this.data.delete(listener) };
  }
  onExit(listener) {
    this.exits.add(listener);
    return { dispose: () => this.exits.delete(listener) };
  }
  write(text) {
    if (this.writeError !== null) throw this.writeError;
    this.writes.push(text);
  }
  resize(cols, rows) {
    if (this.resizeError !== null) throw this.resizeError;
    this.resizes.push([cols, rows]);
  }
  kill() {
    this.kills += 1;
    this.onKill?.();
  }
  emitExit(exitCode = 0) {
    for (const listener of [...this.exits]) listener({ exitCode, signal: 0 });
  }
}

class FakeNodePty {
  ptys = [];
  spawn() {
    const pty = new FakePty();
    this.ptys.push(pty);
    return pty;
  }
}

class FakeSocket {
  static OPEN = 1;
  readyState = FakeSocket.OPEN;
  bufferedAmount = 0;
  listeners = new Map();
  sent = [];
  closes = [];
  terminations = 0;
  sendError = null;
  closeError = null;
  on(event, listener) {
    const listeners = this.listeners.get(event) ?? new Set();
    listeners.add(listener);
    this.listeners.set(event, listeners);
  }
  off(event, listener) {
    this.listeners.get(event)?.delete(listener);
  }
  send(data) {
    if (this.sendError !== null) throw this.sendError;
    this.sent.push(data);
  }
  close(code, reason) {
    if (this.closeError !== null) throw this.closeError;
    this.closes.push([code, reason]);
    this.readyState = 3;
    this.emit("close");
  }
  terminate() {
    this.terminations += 1;
    this.readyState = 3;
    this.emit("close");
  }
  emit(event, data) {
    for (const listener of [...(this.listeners.get(event) ?? [])]) listener(data);
  }
  message(value) {
    this.emit("message", Buffer.from(value));
  }
  disconnect() {
    this.readyState = 3;
    this.emit("close");
  }
  listenerCount() {
    return [...this.listeners.values()].reduce((total, listeners) => total + listeners.size, 0);
  }
}

function fakeClock() {
  let next = 1;
  const callbacks = new Map();
  const active = new Set();
  return {
    callbacks,
    active,
    setTimeout(callback) {
      const id = next++;
      callbacks.set(id, callback);
      active.add(id);
      return id;
    },
    clearTimeout(id) {
      active.delete(id);
    },
    run(id) {
      active.delete(id);
      callbacks.get(id)?.();
    },
  };
}

function runtime(clock, nodePty, resolveSessionCwd = async (_ctx, _sessionId, cwd) => cwd ?? process.cwd()) {
  const context = vm.createContext({
    Buffer,
    Bun: globalThis.Bun,
    TextDecoder,
    cached: undefined,
    defaultRequire: createRequire(pathToFileURL(sourcePath)),
    Error,
    JSON,
    Map,
    Math,
    Promise,
    Set,
    String,
    URL,
    WebSocket: FakeSocket,
    SidebarError,
    PTY_DEPS_MISSING: "pty-deps-missing",
    TRANSCRIPT_LIMIT: 1 << 20,
    "TRANSCRIPT_LIMIT$1": 1 << 20,
    clampDims: (cols, rows) => ({ cols: Math.max(2, Math.min(1024, Math.floor(cols))), rows: Math.max(2, Math.min(1024, Math.floor(rows))) }),
    ensureSpawnHelper: () => {},
    loadRequiredNodePty: () => nodePty,
    locateNeedle: () => undefined,
    process,
    osConstants,
    queueMicrotask,
    randomUUID: (() => { let id = 0; return () => `uuid-${++id}`; })(),
    sessionCwdOf: resolveSessionCwd,
    shellOverridesOf: () => ({ shell: undefined, shellArgs: undefined }),
    signalNameOf: () => null,
    snapshotOf: (handle) => ({ uuid: handle.uuid, exited: handle.exited }),
    setTimeout: clock?.setTimeout ?? setTimeout,
    clearTimeout: clock?.clearTimeout ?? clearTimeout,
  });
  vm.runInContext(block("function shellSpawnArgs(", "\n//#endregion"), context);
  vm.runInContext(`${block("function spawnBunPty(", "/** The recorded load failure")}\nthis.loadNodePty = loadNodePty;`, context);
  vm.runInContext(`${block("var PtyManager = class {", "\n};")}\nthis.PtyManager = PtyManager;`, context);
  vm.runInContext(`${block("var AgentPtyRegistry = class {", "\n};")}\nthis.AgentPtyRegistry = AgentPtyRegistry;`, context);
  vm.runInContext(block("function deadPtyError(", "async function attachTerminal("), context);
  vm.runInContext(`${block("async function attachTerminal(", "\n/**")}\nthis.attachTerminal = attachTerminal;`, context);
  vm.runInContext(`${block("function pumpAgentTerminal(", "\n//#endregion")}\nthis.pumpAgentTerminal = pumpAgentTerminal;`, context);
  return context;
}

async function deterministicRegression() {
  const clock = fakeClock();
  const nodePty = new FakeNodePty();
  const api = runtime(clock, nodePty);
  const warnings = [];
  const ctx = { logger: { warn: (message) => warnings.push(message) } };
  const manager = new api.PtyManager("shell", 2, [], nodePty);
  const req = (cwd, tab = "one") => ({ url: `/?sessionId=session&tab=${tab}&cwd=${encodeURIComponent(cwd)}` });
  const attach = async (ws, cwd, tab) => api.attachTerminal(ctx, manager, null, ws, req(cwd, tab), { reconnectGraceMs: 30 }, () => ({}));

  const parkedSocket = new FakeSocket();
  await attach(parkedSocket, "/a");
  const first = manager.get("session:one");
  parkedSocket.message('{"type":"park"}');
  parkedSocket.disconnect();
  assert.equal(manager.get(first.key), first, "park must preserve the terminal");
  assert.equal(clock.active.size, 0, "park must not start reconnect grace");

  const refresh = new FakeSocket();
  await attach(refresh, "/a");
  refresh.disconnect();
  const [staleTimer] = clock.active;
  assert.ok(staleTimer, "bare disconnect must start reconnect grace");
  const reconnected = new FakeSocket();
  await attach(reconnected, "/a");
  assert.equal(manager.get(first.key), first, "quick reconnect must reuse the terminal");
  assert.equal(clock.active.size, 0, "quick reconnect must cancel grace");
  clock.run(staleTimer);
  assert.equal(manager.get(first.key), first, "cancelled stale timer must be inert");

  const replacementSocket = new FakeSocket();
  await attach(replacementSocket, "/b");
  const replacement = manager.get(first.key);
  assert.notEqual(replacement, first);
  assert.equal(first.closed, true);
  assert.equal(first.exited, true);
  assert.equal(first.pty.kills, 1);
  assert.equal(reconnected.closes.length, 1, "superseded socket must close");
  assert.equal(reconnected.listenerCount(), 0, "superseded socket must release listeners");
  reconnected.disconnect();
  parkedSocket.disconnect();
  assert.equal(manager.get(first.key), replacement, "old socket close must not kill replacement");

  replacement.pty.emitExit(7);
  replacementSocket.message('{"type":"resize","cols":90,"rows":30}');
  assert.equal(replacement.pty.resizes.length, 0, "exit/resize race must not touch exited descriptor");
  assert.match(String(replacementSocket.sent.at(-1)), /process exited with code 7/, "exit marker must be sent before socket close");
  await Promise.resolve();
  assert.equal(replacement.pty.data.size + replacement.pty.exits.size, 0, "process exit must dispose pty listeners");
  assert.equal(replacementSocket.listenerCount(), 0, "process exit must dispose socket listeners");
  assert.equal(replacementSocket.closes.length, 1, "exited terminal socket must close");

  const closingSocket = new FakeSocket();
  await attach(closingSocket, "/a", "close");
  const closing = manager.get("session:close");
  closing.pty.onKill = () => {
    assert.equal(closing.closed, true, "closed flag must precede kill");
    assert.equal(closing.exited, true, "exited flag must precede kill");
  };
  closingSocket.message('{"type":"close"}');
  closingSocket.message("late write");
  assert.equal(closing.pty.writes.length, 0);
  assert.equal(closing.pty.resizes.length, 0);
  assert.equal(closingSocket.listenerCount(), 0, "close must dispose socket listeners");
  assert.equal(closing.pty.data.size + closing.pty.exits.size, 0, "close must dispose pty listeners");
  assert.equal(manager.close(closing.key, closing), false, "close must be idempotent");

  const brokenSocket = new FakeSocket();
  await attach(brokenSocket, "/a", "broken");
  const broken = manager.get("session:broken");
  broken.pty.resizeError = Object.assign(new Error("resize EBADF"), { code: "EBADF" });
  brokenSocket.message('{"type":"resize","cols":90,"rows":30}');
  assert.equal(manager.get(broken.key), undefined, "known invalid descriptor must retire the handle");
  assert.equal(broken.pty.kills, 1);
  assert.equal(brokenSocket.closes.at(-1)?.[0], 1011);
  assert.match(warnings.at(-1), /resize EBADF/);

  const recoverableSocket = new FakeSocket();
  recoverableSocket.closeError = new Error("close handshake failed");
  await attach(recoverableSocket, "/a", "recoverable");
  const recoverable = manager.get("session:recoverable");
  recoverable.pty.resizeError = new Error("resize backend hiccup");
  recoverableSocket.message('{"type":"resize","cols":90,"rows":30}');
  assert.equal(manager.get(recoverable.key), recoverable, "unknown resize error must preserve the live shell");
  assert.equal(recoverable.closed, false);
  assert.equal(recoverable.pty.kills, 0);
  assert.equal(recoverableSocket.listenerCount(), 0);
  assert.equal(recoverableSocket.terminations, 1, "failed WebSocket close must fall back to terminate");
  const [recoverableTimer] = clock.active;
  assert.ok(recoverableTimer, "recoverable UI failure must enter reconnect grace");
  recoverable.pty.resizeError = null;
  const recoveredSocket = new FakeSocket();
  await attach(recoveredSocket, "/a", "recoverable");
  assert.equal(manager.get(recoverable.key), recoverable, "reconnect must reclaim the existing shell");
  assert.equal(clock.active.size, 0, "reconnect must cancel failure grace");
  recoveredSocket.message("echo still-live");
  assert.deepEqual(recoverable.pty.writes, ["echo still-live"]);
  clock.run(recoverableTimer);
  assert.equal(manager.get(recoverable.key), recoverable, "cancelled failure timer must be inert");

  const pendingCwds = [];
  const racePty = new FakeNodePty();
  const raceApi = runtime(clock, racePty, (_ctx, _sessionId, _cwd) => new Promise((resolve) => pendingCwds.push(resolve)));
  const raceManager = new raceApi.PtyManager("shell", 1, [], racePty);
  const raceReq = (cwd) => ({ url: `/?sessionId=race&tab=one&cwd=${encodeURIComponent(cwd)}` });
  const raceAttach = (ws, cwd) => raceApi.attachTerminal(ctx, raceManager, null, ws, raceReq(cwd), { reconnectGraceMs: 30 }, () => ({}));
  const olderSocket = new FakeSocket();
  const olderAttach = raceAttach(olderSocket, "/older");
  const newerSocket = new FakeSocket();
  const newerAttach = raceAttach(newerSocket, "/newer");
  assert.equal(pendingCwds.length, 2);
  pendingCwds[1]("/newer");
  await newerAttach;
  const liveRaceHandle = raceManager.get("race:one");
  assert.ok(liveRaceHandle);
  assert.equal(racePty.ptys.length, 1);
  pendingCwds[0]("/older");
  await olderAttach;
  assert.equal(raceManager.get("race:one"), liveRaceHandle, "late attach must not replace the newer terminal");
  assert.equal(racePty.ptys.length, 1, "late attach must not spawn an orphan terminal");
  assert.equal(newerSocket.closes.length, 0, "late attach must not dispose the newer socket");
  assert.equal(olderSocket.closes.at(-1)?.[0], 1011, "superseded attach must close its stale socket");
  newerSocket.message("newer owner");
  assert.deepEqual(liveRaceHandle.pty.writes, ["newer owner"]);

  const disconnectedSocket = new FakeSocket();
  const disconnectedAttach = raceAttach(disconnectedSocket, "/disconnected");
  assert.equal(pendingCwds.length, 3);
  disconnectedSocket.disconnect();
  pendingCwds[2]("/disconnected");
  await disconnectedAttach;
  assert.equal(raceManager.get("race:one"), liveRaceHandle, "closed attach must preserve the active terminal");
  assert.equal(racePty.ptys.length, 1, "closed attach must not spawn an orphan terminal");
  assert.equal(newerSocket.closes.length, 0, "closed attach must not dispose the active socket");
  newerSocket.message("still newer owner");
  assert.deepEqual(liveRaceHandle.pty.writes, ["newer owner", "still newer owner"]);
  raceManager.disposeAll();

  const quotaPty = new FakeNodePty();
  const quota = new api.PtyManager("shell", 1, [], quotaPty);
  const quotaHandle = quota.open("quota", "one", "/a", 80, 24);
  assert.throws(() => quota.open("quota", "two", "/a", 80, 24), /terminal limit reached/);
  quota.close(quotaHandle.key, quotaHandle);
  assert.doesNotThrow(() => quota.open("quota", "two", "/a", 80, 24));
  quota.disposeAll();

  const agentPty = new FakeNodePty();
  const registry = new api.AgentPtyRegistry("shell", [], agentPty);
  const exitUuid = registry.create("session", "agent exit", "", "/a");
  const exitingAgent = registry.get(exitUuid);
  const exitingSocket = new FakeSocket();
  api.pumpAgentTerminal(ctx, registry, exitingAgent, exitingSocket);
  exitingAgent.pty.emitExit(0);
  await Promise.resolve();
  assert.equal(exitingAgent.pty.data.size + exitingAgent.pty.exits.size, 0, "agent exit must dispose pty listeners");
  assert.equal(exitingSocket.listenerCount(), 0, "agent exit must dispose socket listeners");
  assert.equal(exitingSocket.closes.length, 1, "exited agent socket must close");
  registry.close(exitUuid, exitingAgent);
  const uuid = registry.create("session", "agent", "", "/a");
  const agent = registry.get(uuid);
  const agentSocket = new FakeSocket();
  api.pumpAgentTerminal(ctx, registry, agent, agentSocket);
  agent.pty.writeError = Object.assign(new Error("write EBADF"), { code: "EBADF" });
  agentSocket.message("late agent input");
  assert.equal(registry.get(uuid), undefined, "known agent descriptor failure must retire the handle");
  assert.equal(agent.closed, true);
  assert.equal(agent.pty.kills, 1);
  assert.equal(agent.pty.data.size + agent.pty.exits.size, 0);
  assert.equal(agentSocket.closes.at(-1)?.[0], 1011);
  assert.match(warnings.at(-1), /write EBADF/);

  const recoverableUuid = registry.create("session", "recoverable agent", "", "/a");
  const recoverableAgent = registry.get(recoverableUuid);
  const recoverableAgentSocket = new FakeSocket();
  api.pumpAgentTerminal(ctx, registry, recoverableAgent, recoverableAgentSocket);
  recoverableAgent.pty.resizeError = new Error("agent resize backend hiccup");
  recoverableAgentSocket.message('{"type":"resize","cols":90,"rows":30}');
  assert.equal(registry.get(recoverableUuid), recoverableAgent, "unknown agent resize error must preserve the shell");
  assert.equal(recoverableAgent.pty.kills, 0);
  assert.equal(recoverableAgentSocket.listenerCount(), 0);
  recoverableAgent.pty.resizeError = null;
  const reclaimedAgentSocket = new FakeSocket();
  api.pumpAgentTerminal(ctx, registry, recoverableAgent, reclaimedAgentSocket);
  reclaimedAgentSocket.message("agent still-live");
  assert.deepEqual(recoverableAgent.pty.writes, ["agent still-live"]);

  const disconnectedAgentSocket = new FakeSocket();
  disconnectedAgentSocket.disconnect();
  await api.attachTerminal(ctx, null, registry, disconnectedAgentSocket, { url: `/?uuid=${recoverableUuid}` }, {}, () => ({}));
  assert.equal(reclaimedAgentSocket.closes.length, 0, "closed agent attach must preserve the active socket");
  assert.equal(recoverableAgent.pty.kills, 0, "closed agent attach must preserve the active terminal");
  reclaimedAgentSocket.message("agent remains owner");
  assert.deepEqual(recoverableAgent.pty.writes, ["agent still-live", "agent remains owner"]);

  recoverableAgent.transcript = "retained output";
  const failedReplaySocket = new FakeSocket();
  failedReplaySocket.sendError = new Error("socket send failed");
  api.pumpAgentTerminal(ctx, registry, recoverableAgent, failedReplaySocket);
  assert.equal(registry.get(recoverableUuid), recoverableAgent, "failed replay must preserve the agent shell");
  assert.equal(recoverableAgent.connection, null, "failed replay must release connection ownership");
  assert.equal(failedReplaySocket.listenerCount(), 0, "failed replay must not leak listeners");
  assert.equal(failedReplaySocket.closes.at(-1)?.[0], 1011);
  registry.disposeAll();

  manager.disposeAll();
  assert.equal(clock.active.size, 0, "disposeAll must clear every timer");
}

async function realPtySmoke() {
  const api = runtime(null, null);
  const nodePty = api.loadNodePty();
  assert.ok(nodePty, "production PTY backend must load");
  const shell = process.platform === "win32" ? (process.env.ComSpec ?? "powershell.exe") : (process.env.SHELL ?? "/bin/sh");
  const manager = new api.PtyManager(shell, 1, [], nodePty);
  const handle = manager.open("real", "one", process.cwd(), 80, 24);
  const tokenParts = ["tessivum-pty", String(process.pid)];
  const token = tokenParts.join("-");
  let subscription;
  let timeout;
  const output = new Promise((resolve, reject) => {
    timeout = setTimeout(() => reject(new Error("real PTY command did not finish")), 5000);
    subscription = handle.pty.onExit(({ exitCode }) => {
      try {
        assert.equal(exitCode, 7, "shell command must actually execute");
        assert.ok(handle.transcript.includes(token), "final output must arrive before exit");
        resolve();
      } catch (error) { reject(error); }
    });
  });
  let closed;
  try {
    handle.pty.resize(90, 30);
    handle.pty.write(process.platform === "win32"
      ? `Write-Output ('{0}-{1}' -f '${tokenParts[0]}', '${tokenParts[1]}'); exit 7\r`
      : `printf '%s-%s\\n' '${tokenParts[0]}' '${tokenParts[1]}'; exit 7\r`);
    await output;
  } finally {
    clearTimeout(timeout);
    subscription?.dispose();
    closed = manager.close(handle.key, handle);
  }
  assert.equal(closed, true);
  assert.equal(handle.closed, true);
  assert.equal(handle.exited, true);
  assert.equal(manager.close(handle.key, handle), false);
  if (process.platform !== "win32") {
    const signaled = nodePty.spawn("/bin/sh", ["-c", "trap '' HUP; printf READY; exec sleep 10"], {
      name: "xterm-256color", cols: 80, rows: 24, cwd: process.cwd(), env: process.env,
    });
    let signalTimer;
    let received = "";
    try {
      await new Promise((resolve, reject) => {
        signalTimer = setTimeout(() => reject(new Error("SIGKILL did not terminate PTY")), 5000);
        signaled.onData(data => {
          received += data;
          if (received.includes("READY")) signaled.kill("SIGKILL");
        });
        signaled.onExit(({ signal }) => {
          try { assert.equal(signal, osConstants.signals.SIGKILL); resolve(); }
          catch (error) { reject(error); }
        });
      });
    } finally {
      clearTimeout(signalTimer);
      try { process.kill(signaled.pid, "SIGKILL"); }
      catch (error) { if (error.code !== "ESRCH") throw error; }
    }
  }
}

await deterministicRegression();
if (realPty) await realPtySmoke();
console.log(`sidebar PTY lifecycle check passed${realPty ? " (real shell output and exit)" : ""}`);
