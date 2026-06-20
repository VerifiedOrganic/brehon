use clap::Parser;

#[derive(Debug, Clone, Parser)]
#[command(author, version, about)]
pub struct Cli {
    /// Run as a headless ACP worker over stdin/stdout.
    #[arg(long, conflicts_with = "supervised")]
    pub worker: bool,
    /// Run as a visible supervisor process with ACP on a Unix socket.
    #[arg(long, conflicts_with = "worker")]
    pub supervised: bool,
    /// Provider backend. Currently supports openai-compatible and fake.
    #[arg(long, default_value = "openai-compatible")]
    pub provider: String,
    /// OpenAI-compatible API base URL.
    #[arg(long, alias = "api-base")]
    pub base_url: Option<String>,
    /// Environment variable containing the API key.
    #[arg(long)]
    pub api_key_env: Option<String>,
    /// Static HTTP header in KEY=VALUE form. Can be repeated.
    #[arg(long = "header")]
    pub headers: Vec<String>,
    /// Model identifier. Defaults to BREHON_AGENT_MODEL when present.
    #[arg(long)]
    pub model: Option<String>,
    /// Generic reasoning effort hint forwarded as reasoning_effort.
    #[arg(long)]
    pub reasoning_effort: Option<String>,
    /// Dotted request-body path where reasoning effort should be written.
    #[arg(long)]
    pub reasoning_effort_param: Option<String>,
    /// Extra JSON object merged into each chat/completions request body.
    #[arg(long)]
    pub extra_body_json: Option<String>,
    /// Permission mode: default, accept-edits, plan, or bypass.
    #[arg(long)]
    pub permission_mode: Option<String>,
    /// Maximum native tool calls to execute concurrently.
    #[arg(long)]
    pub max_parallel_tool_calls: Option<usize>,
    /// Approximate per-sequence context window (tokens) of the endpoint. When
    /// set, conversation history is trimmed to fit so small local context
    /// windows stay off llama.cpp's hard context-overflow error. Falls back to
    /// BREHON_AGENT_CONTEXT_WINDOW; unset means no token-based trimming.
    #[arg(long)]
    pub context_window: Option<usize>,
    /// Maximum seconds a streaming model call may go without provider activity.
    #[arg(long)]
    pub stream_idle_timeout_secs: Option<u64>,
    /// Assistant message extension field to preserve across native tool-call subturns.
    #[arg(long = "assistant-message-passthrough-field")]
    pub assistant_message_passthrough_fields: Vec<String>,
    /// JSON-encoded Brehon permission policy.
    #[arg(long)]
    pub permission_policy_json: Option<String>,
    /// Environment variable names exposed to bash and in-process Brehon tools.
    /// BREHON_* runtime variables are always preserved.
    #[arg(long = "env-allowlist", value_delimiter = ',')]
    pub env_allowlist: Vec<String>,
    /// Tool prefix for in-process Brehon MCP tools.
    #[arg(long, default_value = "mcp_brehon_")]
    pub tool_prefix: String,
    /// Disable in-process Brehon MCP tools and expose only coding tools.
    #[arg(long)]
    pub no_brehon_tools: bool,
    /// Unix socket path for supervised ACP mode.
    #[arg(long)]
    pub socket_path: Option<String>,
    /// Ready-file path for supervised ACP mode.
    #[arg(long)]
    pub ready_file: Option<String>,
}
