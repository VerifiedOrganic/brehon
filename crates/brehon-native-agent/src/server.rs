use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::Duration;

use brehon_adapter_sdk::{
    JsonRpcError, JsonRpcMessage, JsonRpcNotification, JsonRpcRequest, JsonRpcResponse,
};
use serde::Serialize;
use serde_json::Value;
use tokio::io::{AsyncBufReadExt, AsyncRead, AsyncWrite, AsyncWriteExt, BufReader};
use tokio::sync::{oneshot, Mutex};
use tracing::{debug, warn};

use crate::runtime::{CancellationToken, NativeRuntime};
use crate::ui::{TerminalEvent, TerminalEventSink};

type PendingMap = Arc<Mutex<HashMap<String, oneshot::Sender<JsonRpcResponse>>>>;

#[derive(Clone)]
pub struct RpcHandle {
    writer: Arc<Mutex<Box<dyn AsyncWrite + Send + Unpin>>>,
    pending: PendingMap,
    next_id: Arc<AtomicU64>,
    terminal_events: Option<TerminalEventSink>,
}

impl RpcHandle {
    pub(crate) fn new<W>(writer: W) -> Self
    where
        W: AsyncWrite + Send + Unpin + 'static,
    {
        Self {
            writer: Arc::new(Mutex::new(Box::new(writer))),
            pending: Arc::new(Mutex::new(HashMap::new())),
            next_id: Arc::new(AtomicU64::new(1)),
            terminal_events: None,
        }
    }

    fn with_terminal_events(mut self, terminal_events: TerminalEventSink) -> Self {
        self.terminal_events = Some(terminal_events);
        self
    }

    pub async fn send_response(&self, response: JsonRpcResponse) -> Result<(), String> {
        self.write_json(&response).await
    }

    pub async fn send_notification(
        &self,
        method: impl Into<String>,
        params: Option<Value>,
    ) -> Result<(), String> {
        let method = method.into();
        if method == "session/update" {
            if let Some(update) = params
                .as_ref()
                .and_then(|params| params.get("update"))
                .cloned()
            {
                self.send_terminal_event(TerminalEvent::SessionUpdate(update));
            }
        }
        self.write_json(&JsonRpcNotification::new(method, params))
            .await
    }

    pub async fn request_with_cancel(
        &self,
        method: impl Into<String>,
        params: Option<Value>,
        cancel: &CancellationToken,
    ) -> Result<JsonRpcResponse, String> {
        let method = method.into();
        let id = format!(
            "native-agent-{}",
            self.next_id.fetch_add(1, Ordering::Relaxed)
        );
        if method == "session/request_permission" {
            if let Some(response) = self
                .request_terminal_permission(&id, params.clone(), cancel)
                .await?
            {
                return Ok(response);
            }
        }
        let request = JsonRpcRequest::new_with_id(id.clone(), method, params);
        let (tx, rx) = oneshot::channel();
        self.pending.lock().await.insert(id.clone(), tx);
        self.write_json(&request).await?;

        tokio::select! {
            response = rx => response.map_err(|_| format!("request {id} response channel closed")),
            _ = cancel.cancelled() => {
                self.pending.lock().await.remove(&id);
                Err("cancelled while waiting for permission response".to_string())
            }
            _ = tokio::time::sleep(Duration::from_secs(600)) => {
                self.pending.lock().await.remove(&id);
                Err(format!("timeout waiting for response to {id}"))
            }
        }
    }

    async fn request_terminal_permission(
        &self,
        id: &str,
        params: Option<Value>,
        cancel: &CancellationToken,
    ) -> Result<Option<JsonRpcResponse>, String> {
        let Some(terminal_events) = self.terminal_events.as_ref() else {
            return Ok(None);
        };
        let (response_tx, response_rx) = oneshot::channel();
        if terminal_events
            .send(TerminalEvent::PermissionRequest {
                request_id: id.to_string(),
                params,
                response_tx,
            })
            .is_err()
        {
            return Ok(None);
        }

        tokio::select! {
            response = response_rx => response
                .map(Some)
                .map_err(|_| format!("terminal permission request {id} response channel closed")),
            _ = cancel.cancelled() => Err("cancelled while waiting for terminal permission response".to_string()),
            _ = tokio::time::sleep(Duration::from_secs(600)) => {
                Err(format!("timeout waiting for terminal permission response to {id}"))
            }
        }
    }

    async fn write_json<T>(&self, value: &T) -> Result<(), String>
    where
        T: Serialize + ?Sized,
    {
        let mut line = serde_json::to_vec(value).map_err(|err| err.to_string())?;
        line.push(b'\n');
        let mut writer = self.writer.lock().await;
        writer
            .write_all(&line)
            .await
            .map_err(|err| err.to_string())?;
        writer.flush().await.map_err(|err| err.to_string())
    }

    async fn resolve_pending(&self, response: JsonRpcResponse) -> bool {
        let tx = self.pending.lock().await.remove(&response.id);
        if let Some(tx) = tx {
            let _ = tx.send(response);
            true
        } else {
            false
        }
    }

    pub(crate) fn send_terminal_event(&self, event: TerminalEvent) {
        if let Some(terminal_events) = self.terminal_events.as_ref() {
            let _ = terminal_events.send(event);
        }
    }

    #[cfg(test)]
    pub(crate) async fn inject_response(&self, response: JsonRpcResponse) -> bool {
        self.resolve_pending(response).await
    }
}

pub async fn serve_io<R, W>(reader: R, writer: W, runtime: NativeRuntime) -> anyhow::Result<()>
where
    R: AsyncRead + Send + Unpin + 'static,
    W: AsyncWrite + Send + Unpin + 'static,
{
    serve_io_inner(reader, writer, runtime, None).await
}

pub(crate) async fn serve_io_with_events<R, W>(
    reader: R,
    writer: W,
    runtime: NativeRuntime,
    terminal_events: TerminalEventSink,
) -> anyhow::Result<()>
where
    R: AsyncRead + Send + Unpin + 'static,
    W: AsyncWrite + Send + Unpin + 'static,
{
    serve_io_inner(reader, writer, runtime, Some(terminal_events)).await
}

async fn serve_io_inner<R, W>(
    reader: R,
    writer: W,
    runtime: NativeRuntime,
    terminal_events: Option<TerminalEventSink>,
) -> anyhow::Result<()>
where
    R: AsyncRead + Send + Unpin + 'static,
    W: AsyncWrite + Send + Unpin + 'static,
{
    let mut rpc = RpcHandle::new(writer);
    if let Some(terminal_events) = terminal_events {
        rpc = rpc.with_terminal_events(terminal_events);
    }
    let runtime = Arc::new(runtime);
    let mut lines = BufReader::new(reader).lines();

    while let Some(line) = lines.next_line().await? {
        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }
        match brehon_adapter_sdk::protocol::parse_message(trimmed) {
            Ok(JsonRpcMessage::Response(response)) => {
                if !rpc.resolve_pending(response).await {
                    debug!("received unmatched JSON-RPC response");
                }
            }
            Ok(JsonRpcMessage::Notification(notification)) => {
                let runtime = Arc::clone(&runtime);
                let rpc = rpc.clone();
                tokio::spawn(async move {
                    runtime.handle_notification(&rpc, notification).await;
                });
            }
            Ok(JsonRpcMessage::Request(request)) => {
                let runtime = Arc::clone(&runtime);
                let rpc = rpc.clone();
                tokio::spawn(async move {
                    let shutdown = request.method == "shutdown";
                    let response = runtime.handle_request(&rpc, request).await;
                    if let Err(err) = rpc.send_response(response).await {
                        warn!(error = %err, "failed to send JSON-RPC response");
                    }
                    if shutdown {
                        tokio::time::sleep(Duration::from_millis(20)).await;
                        std::process::exit(0);
                    }
                });
            }
            Err(err) => {
                warn!(error = %err.message, raw = trimmed, "failed to parse JSON-RPC message");
            }
        }
    }

    Ok(())
}

pub fn rpc_error(id: String, code: i32, message: impl Into<String>) -> JsonRpcResponse {
    JsonRpcResponse::error(id, JsonRpcError::new(code, message))
}
