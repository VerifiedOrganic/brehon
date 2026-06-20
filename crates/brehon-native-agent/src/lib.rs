mod agent_runtime;
mod cli;
mod health;
mod permissions;
mod provider;
mod runtime;
mod server;
mod shell;
mod terminal;
mod tools;
mod ui;

use std::path::PathBuf;

use tokio::net::UnixListener;
use tracing::{info, warn};

pub use cli::Cli;
pub use runtime::PermissionMode;

pub async fn run(cli: Cli) -> anyhow::Result<()> {
    let runtime = runtime::NativeRuntime::from_cli(&cli)?;
    if cli.supervised {
        run_supervised(cli, runtime).await
    } else {
        run_worker(runtime).await
    }
}

async fn run_worker(runtime: runtime::NativeRuntime) -> anyhow::Result<()> {
    server::serve_io(tokio::io::stdin(), tokio::io::stdout(), runtime).await
}

async fn run_supervised(cli: Cli, runtime: runtime::NativeRuntime) -> anyhow::Result<()> {
    let socket_path = cli
        .socket_path
        .clone()
        .or_else(|| std::env::var("BREHON_NATIVE_AGENT_SOCKET").ok())
        .map(PathBuf::from)
        .ok_or_else(|| anyhow::anyhow!("--supervised requires BREHON_NATIVE_AGENT_SOCKET"))?;
    let ready_path = cli
        .ready_file
        .clone()
        .or_else(|| std::env::var("BREHON_NATIVE_AGENT_READY_FILE").ok())
        .map(PathBuf::from);

    if let Some(parent) = socket_path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    if socket_path.exists() {
        std::fs::remove_file(&socket_path)?;
    }

    let listener = UnixListener::bind(&socket_path)?;
    let (terminal_tx, terminal_rx) = ui::event_channel();
    if let Some(path) = ready_path.as_ref() {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let ready = serde_json::json!({
            "socket_path": socket_path,
            "pid": std::process::id(),
            "protocol": "acp",
            "mode": "supervised"
        });
        std::fs::write(path, serde_json::to_vec_pretty(&ready)?)?;
    }

    let model = runtime
        .configured_model()
        .unwrap_or_else(|| "provider-default".to_string());
    let terminal_config = ui::TerminalUiConfig {
        provider: cli.provider.clone(),
        model,
        socket_path: socket_path.display().to_string(),
    };
    tokio::spawn(async move {
        if let Err(err) = ui::run_ratatui_terminal_ui(terminal_config, terminal_rx).await {
            warn!(error = %err, "native-agent terminal UI stopped");
        }
    });

    loop {
        let (stream, _) = listener.accept().await?;
        let _ = terminal_tx.send(ui::TerminalEvent::GatewayConnected);
        info!("accepted ACP sidecar connection");
        let (reader, writer) = stream.into_split();
        match server::serve_io_with_events(reader, writer, runtime.clone(), terminal_tx.clone())
            .await
        {
            Ok(()) => {
                let _ = terminal_tx.send(ui::TerminalEvent::GatewayDisconnected { error: None });
            }
            Err(err) => {
                let message = err.to_string();
                let _ = terminal_tx.send(ui::TerminalEvent::GatewayDisconnected {
                    error: Some(message.clone()),
                });
                warn!(error = %message, "ACP sidecar connection ended with error");
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::{json, Value};
    use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};

    #[tokio::test]
    async fn worker_lifecycle_handles_prompt() {
        let cli = Cli {
            worker: true,
            supervised: false,
            provider: "fake".to_string(),
            base_url: None,
            api_key_env: None,
            headers: Vec::new(),
            model: Some("fake-model".to_string()),
            reasoning_effort: None,
            reasoning_effort_param: None,
            extra_body_json: None,
            permission_mode: Some("default".to_string()),
            max_parallel_tool_calls: None,
            context_window: None,
            stream_idle_timeout_secs: None,
            assistant_message_passthrough_fields: Vec::new(),
            permission_policy_json: None,
            env_allowlist: Vec::new(),
            tool_prefix: "mcp_brehon_".to_string(),
            no_brehon_tools: true,
            socket_path: None,
            ready_file: None,
        };
        let runtime = runtime::NativeRuntime::from_cli(&cli).unwrap();
        let (client, server_stream) = tokio::io::duplex(16 * 1024);
        let (server_reader, server_writer) = tokio::io::split(server_stream);
        tokio::spawn(async move {
            server::serve_io(server_reader, server_writer, runtime)
                .await
                .unwrap();
        });

        let (client_reader, mut client_writer) = tokio::io::split(client);
        let mut lines = BufReader::new(client_reader).lines();

        write_rpc(
            &mut client_writer,
            json!({"jsonrpc":"2.0","id":"init-1","method":"initialize","params":{"protocolVersion":1,"clientCapabilities":{}}}),
        )
        .await;
        let init = read_response(&mut lines, "init-1").await;
        assert_eq!(
            init["result"]["agentCapabilities"]["permission_support"],
            true
        );
        assert_eq!(
            init["result"]["agentCapabilities"]["terminal_support"],
            true
        );

        write_rpc(
            &mut client_writer,
            json!({"jsonrpc":"2.0","id":"sess-1","method":"session/new","params":{"cwd":".","mcpServers":[]}}),
        )
        .await;
        let session = read_response(&mut lines, "sess-1").await;
        let session_id = session["result"]["sessionId"].as_str().unwrap();

        write_rpc(
            &mut client_writer,
            json!({
                "jsonrpc":"2.0",
                "id":"prompt-1",
                "method":"session/prompt",
                "params":{
                    "sessionId": session_id,
                    "prompt":[{"type":"text","text":"hello"}]
                }
            }),
        )
        .await;

        let mut saw_output = false;
        loop {
            let message = read_next(&mut lines).await;
            if message.get("method").and_then(Value::as_str) == Some("session/update") {
                let update = &message["params"]["update"];
                if update["sessionUpdate"] == "agent_message_chunk" {
                    saw_output = true;
                }
            }
            if message.get("id").and_then(Value::as_str) == Some("prompt-1") {
                assert_eq!(message["result"]["stopReason"], "stop");
                break;
            }
        }
        assert!(saw_output, "expected session/update output");
    }

    #[tokio::test]
    async fn reviewer_lifecycle_uses_reviewer_role_contract() {
        let saved_role = std::env::var("BREHON_AGENT_ROLE").ok();
        let saved_name = std::env::var("BREHON_AGENT_NAME").ok();
        let saved_type = std::env::var("BREHON_AGENT_TYPE").ok();
        std::env::set_var("BREHON_AGENT_ROLE", "reviewer");
        std::env::set_var("BREHON_AGENT_NAME", "reviewer-1");
        std::env::set_var("BREHON_AGENT_TYPE", "native-reviewer");

        let cli = Cli {
            worker: true,
            supervised: false,
            provider: "fake".to_string(),
            base_url: None,
            api_key_env: None,
            headers: Vec::new(),
            model: Some("fake-model".to_string()),
            reasoning_effort: None,
            reasoning_effort_param: None,
            extra_body_json: None,
            permission_mode: Some("default".to_string()),
            max_parallel_tool_calls: None,
            context_window: None,
            stream_idle_timeout_secs: None,
            assistant_message_passthrough_fields: Vec::new(),
            permission_policy_json: None,
            env_allowlist: Vec::new(),
            tool_prefix: "mcp_brehon_".to_string(),
            no_brehon_tools: true,
            socket_path: None,
            ready_file: None,
        };
        let runtime = runtime::NativeRuntime::from_cli(&cli).unwrap();
        restore_env("BREHON_AGENT_ROLE", saved_role);
        restore_env("BREHON_AGENT_NAME", saved_name);
        restore_env("BREHON_AGENT_TYPE", saved_type);

        let (client, server_stream) = tokio::io::duplex(16 * 1024);
        let (server_reader, server_writer) = tokio::io::split(server_stream);
        tokio::spawn(async move {
            server::serve_io(server_reader, server_writer, runtime)
                .await
                .unwrap();
        });

        let (client_reader, mut client_writer) = tokio::io::split(client);
        let mut lines = BufReader::new(client_reader).lines();

        write_rpc(
            &mut client_writer,
            json!({"jsonrpc":"2.0","id":"sess-1","method":"session/new","params":{"cwd":".","mcpServers":[]}}),
        )
        .await;
        let session = read_response(&mut lines, "sess-1").await;
        let session_id = session["result"]["sessionId"].as_str().unwrap();

        write_rpc(
            &mut client_writer,
            json!({
                "jsonrpc":"2.0",
                "id":"prompt-1",
                "method":"session/prompt",
                "params":{
                    "sessionId": session_id,
                    "prompt":[{"type":"text","text":"fake-report-role"}]
                }
            }),
        )
        .await;

        let mut saw_reviewer_role = false;
        loop {
            let message = read_next(&mut lines).await;
            if message.get("method").and_then(Value::as_str) == Some("session/update") {
                let update = &message["params"]["update"];
                if update["sessionUpdate"] == "agent_message_chunk"
                    && update["content"]["text"]
                        .as_str()
                        .is_some_and(|text| text.contains("fake role: reviewer"))
                {
                    saw_reviewer_role = true;
                }
            }
            if message.get("id").and_then(Value::as_str) == Some("prompt-1") {
                assert_eq!(message["result"]["stopReason"], "stop");
                break;
            }
        }
        assert!(
            saw_reviewer_role,
            "reviewer prompt did not use reviewer role"
        );
    }

    #[tokio::test]
    async fn terminal_attach_and_input_route_to_supervised_ui() {
        use base64::{engine::general_purpose::STANDARD, Engine};

        let cli = Cli {
            worker: true,
            supervised: true,
            provider: "fake".to_string(),
            base_url: None,
            api_key_env: None,
            headers: Vec::new(),
            model: Some("fake-model".to_string()),
            reasoning_effort: None,
            reasoning_effort_param: None,
            extra_body_json: None,
            permission_mode: Some("default".to_string()),
            max_parallel_tool_calls: None,
            context_window: None,
            stream_idle_timeout_secs: None,
            assistant_message_passthrough_fields: Vec::new(),
            permission_policy_json: None,
            env_allowlist: Vec::new(),
            tool_prefix: "mcp_brehon_".to_string(),
            no_brehon_tools: true,
            socket_path: None,
            ready_file: None,
        };
        let runtime = runtime::NativeRuntime::from_cli(&cli).unwrap();
        let (terminal_tx, mut terminal_rx) = ui::event_channel();
        let (client, server_stream) = tokio::io::duplex(16 * 1024);
        let (server_reader, server_writer) = tokio::io::split(server_stream);
        tokio::spawn(async move {
            server::serve_io_with_events(server_reader, server_writer, runtime, terminal_tx)
                .await
                .unwrap();
        });

        let (client_reader, mut client_writer) = tokio::io::split(client);
        let mut lines = BufReader::new(client_reader).lines();

        write_rpc(
            &mut client_writer,
            json!({"jsonrpc":"2.0","id":"sess-1","method":"session/new","params":{"cwd":".","mcpServers":[]}}),
        )
        .await;
        let session = read_response(&mut lines, "sess-1").await;
        let session_id = session["result"]["sessionId"].as_str().unwrap();

        write_rpc(
            &mut client_writer,
            json!({
                "jsonrpc":"2.0",
                "id":"term-1",
                "method":"terminal_attach",
                "params":{"sessionId": session_id, "cols": 100, "rows": 30}
            }),
        )
        .await;
        let terminal = read_response(&mut lines, "term-1").await;
        let terminal_id = terminal["result"]["terminalId"].as_str().unwrap();

        write_rpc(
            &mut client_writer,
            json!({
                "jsonrpc":"2.0",
                "id":"term-input-1",
                "method":"terminal_input",
                "params":{
                    "terminalId": terminal_id,
                    "input": STANDARD.encode(b"y\n")
                }
            }),
        )
        .await;
        let _ = read_response(&mut lines, "term-input-1").await;

        let event = tokio::time::timeout(std::time::Duration::from_secs(1), terminal_rx.recv())
            .await
            .expect("terminal event should arrive")
            .expect("terminal event channel open");
        match event {
            ui::TerminalEvent::TerminalInput { terminal_id, data } => {
                assert_eq!(
                    terminal_id,
                    terminal["result"]["terminalId"].as_str().unwrap()
                );
                assert_eq!(data, b"y\n");
            }
            _ => panic!("expected terminal input event"),
        }
    }

    async fn write_rpc<W>(writer: &mut W, value: Value)
    where
        W: tokio::io::AsyncWrite + Unpin,
    {
        let mut data = serde_json::to_vec(&value).unwrap();
        data.push(b'\n');
        writer.write_all(&data).await.unwrap();
        writer.flush().await.unwrap();
    }

    fn restore_env(name: &str, value: Option<String>) {
        match value {
            Some(value) => std::env::set_var(name, value),
            None => std::env::remove_var(name),
        }
    }

    async fn read_response<R>(lines: &mut tokio::io::Lines<BufReader<R>>, id: &str) -> Value
    where
        R: tokio::io::AsyncRead + Unpin,
    {
        loop {
            let message = read_next(lines).await;
            if message.get("id").and_then(Value::as_str) == Some(id) {
                return message;
            }
        }
    }

    async fn read_next<R>(lines: &mut tokio::io::Lines<BufReader<R>>) -> Value
    where
        R: tokio::io::AsyncRead + Unpin,
    {
        let line = lines.next_line().await.unwrap().unwrap();
        serde_json::from_str(&line).unwrap()
    }
}
