import { ChildProcessWithoutNullStreams, spawn } from "node:child_process";

export const MAX_LINE_BYTES = 1024 * 1024;
const MAX_REQUEST_ID = Number.MAX_SAFE_INTEGER;

export interface InitializeParams {
  cwd: string;
  provider: string;
  model: string;
  maxTokens?: number;
}

export interface InitializeResult {
  serverInfo: { name: string; version: string };
}

export interface SessionPromptParams {
  sessionId: string;
  contentBlocks: unknown[];
}

export interface SessionPromptResult {
  messageId: string;
}

export type AgentCancelCause =
  | { kind: "user" }
  | { kind: "parent" }
  | { kind: "hook"; reason: string }
  | { kind: "disposed" };

export interface SessionEventNotification {
  sessionId: string;
  event: unknown;
}

export interface SessionStatusNotification {
  sessionId: string;
  status: "idle" | "running";
}

export interface SubagentStartedNotification {
  parentSessionId: string;
  childSessionId: string;
}

export interface SubagentFinishedNotification {
  provider: string;
  agentId: string;
  parentSessionId: string;
  childSessionId: string;
  status: "ok" | "error";
  stopReason: string;
  lastAssistantMessage?: unknown[];
}

export type SdkNotification =
  | { method: "session.event"; params: SessionEventNotification }
  | { method: "session.status"; params: SessionStatusNotification }
  | { method: "subagent.started"; params: SubagentStartedNotification }
  | { method: "subagent.finished"; params: SubagentFinishedNotification };

export interface ClientOptions {
  cwd?: string;
  env?: Record<string, string | undefined>;
  timeoutMs?: number;
  onNotification?: (notification: SdkNotification) => void;
  onDiagnostic?: (text: string) => void;
  onSessionEvent?: (notification: SessionEventNotification) => void;
  onSessionStatus?: (notification: SessionStatusNotification) => void;
  onSubagentStarted?: (notification: SubagentStartedNotification) => void;
  onSubagentFinished?: (notification: SubagentFinishedNotification) => void;
}

export class JsonRpcError extends Error {
  constructor(
    message: string,
    readonly code?: number,
    readonly data?: unknown,
  ) {
    super(message);
    this.name = "JsonRpcError";
  }
}

type Pending = {
  resolve: (value: unknown) => void;
  reject: (reason: Error) => void;
  timer: ReturnType<typeof setTimeout>;
};

/** Dependency-free Node client for the SDK NDJSON JSON-RPC transport. */
export class JsonRpcClient {
  private readonly child: ChildProcessWithoutNullStreams;
  private readonly timeoutMs: number;
  private readonly pending = new Map<number, Pending>();
  private readonly options: ClientOptions;
  private buffered = Buffer.alloc(0);
  private nextId = 1;
  private initializeStarted = false;
  private closing = false;
  private closed = false;
  private writeTail: Promise<void> = Promise.resolve();
  private readonly exited: Promise<void>;

  constructor(command: string, args: readonly string[] = [], options: ClientOptions = {}) {
    this.options = options;
    this.timeoutMs = options.timeoutMs ?? 30_000;
    if (!Number.isSafeInteger(this.timeoutMs) || this.timeoutMs <= 0) {
      throw new TypeError("timeoutMs must be a positive safe integer");
    }
    this.child = spawn(command, [...args], {
      cwd: options.cwd,
      env: options.env ? { ...process.env, ...options.env } : process.env,
      stdio: ["pipe", "pipe", "pipe"],
    });
    this.child.stdout.on("data", (chunk: Buffer) => this.receive(chunk));
    this.child.stderr.on("data", (chunk: Buffer) => options.onDiagnostic?.(chunk.toString("utf8")));
    this.child.once("error", (error) => this.fail(error));
    this.exited = new Promise((resolve) => {
      this.child.once("exit", (code, signal) => {
        this.closed = true;
        this.fail(new Error(`SDK process exited (${code ?? signal ?? "unknown"})`));
        resolve();
      });
    });
  }

  async initialize(params: InitializeParams): Promise<InitializeResult> {
    if (this.initializeStarted) {
      throw new JsonRpcError("initialize may only be called once");
    }
    this.initializeStarted = true;
    return this.request("initialize", params) as Promise<InitializeResult>;
  }

  async prompt(params: SessionPromptParams): Promise<SessionPromptResult> {
    this.requireInitialized();
    return this.request("session/prompt", params) as Promise<SessionPromptResult>;
  }

  async cancel(sessionId: string, cause: AgentCancelCause = { kind: "user" }): Promise<boolean> {
    this.requireInitialized();
    return this.request("session/cancel", { sessionId, cause }) as Promise<boolean>;
  }

  async shutdown(): Promise<Record<string, never>> {
    if (this.closed) {
      return {};
    }
    this.closing = true;
    try {
      const result = (await this.request("shutdown", {})) as Record<string, never>;
      this.child.stdin.end();
      await this.awaitExit();
      return result;
    } finally {
      this.closing = false;
    }
  }

  async close(): Promise<void> {
    if (this.closed) {
      return;
    }
    this.closing = true;
    this.child.stdin.end();
    this.child.kill("SIGTERM");
    await this.awaitExit();
  }

  private requireInitialized(): void {
    if (!this.initializeStarted) {
      throw new JsonRpcError("initialize must complete before session calls");
    }
    if (this.closed || this.closing) {
      throw new JsonRpcError("SDK process is closing");
    }
  }

  private request(method: string, params: unknown): Promise<unknown> {
    if (this.closed || (this.closing && method !== "shutdown")) {
      return Promise.reject(new JsonRpcError("SDK process is not available"));
    }
    if (this.nextId > MAX_REQUEST_ID) {
      return Promise.reject(new JsonRpcError("request ID space exhausted"));
    }
    const id = this.nextId++;
    let frame: string;
    try {
      frame = `${JSON.stringify({ jsonrpc: "2.0", id, method, params })}\n`;
    } catch (error) {
      return Promise.reject(error);
    }
    if (Buffer.byteLength(frame, "utf8") - 1 > MAX_LINE_BYTES) {
      return Promise.reject(new JsonRpcError("request exceeds 1 MiB"));
    }

    return new Promise((resolve, reject) => {
      const timer = setTimeout(() => {
        if (this.pending.delete(id)) {
          reject(new JsonRpcError(`request ${id} timed out`));
        }
      }, this.timeoutMs);
      this.pending.set(id, { resolve, reject, timer });
      this.writeTail = this.writeTail
        .then(() => this.write(frame))
        .catch((error: Error) => this.fail(error));
    });
  }

  private write(frame: string): Promise<void> {
    return new Promise((resolve, reject) => {
      if (this.closed || !this.child.stdin.writable) {
        reject(new JsonRpcError("SDK stdin is closed"));
        return;
      }
      try {
        this.child.stdin.write(frame, "utf8", (error) => (error ? reject(error) : resolve()));
      } catch (error) {
        reject(error);
      }
    });
  }

  private receive(chunk: Buffer): void {
    let start = 0;
    for (let index = 0; index < chunk.length; index += 1) {
      if (chunk[index] !== 0x0a) {
        continue;
      }
      this.append(chunk.subarray(start, index));
      const line = this.buffered;
      this.buffered = Buffer.alloc(0);
      start = index + 1;
      if (line.length > 0) {
        this.receiveLine(line);
      }
    }
    this.append(chunk.subarray(start));
  }

  private append(part: Buffer): void {
    if (this.buffered.length + part.length > MAX_LINE_BYTES) {
      this.fail(new JsonRpcError("SDK server emitted an oversized frame"));
      this.child.kill("SIGTERM");
      return;
    }
    if (part.length > 0) {
      this.buffered = Buffer.concat([this.buffered, part]);
    }
  }

  private receiveLine(line: Buffer): void {
    let message: unknown;
    try {
      message = JSON.parse(line.toString("utf8"));
    } catch {
      this.fail(new JsonRpcError("SDK server emitted invalid JSON"));
      return;
    }
    if (!isObject(message) || message.jsonrpc !== "2.0") {
      this.fail(new JsonRpcError("SDK server emitted an invalid JSON-RPC frame"));
      return;
    }
    if (typeof message.method === "string" && !("id" in message)) {
      this.notify(message.method, message.params);
      return;
    }
    const responseId = message.id;
    if (typeof responseId !== "number" || !Number.isSafeInteger(responseId) || responseId <= 0) {
      this.fail(new JsonRpcError("SDK server emitted a response with an invalid ID"));
      return;
    }
    const pending = this.pending.get(responseId);
    if (!pending) {
      return; // Timed-out calls are deliberately not retried or re-correlated.
    }
    this.pending.delete(responseId);
    clearTimeout(pending.timer);
    const hasError = "error" in message;
    const hasResult = "result" in message;
    if (hasError === hasResult) {
      pending.reject(new JsonRpcError("SDK server emitted a malformed response"));
    } else if (hasError) {
      const error = isObject(message.error) ? message.error : {};
      pending.reject(new JsonRpcError(
        typeof error.message === "string" ? error.message : "JSON-RPC error",
        typeof error.code === "number" ? error.code : undefined,
        error.data,
      ));
    } else {
      pending.resolve(message.result);
    }
  }

  private notify(method: string, params: unknown): void {
    if (!isObject(params)) {
      this.fail(new JsonRpcError("SDK server emitted notification params that are not an object"));
      return;
    }
    let notification: SdkNotification | undefined;
    switch (method) {
      case "session.event":
        notification = { method, params: params as SessionEventNotification };
        this.options.onSessionEvent?.(notification.params);
        break;
      case "session.status":
        notification = { method, params: params as SessionStatusNotification };
        this.options.onSessionStatus?.(notification.params);
        break;
      case "subagent.started":
        notification = { method, params: params as SubagentStartedNotification };
        this.options.onSubagentStarted?.(notification.params);
        break;
      case "subagent.finished":
        notification = { method, params: params as SubagentFinishedNotification };
        this.options.onSubagentFinished?.(notification.params);
        break;
      default:
        this.fail(new JsonRpcError(`SDK server emitted unknown notification ${method}`));
        return;
    }
    if (notification) {
      this.options.onNotification?.(notification);
    }
  }

  private fail(reason: unknown): void {
    const error = reason instanceof Error ? reason : new Error(String(reason));
    if (this.closed && this.pending.size === 0) {
      return;
    }
    for (const [id, pending] of this.pending) {
      this.pending.delete(id);
      clearTimeout(pending.timer);
      pending.reject(error);
    }
  }

  private async awaitExit(): Promise<void> {
    let timer: ReturnType<typeof setTimeout> | undefined;
    try {
      await Promise.race([
        this.exited,
        new Promise<void>((_, reject) => {
          timer = setTimeout(() => reject(new JsonRpcError("SDK process did not exit after shutdown")), this.timeoutMs);
        }),
      ]);
    } catch (error) {
      this.child.kill("SIGKILL");
      await this.exited;
      throw error;
    } finally {
      if (timer) {
        clearTimeout(timer);
      }
    }
  }
}

function isObject(value: unknown): value is Record<string, unknown> {
  return typeof value === "object" && value !== null && !Array.isArray(value);
}

export { JsonRpcClient as SdkClient };
