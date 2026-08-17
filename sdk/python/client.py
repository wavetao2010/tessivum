"""Dependency-free asyncio client for the SDK NDJSON JSON-RPC server."""

from __future__ import annotations

import asyncio
import inspect
import json
import os
from collections.abc import Awaitable, Callable, Mapping, Sequence
from dataclasses import dataclass
from typing import Any, Union
MAX_LINE_BYTES = 1024 * 1024
MAX_REQUEST_ID = (1 << 53) - 1



class JsonRpcError(RuntimeError):
    def __init__(self, message: str, code: int | None = None, data: Any = None) -> None:
        super().__init__(message)
        self.code = code
        self.data = data


@dataclass(frozen=True)
class Notification:
    method: str
    params: Mapping[str, Any]


Callback = Callable[[Notification], Union[Awaitable[None], None]]
DiagnosticCallback = Callable[[str], Union[Awaitable[None], None]]


class JsonRpcClient:
    """Owns one SDK process and correlates its positive JSON-RPC request IDs."""

    def __init__(
        self,
        process: asyncio.subprocess.Process,
        *,
        timeout: float,
        on_notification: Callback | None,
        on_diagnostic: DiagnosticCallback | None,
    ) -> None:
        self._process = process
        self._timeout = timeout
        self._on_notification = on_notification
        self._on_diagnostic = on_diagnostic
        self._pending: dict[int, asyncio.Future[Any]] = {}
        self._next_id = 1
        self._initialized = False
        self._closing = False
        self._write_lock = asyncio.Lock()
        self._reader_task = asyncio.create_task(self._read_stdout())
        self._stderr_task = asyncio.create_task(self._read_stderr())
        self._watch_task = asyncio.create_task(self._watch_process())

    @classmethod
    async def start(
        cls,
        command: str,
        *args: str,
        cwd: str | None = None,
        env: Mapping[str, str] | None = None,
        timeout: float = 30.0,
        on_notification: Callback | None = None,
        on_diagnostic: DiagnosticCallback | None = None,
    ) -> "JsonRpcClient":
        if timeout <= 0:
            raise ValueError("timeout must be positive")
        process = await asyncio.create_subprocess_exec(
            command,
            *args,
            cwd=cwd,
            env={**os.environ, **env} if env is not None else None,
            stdin=asyncio.subprocess.PIPE,
            stdout=asyncio.subprocess.PIPE,
            stderr=asyncio.subprocess.PIPE,
            limit=MAX_LINE_BYTES + 1,
        )
        return cls(
            process,
            timeout=timeout,
            on_notification=on_notification,
            on_diagnostic=on_diagnostic,
        )

    async def initialize(self, params: Mapping[str, Any]) -> Mapping[str, Any]:
        if self._initialized:
            raise JsonRpcError("initialize may only be called once")
        self._initialized = True
        return await self._request("initialize", dict(params))

    async def prompt(
        self, session_id: str, content_blocks: Sequence[Mapping[str, Any]]
    ) -> Mapping[str, Any]:
        self._require_initialized()
        return await self._request(
            "session/prompt",
            {"sessionId": session_id, "contentBlocks": list(content_blocks)},
        )

    async def cancel(
        self, session_id: str, cause: Mapping[str, Any] | None = None
    ) -> bool:
        self._require_initialized()
        result = await self._request(
            "session/cancel",
            {"sessionId": session_id, "cause": dict(cause or {"kind": "user"})},
        )
        if not isinstance(result, bool):
            raise JsonRpcError("SDK server returned a non-boolean cancellation result")
        return result

    async def shutdown(self) -> Mapping[str, Any]:
        if self._process.returncode is not None:
            return {}
        self._closing = True
        try:
            result = await self._request("shutdown", {})
            if not isinstance(result, dict):
                raise JsonRpcError("SDK server returned a non-object shutdown result")
            await self._close_stdin()
            await self._wait_or_kill()
            return result
        finally:
            self._closing = False

    async def close(self) -> None:
        if self._process.returncode is not None:
            return
        self._closing = True
        await self._close_stdin()
        self._process.terminate()
        await self._wait_or_kill()

    async def _request(self, method: str, params: Mapping[str, Any]) -> Any:
        if self._process.returncode is not None or (self._closing and method != "shutdown"):
            raise JsonRpcError("SDK process is not available")
        if self._next_id > MAX_REQUEST_ID:
            raise JsonRpcError("request ID space exhausted")
        request_id = self._next_id
        self._next_id += 1
        frame = json.dumps(
            {"jsonrpc": "2.0", "id": request_id, "method": method, "params": params},
            separators=(",", ":"),
            ensure_ascii=False,
        ).encode("utf-8")
        if len(frame) > MAX_LINE_BYTES:
            raise JsonRpcError("request exceeds 1 MiB")

        loop = asyncio.get_running_loop()
        response: asyncio.Future[Any] = loop.create_future()
        self._pending[request_id] = response
        try:
            async with self._write_lock:
                if self._process.stdin is None or self._process.stdin.is_closing():
                    raise JsonRpcError("SDK stdin is closed")
                self._process.stdin.write(frame + b"\n")
                await self._process.stdin.drain()
            return await asyncio.wait_for(asyncio.shield(response), self._timeout)
        except TimeoutError as error:
            if self._pending.pop(request_id, None) is not None:
                raise JsonRpcError(f"request {request_id} timed out") from error
            raise
        except BaseException:
            self._pending.pop(request_id, None)
            raise
        finally:
            if response.done() or response.cancelled():
                self._pending.pop(request_id, None)

    def _require_initialized(self) -> None:
        if not self._initialized:
            raise JsonRpcError("initialize must complete before session calls")
        if self._closing or self._process.returncode is not None:
            raise JsonRpcError("SDK process is closing")

    async def _read_stdout(self) -> None:
        assert self._process.stdout is not None
        try:
            while True:
                try:
                    line = await self._process.stdout.readuntil(b"\n")
                except asyncio.LimitOverrunError as error:
                    raise JsonRpcError("SDK server emitted an oversized frame") from error
                except asyncio.IncompleteReadError as error:
                    if error.partial:
                        raise JsonRpcError("SDK server emitted an unterminated frame") from error
                    return
                if len(line) - 1 > MAX_LINE_BYTES:
                    raise JsonRpcError("SDK server emitted an oversized frame")
                self._handle_line(line[:-1])
        except Exception as error:
            self._fail_all(error)
            if self._process.returncode is None:
                self._process.terminate()

    async def _read_stderr(self) -> None:
        assert self._process.stderr is not None
        try:
            while chunk := await self._process.stderr.read(8192):
                await self._invoke_diagnostic(chunk.decode("utf-8", errors="replace"))
        except Exception as error:
            self._fail_all(error)

    def _handle_line(self, line: bytes) -> None:
        try:
            message = json.loads(line)
        except (TypeError, ValueError) as error:
            raise JsonRpcError("SDK server emitted invalid JSON") from error
        if not isinstance(message, dict) or message.get("jsonrpc") != "2.0":
            raise JsonRpcError("SDK server emitted an invalid JSON-RPC frame")
        method = message.get("method")
        if isinstance(method, str) and "id" not in message:
            params = message.get("params")
            if not isinstance(params, dict):
                raise JsonRpcError("SDK server emitted notification params that are not an object")
            if method not in {
                "session.event",
                "session.status",
                "subagent.started",
                "subagent.finished",
            }:
                raise JsonRpcError(f"SDK server emitted unknown notification {method}")
            self._invoke_callback(Notification(method, params))
            return
        request_id = message.get("id")
        if not isinstance(request_id, int) or isinstance(request_id, bool) or not 0 < request_id <= MAX_REQUEST_ID:
            raise JsonRpcError("SDK server emitted a response with an invalid ID")
        response = self._pending.pop(request_id, None)
        if response is None:
            return  # A timed-out request is not retried or re-correlated.
        has_error = "error" in message
        has_result = "result" in message
        if has_error == has_result:
            raise JsonRpcError("SDK server emitted a malformed response")
        if has_error:
            error = message["error"]
            if not isinstance(error, dict):
                raise JsonRpcError("SDK server emitted an invalid JSON-RPC error")
            response.set_exception(
                JsonRpcError(
                    str(error.get("message", "JSON-RPC error")),
                    error.get("code") if isinstance(error.get("code"), int) else None,
                    error.get("data"),
                )
            )
        else:
            response.set_result(message["result"])

    def _invoke_callback(self, notification: Notification) -> None:
        if self._on_notification is None:
            return
        try:
            result = self._on_notification(notification)
            if inspect.isawaitable(result):
                task = asyncio.ensure_future(result)
                task.add_done_callback(self._callback_done)
        except Exception as error:
            self._fail_all(error)

    def _callback_done(self, task: asyncio.Future[Any]) -> None:
        if task.cancelled():
            return
        if error := task.exception():
            self._fail_all(error)

    async def _invoke_diagnostic(self, text: str) -> None:
        if self._on_diagnostic is None:
            return
        result = self._on_diagnostic(text)
        if inspect.isawaitable(result):
            await result

    async def _watch_process(self) -> None:
        await self._process.wait()
        self._fail_all(JsonRpcError("SDK process exited"))

    async def _close_stdin(self) -> None:
        if self._process.stdin is not None and not self._process.stdin.is_closing():
            self._process.stdin.close()
            try:
                await self._process.stdin.wait_closed()
            except (BrokenPipeError, ConnectionResetError):
                pass

    async def _wait_or_kill(self) -> None:
        try:
            await asyncio.wait_for(self._process.wait(), self._timeout)
        except TimeoutError:
            self._process.kill()
            await self._process.wait()
            raise JsonRpcError("SDK process did not exit after shutdown")
        finally:
            self._fail_all(JsonRpcError("SDK process exited"))
            self._reader_task.cancel()
            self._stderr_task.cancel()
            self._watch_task.cancel()

    def _fail_all(self, reason: BaseException) -> None:
        for request_id, response in tuple(self._pending.items()):
            self._pending.pop(request_id, None)
            if not response.done():
                response.set_exception(reason)


SdkClient = JsonRpcClient
