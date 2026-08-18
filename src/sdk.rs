//! Newline-delimited JSON-RPC 2.0 SDK transport.
//!
//! The transport deliberately has one writer and bounded input/output queues.  Host
//! calls are not retried: `session/prompt` returns only the durable receipt supplied
//! by the host.

use std::{
    fmt,
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc,
    },
};

use serde::{
    de::{DeserializeOwned, Error as DeError, MapAccess, SeqAccess, Visitor},
    Deserialize, Serialize,
};
use serde_json::{json, Map, Value};
use thiserror::Error;
use tokio::{
    io::{self, AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt},
    sync::{broadcast, mpsc, watch},
};

use crate::{
    host::{HostApi, HostNotification},
    protocol::{AgentCancelCause, InitializeParams, SessionPromptParams},
};

/// Maximum bytes in an NDJSON frame, excluding its trailing newline.
pub const MAX_LINE_BYTES: usize = 1024 * 1024;
const INPUT_QUEUE_CAPACITY: usize = 8;
const OUTPUT_QUEUE_CAPACITY: usize = 16;

/// Errors terminating an SDK transport rather than becoming JSON-RPC responses.
#[derive(Debug, Error)]
pub enum SdkError {
    #[error("SDK transport I/O failed: {0}")]
    Io(#[from] io::Error),
    #[error("SDK could not encode a JSON-RPC frame: {0}")]
    Encode(#[from] serde_json::Error),
    #[error("SDK output frame exceeds the configured 1 MiB limit")]
    OutputTooLarge,
    #[error("SDK output channel closed")]
    OutputClosed,
    #[error(
        "SDK notification receiver lagged; connection closed before durable facts could be lost"
    )]
    NotificationLagged,
    #[error("SDK writer task failed")]
    WriterTask,
}

/// A bounded NDJSON JSON-RPC server backed by one host runtime.
#[derive(Clone)]
pub struct JsonRpcServer {
    host: Arc<dyn HostApi>,
    initialized: Arc<AtomicBool>,
}

impl fmt::Debug for JsonRpcServer {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("JsonRpcServer")
            .field("initialized", &self.initialized.load(Ordering::Acquire))
            .finish_non_exhaustive()
    }
}

impl JsonRpcServer {
    /// Creates a server for exactly one initialized host lifetime.
    pub fn new(host: Arc<dyn HostApi>) -> Self {
        Self {
            host,
            initialized: Arc::new(AtomicBool::new(false)),
        }
    }

    /// Serves one pair of newline-delimited streams.
    ///
    /// JSON-RPC is emitted only to `writer`; diagnostics remain on the process
    /// diagnostic stream.  The function returns after `shutdown`, EOF, or a
    /// transport failure.  EOF performs host shutdown so disconnected clients do
    /// not leave agents running.
    pub async fn serve<R, W>(&self, reader: R, writer: W) -> Result<(), SdkError>
    where
        R: AsyncRead + Send + Unpin + 'static,
        W: AsyncWrite + Send + Unpin + 'static,
    {
        let (input_tx, mut input_rx) = mpsc::channel(INPUT_QUEUE_CAPACITY);
        let reader_task = tokio::spawn(read_frames(reader, input_tx));

        let (output_tx, output_rx) = mpsc::channel(OUTPUT_QUEUE_CAPACITY);
        let (fault_tx, mut fault_rx) = mpsc::channel(1);
        let writer_task = tokio::spawn(write_frames(writer, output_rx, fault_tx.clone()));

        let (stop_notifications, notification_stop) = watch::channel(false);
        let mut notification_task = Some(tokio::spawn(relay_notifications(
            self.host.subscribe(),
            output_tx.clone(),
            notification_stop,
            fault_tx,
        )));

        let outcome = self
            .serve_requests(&mut input_rx, &mut fault_rx, &output_tx)
            .await;
        let result = match outcome {
            Ok(ServerExit::Shutdown { id, result }) => {
                let _ = stop_notifications.send(true);
                let relay = notification_task
                    .take()
                    .expect("notification task is present")
                    .await
                    .map_err(|_| SdkError::WriterTask)?;
                match relay {
                    RelayExit::Drained => enqueue_value(&output_tx, rpc_result(id, result)).await,
                    RelayExit::NotificationLagged => Err(SdkError::NotificationLagged),
                    RelayExit::OutputTooLarge => Err(SdkError::OutputTooLarge),
                    RelayExit::OutputClosed => Err(SdkError::OutputClosed),
                }
            }
            Ok(ServerExit::Disconnected) => {
                let _ = stop_notifications.send(true);
                Ok(())
            }
            Err(error) => {
                let _ = stop_notifications.send(true);
                Err(error)
            }
        };

        reader_task.abort();
        let _ = reader_task.await;
        if let Some(task) = notification_task {
            task.abort();
            let _ = task.await;
        }
        drop(output_tx);

        let writer_result = writer_task.await.map_err(|_| SdkError::WriterTask)?;
        match (result, writer_result) {
            (Err(error), _) => Err(error),
            (Ok(()), Err(error)) => Err(SdkError::Io(error)),
            (Ok(()), Ok(())) => Ok(()),
        }
    }

    /// Serves the process standard streams on every Tokio-supported platform.
    pub async fn serve_stdio(&self) -> Result<(), SdkError> {
        self.serve(tokio::io::stdin(), tokio::io::stdout()).await
    }

    async fn serve_requests(
        &self,
        input_rx: &mut mpsc::Receiver<InboundFrame>,
        fault_rx: &mut mpsc::Receiver<TransportFault>,
        output_tx: &mpsc::Sender<Vec<u8>>,
    ) -> Result<ServerExit, SdkError> {
        loop {
            tokio::select! {
                fault = fault_rx.recv() => match fault {
                    Some(TransportFault::NotificationLagged) => {
                        self.cleanup_after_disconnect().await;
                        return Err(SdkError::NotificationLagged);
                    }
                    Some(TransportFault::OutputTooLarge) => {
                        self.cleanup_after_disconnect().await;
                        return Err(SdkError::OutputTooLarge);
                    }
                    Some(TransportFault::WriterFailed) | None => {
                        self.cleanup_after_disconnect().await;
                        return Err(SdkError::OutputClosed);
                    }
                },
                frame = input_rx.recv() => match frame {
                    Some(InboundFrame::Line(line)) => {
                        let request = match parse_request(&line) {
                            Ok(request) => request,
                            Err(error) => {
                                enqueue_value(output_tx, rpc_error(Value::Null, error)).await?;
                                continue;
                            }
                        };
                        let id = request.id.clone();
                        match self.handle_request(request).await {
                            RequestOutcome::Reply(value) => {
                                enqueue_value(output_tx, rpc_result(id, value)).await?;
                            }
                            RequestOutcome::Error(error) => {
                                enqueue_value(output_tx, rpc_error(id, error)).await?;
                            }
                            RequestOutcome::Shutdown(value) => {
                                return Ok(ServerExit::Shutdown { id, result: value });
                            }
                        }
                    }
                    Some(InboundFrame::Oversized) => {
                        enqueue_value(output_tx, rpc_error(Value::Null, RpcFault::invalid_request())).await?;
                    }
                    Some(InboundFrame::Unterminated) => {
                        enqueue_value(output_tx, rpc_error(Value::Null, RpcFault::invalid_request())).await?;
                    }
                    Some(InboundFrame::Io(error)) => {
                        self.cleanup_after_disconnect().await;
                        return Err(SdkError::Io(error));
                    }
                    None => {
                        self.cleanup_after_disconnect().await;
                        return Ok(ServerExit::Disconnected);
                    }
                },
            }
        }
    }

    async fn handle_request(&self, request: RpcRequest) -> RequestOutcome {
        let params = request.params;
        match request.method.as_str() {
            // The slash form is the ACP extension.  The dot/slash session calls
            // share the same strict product DTO conversion below.
            "initialize" | "session/new" => {
                let parameters: InitializeParams =
                    match strict_parameters::<InitializeParams>(params) {
                        Ok(parameters) if parameters.validate().is_ok() => parameters,
                        _ => return RequestOutcome::Error(RpcFault::invalid_params()),
                    };
                // Mark the attempt before calling the host: an initialization error
                // must not be retried into a second durable runtime configuration.
                if self.initialized.swap(true, Ordering::AcqRel) {
                    return RequestOutcome::Error(RpcFault::already_initialized());
                }
                match self.host.initialize(parameters).await {
                    Ok(result) => match serialize_result(result) {
                        Ok(value) => RequestOutcome::Reply(value),
                        Err(error) => {
                            eprintln!("sdk initialize result encoding failed: {error}");
                            RequestOutcome::Error(RpcFault::internal_error())
                        }
                    },
                    Err(error) => {
                        eprintln!("sdk initialize failed: {error}");
                        RequestOutcome::Error(RpcFault::host_failure())
                    }
                }
            }
            "session/prompt" | "session.prompt" => {
                if !self.initialized.load(Ordering::Acquire) {
                    return RequestOutcome::Error(RpcFault::not_initialized());
                }
                let parameters: SessionPromptParams =
                    match strict_parameters::<SessionPromptParams>(params) {
                        Ok(parameters)
                            if !parameters.session_id.as_str().trim().is_empty()
                                && parameters.validate().is_ok() =>
                        {
                            parameters
                        }
                        _ => return RequestOutcome::Error(RpcFault::invalid_params()),
                    };
                match self.host.prompt(parameters).await {
                    Ok(result) => match serialize_result(result) {
                        Ok(value) => RequestOutcome::Reply(value),
                        Err(error) => {
                            eprintln!("sdk prompt result encoding failed: {error}");
                            RequestOutcome::Error(RpcFault::internal_error())
                        }
                    },
                    Err(error) => {
                        eprintln!("sdk session/prompt failed: {error}");
                        RequestOutcome::Error(RpcFault::host_failure())
                    }
                }
            }
            "session/cancel" | "session.cancel" => {
                if !self.initialized.load(Ordering::Acquire) {
                    return RequestOutcome::Error(RpcFault::not_initialized());
                }
                let parameters: CancelParameters =
                    match strict_parameters::<CancelParameters>(params) {
                        Ok(parameters) if !parameters.session_id.as_str().trim().is_empty() => {
                            parameters
                        }
                        _ => return RequestOutcome::Error(RpcFault::invalid_params()),
                    };
                match self
                    .host
                    .cancel(parameters.session_id, parameters.cause)
                    .await
                {
                    Ok(cancelled) => RequestOutcome::Reply(Value::Bool(cancelled)),
                    Err(error) => {
                        eprintln!("sdk session/cancel failed: {error}");
                        RequestOutcome::Error(RpcFault::host_failure())
                    }
                }
            }
            "shutdown" => {
                if strict_parameters::<EmptyParameters>(params).is_err() {
                    return RequestOutcome::Error(RpcFault::invalid_params());
                }
                match self.host.shutdown().await {
                    Ok(()) => RequestOutcome::Shutdown(json!({})),
                    Err(error) => {
                        eprintln!("sdk shutdown failed: {error}");
                        RequestOutcome::Error(RpcFault::host_failure())
                    }
                }
            }
            _ => RequestOutcome::Error(RpcFault::method_not_found()),
        }
    }

    async fn cleanup_after_disconnect(&self) {
        if let Err(error) = self.host.shutdown().await {
            eprintln!("sdk disconnect shutdown failed: {error}");
        }
    }
}

#[derive(Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct RpcRequest {
    jsonrpc: String,
    id: Value,
    method: String,
    params: Value,
}

#[derive(Deserialize, Serialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct CancelParameters {
    session_id: crate::protocol::SessionId,
    cause: AgentCancelCause,
}

#[derive(Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct EmptyParameters {}

enum RequestOutcome {
    Reply(Value),
    Error(RpcFault),
    Shutdown(Value),
}

enum ServerExit {
    Shutdown { id: Value, result: Value },
    Disconnected,
}

#[derive(Clone, Copy)]
struct RpcFault {
    code: i64,
    message: &'static str,
}

impl RpcFault {
    const fn parse_error() -> Self {
        Self {
            code: -32700,
            message: "parse error",
        }
    }

    const fn invalid_request() -> Self {
        Self {
            code: -32600,
            message: "invalid request",
        }
    }

    const fn invalid_params() -> Self {
        Self {
            code: -32602,
            message: "invalid params",
        }
    }

    const fn method_not_found() -> Self {
        Self {
            code: -32601,
            message: "method not found",
        }
    }

    const fn already_initialized() -> Self {
        Self {
            code: -32000,
            message: "already initialized",
        }
    }

    const fn not_initialized() -> Self {
        Self {
            code: -32000,
            message: "server not initialized",
        }
    }

    const fn host_failure() -> Self {
        Self {
            code: -32000,
            message: "host request failed",
        }
    }

    const fn internal_error() -> Self {
        Self {
            code: -32603,
            message: "internal error",
        }
    }
}

fn parse_request(line: &[u8]) -> Result<RpcRequest, RpcFault> {
    let value = serde_json::from_slice::<StrictJson>(line)
        .map_err(|_| RpcFault::parse_error())?
        .0;
    let request: RpcRequest = strict_from_value(value).map_err(|_| RpcFault::invalid_request())?;
    if request.jsonrpc != "2.0"
        || request.method.is_empty()
        || request
            .id
            .as_u64()
            .filter(|id| *id > 0 && *id <= 9_007_199_254_740_991)
            .is_none()
    {
        return Err(RpcFault::invalid_request());
    }
    Ok(request)
}

fn strict_parameters<T>(value: Value) -> Result<T, ()>
where
    T: DeserializeOwned + Serialize,
{
    strict_from_value(value)
}

fn strict_from_value<T>(value: Value) -> Result<T, ()>
where
    T: DeserializeOwned + Serialize,
{
    let typed = serde_json::from_value::<T>(value.clone()).map_err(|_| ())?;
    // Protocol structs intentionally permit extension fields for stored data.  The
    // public RPC boundary does not: round-tripping exposes ignored fields, null
    // optionals, and malformed enum payloads without duplicating every DTO here.
    if serde_json::to_value(&typed).map_err(|_| ())? != value {
        return Err(());
    }
    Ok(typed)
}

fn serialize_result<T: Serialize>(result: T) -> Result<Value, serde_json::Error> {
    serde_json::to_value(result)
}

fn rpc_result(id: Value, result: Value) -> Value {
    json!({"jsonrpc": "2.0", "id": id, "result": result})
}

fn rpc_error(id: Value, error: RpcFault) -> Value {
    json!({
        "jsonrpc": "2.0",
        "id": id,
        "error": {"code": error.code, "message": error.message},
    })
}

async fn enqueue_value(output: &mpsc::Sender<Vec<u8>>, value: Value) -> Result<(), SdkError> {
    let mut frame = serde_json::to_vec(&value)?;
    if frame.len() > MAX_LINE_BYTES {
        return Err(SdkError::OutputTooLarge);
    }
    frame.push(b'\n');
    output.send(frame).await.map_err(|_| SdkError::OutputClosed)
}

async fn write_frames<W>(
    mut writer: W,
    mut output: mpsc::Receiver<Vec<u8>>,
    faults: mpsc::Sender<TransportFault>,
) -> Result<(), io::Error>
where
    W: AsyncWrite + Unpin,
{
    while let Some(frame) = output.recv().await {
        if let Err(error) = writer.write_all(&frame).await {
            let _ = faults.try_send(TransportFault::WriterFailed);
            return Err(error);
        }
        if let Err(error) = writer.flush().await {
            let _ = faults.try_send(TransportFault::WriterFailed);
            return Err(error);
        }
    }
    writer.flush().await
}

enum InboundFrame {
    Line(Vec<u8>),
    Oversized,
    Unterminated,
    Io(io::Error),
}

async fn read_frames<R>(reader: R, input: mpsc::Sender<InboundFrame>)
where
    R: AsyncRead + Unpin,
{
    let mut reader = NdjsonReader::new(reader);
    loop {
        let frame = match reader.next().await {
            Ok(Some(frame)) => frame,
            Ok(None) => break,
            Err(error) => InboundFrame::Io(error),
        };
        let terminal = matches!(frame, InboundFrame::Io(_));
        if input.send(frame).await.is_err() || terminal {
            break;
        }
    }
}

struct NdjsonReader<R> {
    reader: R,
    chunk: [u8; 8192],
    chunk_pos: usize,
    chunk_len: usize,
    pending: Vec<u8>,
    discarding_oversized: bool,
}

impl<R: AsyncRead + Unpin> NdjsonReader<R> {
    fn new(reader: R) -> Self {
        Self {
            reader,
            chunk: [0; 8192],
            chunk_pos: 0,
            chunk_len: 0,
            pending: Vec::with_capacity(8192),
            discarding_oversized: false,
        }
    }

    async fn next(&mut self) -> Result<Option<InboundFrame>, io::Error> {
        loop {
            if self.chunk_pos == self.chunk_len {
                self.chunk_len = self.reader.read(&mut self.chunk).await?;
                self.chunk_pos = 0;
                if self.chunk_len == 0 {
                    if self.discarding_oversized {
                        self.discarding_oversized = false;
                        return Ok(Some(InboundFrame::Oversized));
                    }
                    if self.pending.is_empty() {
                        return Ok(None);
                    }
                    self.pending.clear();
                    return Ok(Some(InboundFrame::Unterminated));
                }
            }

            let byte = self.chunk[self.chunk_pos];
            self.chunk_pos += 1;
            if byte == b'\n' {
                if self.discarding_oversized {
                    self.discarding_oversized = false;
                    self.pending.clear();
                    return Ok(Some(InboundFrame::Oversized));
                }
                if self.pending.last() == Some(&b'\r') {
                    self.pending.pop();
                }
                return Ok(Some(InboundFrame::Line(std::mem::take(&mut self.pending))));
            }
            if !self.discarding_oversized {
                if self.pending.len() == MAX_LINE_BYTES {
                    self.pending.clear();
                    self.discarding_oversized = true;
                } else {
                    self.pending.push(byte);
                }
            }
        }
    }
}

enum TransportFault {
    NotificationLagged,
    OutputTooLarge,
    WriterFailed,
}

enum RelayExit {
    Drained,
    NotificationLagged,
    OutputTooLarge,
    OutputClosed,
}

async fn relay_notifications(
    mut notifications: broadcast::Receiver<HostNotification>,
    output: mpsc::Sender<Vec<u8>>,
    mut stop: watch::Receiver<bool>,
    faults: mpsc::Sender<TransportFault>,
) -> RelayExit {
    loop {
        if *stop.borrow() {
            loop {
                match notifications.try_recv() {
                    Ok(notification) => {
                        if let Err(exit) = send_notification(&output, notification, &faults).await {
                            return exit;
                        }
                    }
                    Err(broadcast::error::TryRecvError::Empty)
                    | Err(broadcast::error::TryRecvError::Closed) => return RelayExit::Drained,
                    Err(broadcast::error::TryRecvError::Lagged(_)) => {
                        let _ = faults.try_send(TransportFault::NotificationLagged);
                        return RelayExit::NotificationLagged;
                    }
                }
            }
        }

        tokio::select! {
            changed = stop.changed() => {
                if changed.is_err() {
                    return RelayExit::Drained;
                }
            }
            notification = notifications.recv() => match notification {
                Ok(notification) => {
                    if let Err(exit) = send_notification(&output, notification, &faults).await {
                        return exit;
                    }
                }
                Err(broadcast::error::RecvError::Lagged(_)) => {
                    let _ = faults.try_send(TransportFault::NotificationLagged);
                    return RelayExit::NotificationLagged;
                }
                Err(broadcast::error::RecvError::Closed) => return RelayExit::Drained,
            },
        }
    }
}

async fn send_notification(
    output: &mpsc::Sender<Vec<u8>>,
    notification: HostNotification,
    faults: &mpsc::Sender<TransportFault>,
) -> Result<(), RelayExit> {
    let value = match notification {
        HostNotification::SessionEvent(payload) => json!({
            "jsonrpc": "2.0",
            "method": "session.event",
            "params": payload,
        }),
        HostNotification::SessionStatus(payload) => json!({
            "jsonrpc": "2.0",
            "method": "session.status",
            "params": payload,
        }),
        HostNotification::ApprovalRequested(payload) => json!({
            "jsonrpc": "2.0",
            "method": "approval.requested",
            "params": payload,
        }),
        HostNotification::ApprovalResolved(payload) => json!({
            "jsonrpc": "2.0",
            "method": "approval.resolved",
            "params": payload,
        }),
        HostNotification::SettingsChanged(payload) => json!({
            "jsonrpc": "2.0",
            "method": "settings.changed",
            "params": payload,
        }),
        HostNotification::CredentialsChanged(payload) => json!({
            "jsonrpc": "2.0",
            "method": "credentials.changed",
            "params": payload,
        }),
        HostNotification::SubagentStarted(payload) => json!({
            "jsonrpc": "2.0",
            "method": "subagent.started",
            "params": payload,
        }),
        HostNotification::SubagentFinished(payload) => json!({
            "jsonrpc": "2.0",
            "method": "subagent.finished",
            "params": payload,
        }),
    };
    match enqueue_value(output, value).await {
        Ok(()) => Ok(()),
        Err(SdkError::OutputTooLarge) => {
            let _ = faults.try_send(TransportFault::OutputTooLarge);
            Err(RelayExit::OutputTooLarge)
        }
        Err(_) => {
            let _ = faults.try_send(TransportFault::WriterFailed);
            Err(RelayExit::OutputClosed)
        }
    }
}

/// JSON parsed with duplicate object members rejected at every nesting level.
struct StrictJson(Value);

impl<'de> Deserialize<'de> for StrictJson {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        deserializer.deserialize_any(StrictJsonVisitor)
    }
}

struct StrictJsonVisitor;

impl<'de> Visitor<'de> for StrictJsonVisitor {
    type Value = StrictJson;

    fn expecting(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("valid JSON without duplicate object members")
    }

    fn visit_bool<E: DeError>(self, value: bool) -> Result<Self::Value, E> {
        Ok(StrictJson(Value::Bool(value)))
    }

    fn visit_i64<E: DeError>(self, value: i64) -> Result<Self::Value, E> {
        Ok(StrictJson(Value::Number(value.into())))
    }

    fn visit_u64<E: DeError>(self, value: u64) -> Result<Self::Value, E> {
        Ok(StrictJson(Value::Number(value.into())))
    }

    fn visit_f64<E: DeError>(self, value: f64) -> Result<Self::Value, E> {
        serde_json::Number::from_f64(value)
            .map(Value::Number)
            .map(StrictJson)
            .ok_or_else(|| E::custom("JSON number must be finite"))
    }

    fn visit_str<E: DeError>(self, value: &str) -> Result<Self::Value, E> {
        Ok(StrictJson(Value::String(value.to_owned())))
    }

    fn visit_string<E: DeError>(self, value: String) -> Result<Self::Value, E> {
        Ok(StrictJson(Value::String(value)))
    }

    fn visit_none<E: DeError>(self) -> Result<Self::Value, E> {
        Ok(StrictJson(Value::Null))
    }

    fn visit_unit<E: DeError>(self) -> Result<Self::Value, E> {
        Ok(StrictJson(Value::Null))
    }

    fn visit_some<D>(self, deserializer: D) -> Result<Self::Value, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        StrictJson::deserialize(deserializer)
    }

    fn visit_seq<A>(self, mut sequence: A) -> Result<Self::Value, A::Error>
    where
        A: SeqAccess<'de>,
    {
        let mut values = Vec::new();
        while let Some(value) = sequence.next_element::<StrictJson>()? {
            values.push(value.0);
        }
        Ok(StrictJson(Value::Array(values)))
    }

    fn visit_map<A>(self, mut map: A) -> Result<Self::Value, A::Error>
    where
        A: MapAccess<'de>,
    {
        let mut values = Map::new();
        while let Some(key) = map.next_key::<String>()? {
            if values.contains_key(&key) {
                return Err(A::Error::custom("duplicate JSON object member"));
            }
            values.insert(key, map.next_value::<StrictJson>()?.0);
        }
        Ok(StrictJson(Value::Object(values)))
    }
}
