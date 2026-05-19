//! MCP server backed by the `rmcp` crate.
//!
//! All protocol negotiation (initialize, notifications/initialized, ping,
//! resources/list, etc.) is handled by rmcp. We implement `ServerHandler`
//! and delegate tool listing/calling to our existing `Tool` trait registry.

use std::any::Any;
use std::collections::HashMap;
use std::future::Future;
use std::io::Write;
use std::panic::AssertUnwindSafe;
use std::path::PathBuf;
use std::sync::Arc;

use futures_util::{FutureExt, SinkExt, StreamExt};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use tokio::io::{AsyncRead, AsyncWrite};
use tokio::sync::Mutex;
use tokio_util::codec::{FramedRead, FramedWrite};

use brehon_ports::EventStore;
use brehon_ports::RunStore;
use brehon_ports::SearchIndex;

use crate::error::McpError;
use crate::tools;
use crate::tools::Tool;

// ── Public types (kept for compatibility with tool impls) ───────────────────

/// Thread-safe handle to an event store for persisting domain events.
pub type EventHandler = Arc<dyn EventStore + Send + Sync>;
/// Thread-safe handle to a search index for full-text queries.
pub type SearchHandler = Arc<dyn SearchIndex + Send + Sync>;
/// Thread-safe handle to a proof projection store.
pub type ProofHandler = Arc<dyn brehon_ports::ProofStore + Send + Sync>;
/// Thread-safe handle to durable run records.
pub type RunHandler = Arc<dyn RunStore + Send + Sync>;

/// Metadata describing an MCP tool: its name, description, and JSON Schema input.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ToolDefinition {
    pub name: String,
    pub description: String,
    pub input_schema: Value,
}

/// Result returned by a tool execution, containing content blocks and an optional error flag.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ToolResult {
    pub content: Vec<ContentBlock>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub is_error: Option<bool>,
}

/// A single content block within a tool result -- either text or an image.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type")]
pub enum ContentBlock {
    #[serde(rename = "text")]
    Text { text: String },
    #[serde(rename = "image")]
    Image { data: String, mime_type: String },
}

const DEFAULT_MAX_TOOL_ARGUMENT_BYTES: usize = 256 * 1024;
const TRANSPORT_FRAME_HEADROOM_BYTES: usize = 4 * 1024 * 1024;

#[derive(Debug, Clone, PartialEq, Eq)]
struct CallerAttribution {
    session_id: String,
    agent_name: String,
    role: String,
}

impl CallerAttribution {
    fn from_env() -> Self {
        Self {
            session_id: env_value_or_unknown("BREHON_SESSION_ID"),
            agent_name: env_value_or_unknown("BREHON_AGENT_NAME"),
            role: env_value_or_unknown("BREHON_AGENT_ROLE"),
        }
    }

    fn label(&self) -> String {
        format!(
            "session={},agent={},role={}",
            self.session_id, self.agent_name, self.role
        )
    }
}

#[derive(Default)]
struct ByteCountingWriter {
    written: usize,
}

impl Write for ByteCountingWriter {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        self.written = self.written.saturating_add(buf.len());
        Ok(buf.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

fn env_value_or_unknown(key: &str) -> String {
    std::env::var(key)
        .ok()
        .map(|value| value.trim().to_string())
        .filter(|value| !value.is_empty())
        .unwrap_or_else(|| "unknown".to_string())
}

fn configured_max_tool_argument_bytes() -> usize {
    std::env::var("BREHON_MCP_MAX_REQUEST_BYTES")
        .ok()
        .and_then(|raw| raw.trim().parse::<usize>().ok())
        .filter(|value| *value > 0)
        .unwrap_or(DEFAULT_MAX_TOOL_ARGUMENT_BYTES)
}

fn serialized_json_size_bytes(value: &Value) -> Result<usize, McpError> {
    let mut writer = ByteCountingWriter::default();
    serde_json::to_writer(&mut writer, value).map_err(|err| {
        McpError::Serialization(format!("Failed to measure tool args size: {err}"))
    })?;
    Ok(writer.written)
}

fn panic_payload_to_string(payload: &(dyn Any + Send)) -> String {
    if let Some(msg) = payload.downcast_ref::<&str>() {
        return (*msg).to_string();
    }
    if let Some(msg) = payload.downcast_ref::<String>() {
        return msg.clone();
    }
    "non-string panic payload".to_string()
}

fn structured_tool_error(
    code: &str,
    message: String,
    tool: &str,
    caller: &CallerAttribution,
    fields: serde_json::Value,
) -> String {
    serde_json::json!({
        "error": {
            "code": code,
            "message": message,
            "tool": tool,
            "caller": {
                "session_id": caller.session_id,
                "agent_name": caller.agent_name,
                "role": caller.role,
            },
            "fields": fields,
        }
    })
    .to_string()
}

// ── Tool registry ───────────────────────────────────────────────────────────

/// The tool registry that holds all registered MCP tools.
/// Used internally by `BrehonService` to service rmcp requests.
pub struct McpServer {
    name: String,
    version: String,
    tools: HashMap<String, Box<dyn Tool>>,
    event_store: Option<EventHandler>,
    proof_store: Option<ProofHandler>,
    run_store: Option<RunHandler>,
    search_index: Option<SearchHandler>,
    caller: CallerAttribution,
    max_tool_argument_bytes: usize,
}

impl McpServer {
    /// Create a new server with the given name and version, and an empty tool registry.
    pub fn new(name: &str, version: &str) -> Self {
        Self {
            name: name.to_string(),
            version: version.to_string(),
            tools: HashMap::new(),
            event_store: None,
            proof_store: None,
            run_store: None,
            search_index: None,
            caller: CallerAttribution::from_env(),
            max_tool_argument_bytes: configured_max_tool_argument_bytes(),
        }
    }

    /// Attach an event store for domain event persistence.
    pub fn with_event_store(mut self, store: EventHandler) -> Self {
        self.event_store = Some(store);
        self
    }

    /// Attach a proof projection store so context/verification/integration
    /// tools can read and write durable proof evidence.
    pub fn with_proof_store(mut self, store: ProofHandler) -> Self {
        self.proof_store = Some(store);
        self
    }

    /// Attach a durable run store so task context can report active attempts.
    pub fn with_run_store(mut self, store: RunHandler) -> Self {
        self.run_store = Some(store);
        self
    }

    /// Attach a search index for full-text memory queries.
    pub fn with_search_index(mut self, index: SearchHandler) -> Self {
        self.search_index = Some(index);
        self
    }

    /// Register a single tool in the server's tool registry.
    pub fn register_tool(&mut self, tool: Box<dyn Tool>) {
        self.tools.insert(tool.name().to_string(), tool);
    }

    /// Register all built-in Brehon tools (health, memory, rules, skills, tasks, agent, factory, verification, task actions).
    pub fn register_builtin_tools(&mut self) {
        tools::health::init_start_time();

        self.register_tool(Box::new(tools::health::HealthCheckTool::new()));
        let mut search_memories = tools::memory::SearchMemoriesTool::new();
        if let Some(ref index) = self.search_index {
            search_memories = search_memories.with_search_index(index.clone());
        }
        if let Some(ref store) = self.event_store {
            search_memories = search_memories.with_event_store(store.clone());
        }
        self.register_tool(Box::new(search_memories));

        let mut create_memory = tools::memory::CreateMemoryTool::new();
        if let Some(ref index) = self.search_index {
            create_memory = create_memory.with_search_index(index.clone());
        }
        if let Some(ref store) = self.event_store {
            create_memory = create_memory.with_event_store(store.clone());
        }
        self.register_tool(Box::new(create_memory));

        let mut get_memories = tools::memory::GetMemoriesTool::new();
        if let Some(ref store) = self.event_store {
            get_memories = get_memories.with_event_store(store.clone());
        }
        self.register_tool(Box::new(get_memories));
        let mut list_memories = tools::memory::ListMemoriesTool::new();
        if let Some(ref store) = self.event_store {
            list_memories = list_memories.with_event_store(store.clone());
        }
        self.register_tool(Box::new(list_memories));
        let mut delete_memory = tools::memory::DeleteMemoryTool::new();
        if let Some(ref index) = self.search_index {
            delete_memory = delete_memory.with_search_index(index.clone());
        }
        if let Some(ref store) = self.event_store {
            delete_memory = delete_memory.with_event_store(store.clone());
        }
        self.register_tool(Box::new(delete_memory));

        self.register_tool(Box::new(tools::rules::SearchRulesTool::new()));
        self.register_tool(Box::new(tools::rules::CreateRuleTool::new()));

        self.register_tool(Box::new(tools::skills::SearchSkillsTool::new()));

        let mut task_context = tools::tasks::GetTaskContextTool::new();
        if let Some(ref store) = self.event_store {
            task_context = task_context.with_event_store(store.clone());
        }
        if let Some(ref store) = self.proof_store {
            task_context = task_context.with_proof_store(store.clone());
        }
        if let Some(ref store) = self.run_store {
            task_context = task_context.with_run_store(store.clone());
        }
        self.register_tool(Box::new(task_context));
        self.register_tool(Box::new(tools::tasks::ListTasksTool::new()));
        self.register_tool(Box::new(tools::tasks::GetTaskTool::new()));

        self.register_tool(Box::new(tools::agent::AgentTool::new()));
        self.register_tool(Box::new(tools::advisor::AdvisorTool::new()));
        self.register_tool(Box::new(tools::research::ResearchTool::new()));

        let mut ft = tools::factory::FactoryTool::new();
        if let Some(ref store) = self.event_store {
            ft = ft.with_event_store(store.clone());
        }
        self.register_tool(Box::new(ft));

        let mut vt = tools::verification::VerificationTool::new();
        if let Some(review_config) = load_project_review_config() {
            vt = vt.with_config(review_config);
        }
        if let Some(ref store) = self.event_store {
            vt = vt.with_event_store(store.clone());
        }
        if let Some(ref store) = self.proof_store {
            vt = vt.with_proof_store(store.clone());
        }
        self.register_tool(Box::new(vt));

        let mut task_actions = tools::task_actions::TaskActionsTool::new();
        if let Some(ref store) = self.event_store {
            task_actions = task_actions.with_event_store(store.clone());
        }
        if let Some(ref store) = self.proof_store {
            task_actions = task_actions.with_proof_store(store.clone());
        }
        self.register_tool(Box::new(task_actions));
    }

    /// Return the registered tools as MCP-style definitions.
    pub fn tool_definitions(&self) -> Vec<ToolDefinition> {
        let mut tools: Vec<ToolDefinition> = self
            .tools
            .values()
            .map(|tool| ToolDefinition {
                name: tool.name().to_string(),
                description: tool.description().to_string(),
                input_schema: tool.input_schema(),
            })
            .collect();
        tools.sort_by(|left, right| left.name.cmp(&right.name));
        tools
    }

    /// Execute a registered tool by name.
    ///
    /// Each tool call is tracked as in-flight work so that the shutdown
    /// drain waits for it to complete instead of aborting mid-operation.
    /// When the process is draining (shutdown in progress), new tool
    /// invocations are rejected so they don't start after drain has
    /// already observed zero in-flight work.
    ///
    /// The in-flight guard is registered BEFORE the draining check to
    /// close the race window: if shutdown flips the flag between
    /// registration and the check, the guard is still tracked and will
    /// be completed on drop, so drain cannot observe zero in-flight
    /// work while this call is mid-check.
    pub async fn call_tool(&self, tool_name: &str, args: Value) -> Result<ToolResult, McpError> {
        let caller = &self.caller;
        // Register with the in-flight tracker FIRST.  This eliminates the
        // TOCTOU race where is_draining() returns false but shutdown flips
        // before we are tracked — the drainer would see count==0 and return
        // while our call proceeds untracked.  By registering first, the
        // drainer always sees us if it has not yet observed count==0.
        let _guard =
            brehon_types::drain::in_flight_guard(&format!("mcp:{tool_name}:{}", caller.session_id));

        if brehon_types::drain::is_draining() {
            // _guard drops here, completing the in-flight entry immediately,
            // so the drainer will not wait for us.
            return Err(McpError::Protocol(format!(
                "Shutdown in progress — tool {tool_name} rejected during drain"
            )));
        }

        let tool = self
            .tools
            .get(tool_name)
            .ok_or_else(|| McpError::ToolNotFound(tool_name.to_string()))?;

        crate::tools::agent::refresh_current_session_file();
        let max_size = tool
            .max_argument_bytes()
            .map(|tool_max| tool_max.min(self.max_tool_argument_bytes))
            .unwrap_or(self.max_tool_argument_bytes);
        let request_size = serialized_json_size_bytes(&args)?;
        if request_size > max_size {
            return Err(McpError::InvalidRequest(structured_tool_error(
                "mcp_input_too_large",
                format!(
                    "Tool {tool_name} rejected oversized arguments from {}",
                    caller.label()
                ),
                tool_name,
                caller,
                serde_json::json!({
                    "request_bytes": request_size,
                    "max_bytes": max_size,
                    "server_max_bytes": self.max_tool_argument_bytes,
                    "tool_max_bytes": tool.max_argument_bytes(),
                }),
            )));
        }

        tracing::info!(
            tool = tool_name,
            session_id = %caller.session_id,
            agent_name = %caller.agent_name,
            role = %caller.role,
            request_bytes = request_size,
            "MCP tool call started"
        );

        match AssertUnwindSafe(tool.execute(args)).catch_unwind().await {
            Ok(result) => {
                if let Err(err) = &result {
                    tracing::warn!(
                        tool = tool_name,
                        session_id = %caller.session_id,
                        agent_name = %caller.agent_name,
                        role = %caller.role,
                        error = %err,
                        "MCP tool call failed"
                    );
                } else {
                    tracing::info!(
                        tool = tool_name,
                        session_id = %caller.session_id,
                        agent_name = %caller.agent_name,
                        role = %caller.role,
                        "MCP tool call completed"
                    );
                }
                result
            }
            Err(payload) => {
                let panic_message = panic_payload_to_string(payload.as_ref());
                tracing::error!(
                    tool = tool_name,
                    session_id = %caller.session_id,
                    agent_name = %caller.agent_name,
                    role = %caller.role,
                    panic = %panic_message,
                    "MCP tool call panicked"
                );
                Err(McpError::Internal(structured_tool_error(
                    "mcp_tool_panic",
                    format!("Tool {tool_name} panicked; caller {}", caller.label()),
                    tool_name,
                    caller,
                    serde_json::json!({ "panic": panic_message }),
                )))
            }
        }
    }

    /// Return a reference to the attached event store, if any.
    pub fn event_store(&self) -> Option<&(dyn EventStore + Send + Sync)> {
        self.event_store.as_deref()
    }

    /// Return a reference to the attached search index, if any.
    pub fn search_index(&self) -> Option<&(dyn SearchIndex + Send + Sync)> {
        self.search_index.as_deref()
    }

    fn max_tool_argument_bytes(&self) -> usize {
        self.max_tool_argument_bytes
    }
}

pub(crate) fn configured_project_root() -> Option<PathBuf> {
    if let Some(project_root) = std::env::var("BREHON_PROJECT_ROOT")
        .ok()
        .map(PathBuf::from)
        .filter(|path| !path.as_os_str().is_empty())
    {
        return Some(project_root);
    }

    let brehon_root = std::env::var("BREHON_ROOT")
        .ok()
        .map(PathBuf::from)
        .filter(|path| !path.as_os_str().is_empty())?;

    // BREHON_ROOT conventionally points at the `.brehon` directory; the project
    // root is its parent. Strip the suffix when present so callers downstream
    // (notably `brehon_config::load_config`, which appends `.brehon/config.yaml`)
    // can locate the project's config. Adapters that only propagate BREHON_ROOT
    // (codex, gemini, opencode) used to silently fall back to defaults here.
    if brehon_root.file_name().and_then(|name| name.to_str()) == Some(".brehon") {
        brehon_root.parent().map(PathBuf::from)
    } else {
        Some(brehon_root)
    }
}

fn load_project_review_config() -> Option<brehon_types::ReviewConfig> {
    let root = configured_project_root()?;
    brehon_config::load_config(Some(&root))
        .ok()
        .map(|config| config.review)
}

// ── rmcp ServerHandler ──────────────────────────────────────────────────────

use rmcp::model::{
    CallToolRequestParams, CallToolResult, Content, Implementation, ListToolsResult,
    PaginatedRequestParams, ProtocolVersion, ServerCapabilities, ServerInfo, Tool as RmcpTool,
};
use rmcp::service::{RequestContext, RoleServer, RxJsonRpcMessage, TxJsonRpcMessage};
use rmcp::transport::async_rw::JsonRpcMessageCodec;
use rmcp::transport::Transport;
use rmcp::{ErrorData as RmcpError, ServerHandler};

/// rmcp service wrapping our tool registry.
pub struct BrehonService {
    inner: Arc<McpServer>,
}

impl BrehonService {
    /// Wrap an `McpServer` tool registry as an rmcp-compatible service.
    pub fn new(server: McpServer) -> Self {
        Self {
            inner: Arc::new(server),
        }
    }
}

impl ServerHandler for BrehonService {
    fn get_info(&self) -> ServerInfo {
        // Build ACP bootstrap instructions from agent env vars.
        let instructions = if std::env::var("BREHON_AGENT_ROLE").is_ok() {
            Some(
                "You are an Brehon factory agent. \
                 On startup, immediately call the `agent` tool with \
                 `action=session_start`. The response contains your role, \
                 identity, and full operating instructions. Follow those \
                 instructions exactly. Do NOT start working on anything \
                 until you have completed the session_start handshake."
                    .to_string(),
            )
        } else {
            None
        };

        ServerInfo {
            protocol_version: ProtocolVersion::LATEST,
            capabilities: ServerCapabilities::builder().enable_tools().build(),
            server_info: Implementation {
                name: self.inner.name.clone(),
                version: self.inner.version.clone(),
                title: None,
                description: None,
                icons: None,
                website_url: None,
            },
            instructions,
        }
    }

    async fn list_tools(
        &self,
        _request: Option<PaginatedRequestParams>,
        _context: RequestContext<RoleServer>,
    ) -> Result<ListToolsResult, RmcpError> {
        let tools: Vec<RmcpTool> = self
            .inner
            .tool_definitions()
            .into_iter()
            .map(|tool| RmcpTool {
                name: tool.name.into(),
                description: Some(tool.description.into()),
                input_schema: serde_json::from_value(tool.input_schema).unwrap_or_default(),
                title: None,
                annotations: None,
                icons: None,
                meta: None,
                execution: None,
                output_schema: None,
            })
            .collect();

        Ok(ListToolsResult {
            tools,
            next_cursor: None,
            meta: None,
        })
    }

    async fn call_tool(
        &self,
        request: CallToolRequestParams,
        _context: RequestContext<RoleServer>,
    ) -> Result<CallToolResult, RmcpError> {
        let tool_name: &str = &request.name;
        self.inner.tools.get(tool_name).ok_or_else(|| {
            RmcpError::new(
                rmcp::model::ErrorCode::METHOD_NOT_FOUND,
                format!("Tool not found: {tool_name}"),
                None::<Value>,
            )
        })?;

        let args = request
            .arguments
            .map(serde_json::Value::Object)
            .unwrap_or(Value::Null);

        match self.inner.call_tool(tool_name, args).await {
            Ok(result) => {
                let content: Vec<Content> = result
                    .content
                    .into_iter()
                    .map(|block| match block {
                        ContentBlock::Text { text } => Content::text(text),
                        ContentBlock::Image { data, mime_type } => Content::image(data, mime_type),
                    })
                    .collect();

                Ok(CallToolResult {
                    content,
                    is_error: result.is_error,
                    meta: None,
                    structured_content: None,
                })
            }
            Err(e) => Ok(CallToolResult {
                content: vec![Content::text(e.to_string())],
                is_error: Some(true),
                meta: None,
                structured_content: None,
            }),
        }
    }
}

struct BoundedServerTransport<R, W>
where
    R: AsyncRead + Unpin,
    W: AsyncWrite + Unpin,
{
    read: FramedRead<R, JsonRpcMessageCodec<RxJsonRpcMessage<RoleServer>>>,
    write: Arc<Mutex<Option<FramedWrite<W, JsonRpcMessageCodec<TxJsonRpcMessage<RoleServer>>>>>>,
}

impl<R, W> BoundedServerTransport<R, W>
where
    R: Send + AsyncRead + Unpin,
    W: Send + AsyncWrite + Unpin + 'static,
{
    fn new(read: R, write: W, max_line_length: usize) -> Self {
        let read = FramedRead::new(
            read,
            JsonRpcMessageCodec::<RxJsonRpcMessage<RoleServer>>::new_with_max_length(
                max_line_length,
            ),
        );
        let write = Arc::new(Mutex::new(Some(FramedWrite::new(
            write,
            JsonRpcMessageCodec::<TxJsonRpcMessage<RoleServer>>::default(),
        ))));
        Self { read, write }
    }
}

impl<R, W> Transport<RoleServer> for BoundedServerTransport<R, W>
where
    R: Send + AsyncRead + Unpin,
    W: Send + AsyncWrite + Unpin + 'static,
{
    type Error = std::io::Error;

    fn send(
        &mut self,
        item: TxJsonRpcMessage<RoleServer>,
    ) -> impl Future<Output = Result<(), Self::Error>> + Send + 'static {
        let lock = self.write.clone();
        async move {
            let mut write = lock.lock().await;
            if let Some(ref mut write) = *write {
                write.send(item).await.map_err(Into::into)
            } else {
                Err(std::io::Error::new(
                    std::io::ErrorKind::NotConnected,
                    "Transport is closed",
                ))
            }
        }
    }

    fn receive(&mut self) -> impl Future<Output = Option<RxJsonRpcMessage<RoleServer>>> + Send {
        async move {
            // `tokio_util::FramedRead` yields exactly one `None` after a codec error as a
            // side-effect of tokio-rs/tokio#3976 (the "has_errored -> paused" transition), then
            // resumes polling the underlying reader. We want recoverable codec errors such as
            // `MaxLineLengthExceeded` to skip the bad frame and keep reading, so we track
            // whether the previous yield was an error and, if so, re-poll once past the
            // synthetic `None` to give the codec its recovery pass.
            let mut previous_was_err = false;
            loop {
                match self.read.next().await {
                    Some(Ok(message)) => return Some(message),
                    Some(Err(err)) => {
                        tracing::warn!(
                            "Discarding oversized/invalid MCP transport frame and continuing: {err}"
                        );
                        previous_was_err = true;
                    }
                    None => {
                        if previous_was_err {
                            // Consume the pseudo-EOF produced by FramedRead's
                            // has_errored -> paused transition and keep polling.
                            previous_was_err = false;
                            continue;
                        }
                        return None;
                    }
                }
            }
        }
    }

    async fn close(&mut self) -> Result<(), Self::Error> {
        let mut write = self.write.lock().await;
        drop(write.take());
        Ok(())
    }
}

/// Run the MCP server over stdio transport using rmcp.
pub async fn run_stdio(server: McpServer) -> Result<(), McpError> {
    use rmcp::ServiceExt;

    let max_message_bytes = server
        .max_tool_argument_bytes()
        .saturating_add(TRANSPORT_FRAME_HEADROOM_BYTES);
    let service = BrehonService::new(server);
    let transport =
        BoundedServerTransport::new(tokio::io::stdin(), tokio::io::stdout(), max_message_bytes);

    let running = service
        .serve(transport)
        .await
        .map_err(|e| McpError::Protocol(format!("Failed to start MCP server: {e}")))?;

    running
        .waiting()
        .await
        .map_err(|e| McpError::Protocol(format!("MCP server error: {e}")))?;

    Ok(())
}

#[cfg(test)]
#[path = "server_tests.rs"]
mod server_tests;
