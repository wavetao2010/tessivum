#!/usr/bin/env node
import assert from "node:assert/strict";
import { spawnSync } from "node:child_process";
import { createHash } from "node:crypto";
import { chmod, mkdir, mkdtemp, readFile, rm, writeFile } from "node:fs/promises";
import { accessSync, constants as fsConstants, existsSync, statSync } from "node:fs";
import { createRequire } from "node:module";
import { pathToFileURL } from "node:url";
import { delimiter, dirname, isAbsolute, join, resolve } from "node:path";
import { constants as osConstants, tmpdir, userInfo } from "node:os";
import vm from "node:vm";

const PATCHED_SHA256 = "9d3ecea3921ecee81d338074784eb86f40faa76e3546cea53c49e6aab31b46ff";
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
  spawn(shell, args, options) {
    const pty = new FakePty();
    pty.shell = shell;
    pty.args = Array.from(args);
    pty.options = options;
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

function runtime(clock, nodePty, resolveSessionCwd = async (_ctx, _sessionId, cwd) => cwd ?? process.cwd(), options = {}) {
  const environment = options.env ?? process.env;
  const context = vm.createContext({
    accessSync,
    Buffer,
    Bun: globalThis.Bun,
    TextDecoder,
    cached: undefined,
    defaultRequire: createRequire(pathToFileURL(sourcePath)),
    delimiter,
    Error,
    existsSync,
    fsConstants,
    isAbsolute,
    join,
    JSON,
    Map,
    Math,
    Promise,
    resolve,
    Set,
    statSync,
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
    process: { cwd: () => process.cwd(), env: environment, platform: process.platform },
    osConstants,
    queueMicrotask,
    randomUUID: (() => { let id = 0; return () => `uuid-${++id}`; })(),
    sessionCwdOf: resolveSessionCwd,
    signalNameOf: () => null,
    snapshotOf: (handle) => ({ uuid: handle.uuid, exited: handle.exited }),
    setTimeout: clock?.setTimeout ?? setTimeout,
    clearTimeout: clock?.clearTimeout ?? clearTimeout,
    userInfo: options.userInfo ?? userInfo,
  });
  vm.runInContext(`${block("function windowsPwshCandidateDirs(", "\n}")}\n}`, context);
  vm.runInContext(`${block("function resolveShellExecutable(", "\n//#endregion")}\nthis.resolveShellExecutable = resolveShellExecutable;\nthis.defaultShell = defaultShell;\nthis.shellDisplayName = shellDisplayName;\nthis.shellSpawnArgs = shellSpawnArgs;`, context);
  vm.runInContext(`${block("function spawnBunPty(", "/** The recorded load failure")}\nthis.loadNodePty = loadNodePty;`, context);
  vm.runInContext(`${block("var PtyManager = class {", "\n};")}\nthis.PtyManager = PtyManager;`, context);
  vm.runInContext(`${block("var AgentPtyRegistry = class {", "\n};")}\nthis.AgentPtyRegistry = AgentPtyRegistry;`, context);
  vm.runInContext(`${block("function shellOverridesOf(", "\nfunction parseLoopbackAllowlist(")}\nthis.shellOverridesOf = shellOverridesOf;`, context);
  vm.runInContext(block("function deadPtyError(", "async function attachTerminal("), context);
  vm.runInContext(`${block("async function attachTerminal(", "\n/**")}\nthis.attachTerminal = attachTerminal;`, context);
  vm.runInContext(`${block("function pumpAgentTerminal(", "\n//#endregion")}\nthis.pumpAgentTerminal = pumpAgentTerminal;`, context);
  return context;
}
async function writeExecutable(path) {
  await mkdir(dirname(path), { recursive: true });
  await writeFile(path, "#!/bin/sh\nexit 0\n");
  await chmod(path, 0o755);
}

async function shellResolverRegression() {
  const windowsApi = runtime(null, new FakeNodePty());
  assert.equal(windowsApi.defaultShell({ platform: "win32", explicit: " custom.exe ", env: {}, exists: () => false }), "custom.exe");
  assert.equal(windowsApi.defaultShell({ platform: "win32", env: { DSH_SIDEBAR_SHELL: " pwsh-custom.exe " }, exists: () => false }), "pwsh-custom.exe");
  const pwsh = join("C:\\PowerShell", "pwsh.exe");
  assert.equal(windowsApi.defaultShell({ platform: "win32", env: { PATH: "C:\\PowerShell" }, exists: candidate => candidate === pwsh }), pwsh);
  assert.equal(windowsApi.defaultShell({ platform: "win32", env: {}, exists: () => false }), "powershell.exe");

  if (process.platform === "win32") return;
  const environment = { PATH: ["/bin", "/usr/bin"].join(delimiter) };
  const loginCases = [
    ["unknown", () => ({ shell: "unknown" })],
    ["empty", () => ({ shell: "" })],
    ["throwing", () => { throw new Error("passwd lookup failed"); }],
    ["non-executable", () => ({ shell: "/etc/hosts" })],
  ];
  for (const [label, login] of loginCases) {
    const api = runtime(null, new FakeNodePty(), undefined, { env: environment, userInfo: login });
    assert.equal(api.defaultShell({ platform: "linux" }), "/bin/bash", `${label} login shell must use the POSIX fallback`);
  }

  const api = runtime(null, new FakeNodePty(), undefined, { env: environment, userInfo: () => ({ shell: "/bin/sh" }) });
  assert.equal(api.defaultShell({ platform: "linux", env: { ...environment, SHELL: "unknown" } }), "/bin/sh", "invalid SHELL must fall through to the login shell");
  assert.equal(api.defaultShell({ platform: "linux", env: { ...environment, SHELL: " /bin/bash " } }), "/bin/bash", "a valid SHELL must be trimmed and win over the login shell");
  assert.equal(api.defaultShell({ platform: "linux", explicit: " /bin/sh ", env: { ...environment, SHELL: "/bin/bash" } }), "/bin/sh", "deployment shell must win over automatic candidates");
  assert.throws(() => api.resolveShellExecutable("   "), /empty/i);
  assert.throws(() => api.defaultShell({ platform: "linux", explicit: "missing-tessivum-shell", env: environment }), /executable/i);
  assert.throws(() => api.defaultShell({ platform: "linux", explicit: process.cwd(), env: environment }), /executable/i, "directories are not shell executables");
  assert.throws(() => api.defaultShell({ platform: "linux", explicit: "/etc/hosts", env: environment }), /executable/i, "non-executable files are not shells");

  const fixtureRoot = await mkdtemp(join(tmpdir(), "tessivum-sidebar-shell-"));
  try {
    const shellName = "tessivum-fixture-shell";
    const uiCwd = join(fixtureRoot, "ui");
    const modelCwd = join(fixtureRoot, "model");
    const uiShell = join(uiCwd, "tools", shellName);
    const modelShell = join(modelCwd, "tools", shellName);
    const cwdOnlyName = `${shellName}-cwd-only`;
    const cwdOnlyShell = join(uiCwd, cwdOnlyName);
    const spacedShell = join(uiCwd, "shell with trailing spaces   ");
    await Promise.all([uiShell, modelShell, cwdOnlyShell, spacedShell].map(writeExecutable));

    const uiPty = new FakeNodePty();
    const uiApi = runtime(null, uiPty, undefined, { env: { PATH: "tools" } });
    const manager = new uiApi.PtyManager("missing-deployment-shell", 8, [], uiPty);
    const bareHandle = manager.open("fixture", "bare", uiCwd, 80, 24, shellName);
    assert.equal(manager.get(bareHandle.key), bareHandle, "a PATH-resolved UI shell must produce a live handle");
    assert.equal(bareHandle.pty.shell, uiShell, "the UI terminal must select the executable under its session cwd");
    assert.equal(spawnSync(bareHandle.pty.shell, [], { cwd: uiCwd }).status, 0, "the PATH-resolved UI executable must run successfully");
    const relativeHandle = manager.open("fixture", "relative", uiCwd, 80, 24, `./tools/${shellName}`);
    assert.equal(relativeHandle.pty.shell, uiShell, "an explicit relative UI shell must resolve from the session cwd");
    assert.throws(() => manager.open("fixture", "invalid", uiCwd, 80, 24), /executable/i, "an invalid selected deployment shell must fail when a terminal is created");

    const modelPty = new FakeNodePty();
    const modelApi = runtime(null, modelPty, undefined, { env: { PATH: "tools" } });
    const registry = new modelApi.AgentPtyRegistry("missing-deployment-shell", [], modelPty);
    const uuid = registry.create("fixture", "bare model shell", "", modelCwd, 80, 24, shellName);
    const modelHandle = registry.get(uuid);
    assert.equal(modelHandle.pty.shell, modelShell, "the model terminal must select the same-name executable under its own session cwd");
    assert.equal(spawnSync(modelHandle.pty.shell, [], { cwd: modelCwd }).status, 0, "the PATH-resolved model executable must run successfully");
    assert.throws(() => registry.create("fixture", "invalid model shell", "", modelCwd), /executable/i, "an invalid selected deployment shell must fail when a model terminal is created");

    const cwdPty = new FakeNodePty();
    const cwdApi = runtime(null, cwdPty, undefined, { env: { PATH: "" } });
    const cwdManager = new cwdApi.PtyManager("missing-deployment-shell", 2, [], cwdPty);
    const cwdHandle = cwdManager.open("fixture", "empty-path", uiCwd, 80, 24, cwdOnlyName);
    assert.equal(cwdHandle.pty.shell, cwdOnlyShell, "an intentional empty PATH component must search the terminal cwd");
    const missingPathApi = runtime(null, new FakeNodePty(), undefined, { env: {} });
    const missingPathManager = new missingPathApi.PtyManager(cwdOnlyName, 1, [], new FakeNodePty());
    assert.throws(() => missingPathManager.open("fixture", "missing-path", uiCwd, 80, 24), /executable/i, "an absent PATH must not accidentally select a cwd executable");

    const settingsPty = new FakeNodePty();
    const settingsApi = runtime(null, settingsPty, undefined, { env: environment });
    const settingsManager = new settingsApi.PtyManager("missing-deployment-shell", 2, [], settingsPty);
    const settingsSocket = new FakeSocket();
    const overrideArgs = ["--first", "second"];
    await settingsApi.attachTerminal(
      { logger: { warn: () => {} } },
      settingsManager,
      null,
      settingsSocket,
      { url: `/?sessionId=settings&tab=raw&cwd=${encodeURIComponent(uiCwd)}` },
      { reconnectGraceMs: 30 },
      () => ({ get: () => ({ value: { terminalShell: spacedShell, terminalShellArgs: overrideArgs.join(" ") } }) }),
    );
    const settingsHandle = settingsManager.get("settings:raw");
    assert.ok(settingsHandle, "a valid saved override must open despite an invalid deployment shell");
    assert.equal(settingsHandle.pty.shell, spacedShell, "a valid saved shell path ending in spaces must reach the UI terminal unchanged");
    assert.deepEqual(settingsHandle.pty.args, overrideArgs, "saved shell arguments must retain argv boundaries");
    assert.equal(settingsHandle.closed, false, "the saved override must produce a live handle");
    assert.equal(spawnSync(settingsHandle.pty.shell, settingsHandle.pty.args, { cwd: uiCwd }).status, 0, "the raw saved executable and argv must run successfully");

    const warnings = [];
    const longSocket = new FakeSocket();
    const longShell = "界".repeat(100);
    await settingsApi.attachTerminal(
      { logger: { warn: message => warnings.push(message) } },
      settingsManager,
      null,
      longSocket,
      { url: `/?sessionId=settings&tab=invalid&cwd=${encodeURIComponent(uiCwd)}` },
      { reconnectGraceMs: 30 },
      () => ({ get: () => ({ value: { terminalShell: longShell } }) }),
    );
    const [closeCode, closeReason] = longSocket.closes.at(-1);
    assert.equal(closeCode, 1011, "an invalid configured shell must retain the terminal failure code");
    assert.ok(Buffer.byteLength(closeReason) <= 123, "a WebSocket close reason must fit the protocol byte limit");
    assert.match(closeReason, /executable/i, "a truncated close reason must retain an actionable diagnosis");
    assert.equal(closeReason.includes("�"), false, "a truncated close reason must end on a UTF-8 codepoint boundary");
    assert.equal(longSocket.terminations, 0, "a bounded diagnostic must use the WebSocket close handshake");
    assert.ok(warnings.at(-1).includes(longShell), "server diagnostics must retain the complete invalid shell path");


    settingsManager.disposeAll();
    cwdManager.disposeAll();
    registry.disposeAll();
    manager.disposeAll();
  } finally {
    await rm(fixtureRoot, { recursive: true, force: true });
  }
}

function terminalShellRegression() {
  const nodePty = new FakeNodePty();
  const api = runtime(null, nodePty);
  const deploymentShell = process.platform === "win32" ? "powershell.exe" : "/bin/sh";
  const overrideShell = process.execPath;
  const overrideArgs = ["--first", "two words", "semi;colon"];
  const manager = new api.PtyManager(deploymentShell, 4, [], nodePty);
  manager.open("choices", "default", process.cwd(), 80, 24);
  assert.equal(nodePty.ptys[0].shell, deploymentShell, "UI terminal must use the deployment shell by default");
  const overridden = manager.open("choices", "override", process.cwd(), 80, 24, overrideShell, overrideArgs);
  assert.equal(nodePty.ptys[1].shell, overrideShell, "UI override must win over the deployment shell");
  assert.deepEqual(nodePty.ptys[1].args, overrideArgs, "UI shell arguments must retain argv boundaries");
  if (process.platform !== "win32") {
    assert.equal(manager.open("choices", "override", process.cwd(), 80, 24, "missing-tessivum-shell"), overridden, "a live UI terminal must be reused before changed settings are revalidated");
    assert.throws(() => manager.open("choices", "invalid", process.cwd(), 80, 24, "missing-tessivum-shell"), /executable/i);
  }

  const registry = new api.AgentPtyRegistry(deploymentShell, [], nodePty);
  const uuid = registry.create("choices", "model override", "", process.cwd(), 80, 24, overrideShell, overrideArgs);
  const modelPty = registry.get(uuid).pty;
  assert.equal(modelPty.shell, overrideShell, "model terminal override must win over the deployment shell");
  assert.deepEqual(modelPty.args, overrideArgs, "model shell arguments must retain argv boundaries");
  if (process.platform !== "win32") {
    assert.throws(() => registry.create("choices", "invalid model shell", "", process.cwd(), 80, 24, "missing-tessivum-shell"), /executable/i);
  }
  registry.disposeAll();
  manager.disposeAll();
}

async function deterministicRegression() {
  const clock = fakeClock();
  const nodePty = new FakeNodePty();
  const api = runtime(clock, nodePty);
  const warnings = [];
  const ctx = { logger: { warn: (message) => warnings.push(message) } };
  const manager = new api.PtyManager(process.execPath, 2, [], nodePty);
  const req = (cwd, tab = "one") => ({ url: `/?sessionId=session&tab=${tab}&cwd=${encodeURIComponent(cwd)}` });
  const attach = async (ws, cwd, tab) => api.attachTerminal(ctx, manager, null, ws, req(cwd, tab), { reconnectGraceMs: 30 }, () => ({ get: () => ({ value: {} }) }));

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
  const raceManager = new raceApi.PtyManager(process.execPath, 1, [], racePty);
  const raceReq = (cwd) => ({ url: `/?sessionId=race&tab=one&cwd=${encodeURIComponent(cwd)}` });
  const raceAttach = (ws, cwd) => raceApi.attachTerminal(ctx, raceManager, null, ws, raceReq(cwd), { reconnectGraceMs: 30 }, () => ({ get: () => ({ value: {} }) }));
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
  const quota = new api.PtyManager(process.execPath, 1, [], quotaPty);
  const quotaHandle = quota.open("quota", "one", "/a", 80, 24);
  assert.throws(() => quota.open("quota", "two", "/a", 80, 24), /terminal limit reached/);
  quota.close(quotaHandle.key, quotaHandle);
  assert.doesNotThrow(() => quota.open("quota", "two", "/a", 80, 24));
  quota.disposeAll();

  const agentPty = new FakeNodePty();
  const registry = new api.AgentPtyRegistry(process.execPath, [], agentPty);
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
  if (process.platform !== "win32") assert.equal(process.env.SHELL, undefined, "--real-pty must run in a child with SHELL removed");
  const api = runtime(null, null);
  const nodePty = api.loadNodePty();
  assert.ok(nodePty, "production PTY backend must load");
  const shell = api.defaultShell();
  const waitForCommand = (handle, write, command, exitCode, token) => {
    let subscription;
    let timeout;
    const output = new Promise((resolve, reject) => {
      timeout = setTimeout(() => reject(new Error("real PTY command did not finish")), 5000);
      subscription = handle.pty.onExit((event) => {
        try {
          assert.equal(event.exitCode, exitCode, "shell command must actually execute");
          assert.ok(handle.transcript.includes(token), "final output must arrive before exit");
          resolve();
        } catch (error) { reject(error); }
      });
      write(command);
    });
    return output.finally(() => {
      clearTimeout(timeout);
      subscription?.dispose();
    });
  };

  const manager = new api.PtyManager(shell, 1, [], nodePty);
  const handle = manager.open("real", "one", process.cwd(), 80, 24);
  const tokenParts = ["tessivum-pty", String(process.pid)];
  const token = tokenParts.join("-");
  let closed;
  try {
    handle.pty.resize(90, 30);
    const command = process.platform === "win32"
      ? `Write-Output ('{0}-{1}' -f '${tokenParts[0]}', '${tokenParts[1]}'); exit 7\r`
      : `printf '%s-%s\\n' '${tokenParts[0]}' '${tokenParts[1]}'; exit 7\r`;
    await waitForCommand(handle, text => handle.pty.write(text), command, 7, token);
  } finally {
    closed = manager.close(handle.key, handle);
  }
  assert.equal(closed, true);
  assert.equal(handle.closed, true);
  assert.equal(handle.exited, true);
  assert.equal(manager.close(handle.key, handle), false);

  const registry = new api.AgentPtyRegistry(shell, [], nodePty);
  const agentTokenParts = [...tokenParts, "agent"];
  const agentToken = agentTokenParts.join("-");
  const uuid = registry.create("real", "real model terminal", "", process.cwd());
  const agent = registry.get(uuid);
  let agentClosed;
  try {
    const command = process.platform === "win32"
      ? `Write-Output ('{0}-{1}-{2}' -f '${agentTokenParts[0]}', '${agentTokenParts[1]}', '${agentTokenParts[2]}'); exit 9\r`
      : `printf '%s-%s-%s\\n' '${agentTokenParts[0]}' '${agentTokenParts[1]}' '${agentTokenParts[2]}'; exit 9\r`;
    await waitForCommand(agent, text => registry.send(uuid, text), command, 9, agentToken);
    assert.ok(registry.read(uuid).text.includes(agentToken), "model terminal read must return real command output");
  } finally {
    agentClosed = registry.close(uuid, agent);
  }
  assert.equal(agentClosed, true);
  assert.equal(agent.closed, true);
  assert.equal(agent.exited, true);
  assert.equal(registry.close(uuid, agent), false);

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

await shellResolverRegression();
terminalShellRegression();
await deterministicRegression();
if (realPty) await realPtySmoke();
console.log(`sidebar PTY lifecycle check passed${realPty ? " (real shell output and exit)" : ""}`);
