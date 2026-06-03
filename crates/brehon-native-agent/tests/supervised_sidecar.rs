#![cfg(unix)]

use std::process::Stdio;
use std::sync::Arc;
use std::time::Duration;

use serde_json::{json, Value};
use tokio::io::{AsyncBufReadExt, AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt, BufReader};
use tokio::net::UnixStream;
use tokio::process::Command;
use tokio::sync::Mutex;

#[tokio::test]
async fn supervised_sidecar_serves_acp_and_updates_visible_tui() {
    let root = tempfile::tempdir().expect("tempdir");
    let socket_path = root.path().join("native-agent.sock");
    let ready_path = root.path().join("native-agent.ready.json");
    let bin = env!("CARGO_BIN_EXE_brehon-native-agent");

    let mut child = Command::new(bin)
        .arg("--supervised")
        .arg("--provider")
        .arg("fake")
        .arg("--model")
        .arg("fake-model")
        .arg("--no-brehon-tools")
        .arg("--socket-path")
        .arg(&socket_path)
        .arg("--ready-file")
        .arg(&ready_path)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .expect("spawn supervised native agent");
    let mut child_stdin = child.stdin.take().expect("child stdin");
    let stdout = child.stdout.take().expect("child stdout");
    let stdout_buffer = Arc::new(Mutex::new(Vec::new()));
    let stdout_task = tokio::spawn(capture_output(stdout, stdout_buffer.clone()));

    wait_for_ready(&ready_path).await;

    let stream = UnixStream::connect(&socket_path)
        .await
        .expect("connect sidecar socket");
    let (reader, mut writer) = stream.into_split();
    let mut lines = BufReader::new(reader).lines();

    write_rpc(
        &mut writer,
        json!({"jsonrpc":"2.0","id":"init-1","method":"initialize","params":{"protocolVersion":1,"clientCapabilities":{}}}),
    )
    .await;
    let init = read_response(&mut lines, "init-1").await;
    assert_eq!(
        init["result"]["agentCapabilities"]["permission_support"],
        true
    );

    write_rpc(
        &mut writer,
        json!({"jsonrpc":"2.0","id":"sess-1","method":"session/new","params":{"cwd":root.path(),"mcpServers":[]}}),
    )
    .await;
    let session = read_response(&mut lines, "sess-1").await;
    let session_id = session["result"]["sessionId"]
        .as_str()
        .expect("session id")
        .to_string();

    write_rpc(
        &mut writer,
        json!({
            "jsonrpc":"2.0",
            "id":"prompt-1",
            "method":"session/prompt",
            "params":{
                "sessionId": session_id,
                "prompt":[{"type":"text","text":"hello supervised sidecar"}]
            }
        }),
    )
    .await;

    let mut saw_acp_text = false;
    loop {
        let message = read_next(&mut lines).await;
        if message.get("method").and_then(Value::as_str) == Some("session/update") {
            let update = &message["params"]["update"];
            if update["sessionUpdate"] == "agent_message_chunk"
                && update["content"]["text"]
                    .as_str()
                    .is_some_and(|text| text.contains("hello supervised sidecar"))
            {
                saw_acp_text = true;
            }
        }
        if message.get("id").and_then(Value::as_str) == Some("prompt-1") {
            assert_eq!(message["result"]["stopReason"], "stop");
            break;
        }
    }
    assert!(saw_acp_text, "expected ACP assistant text update");

    write_rpc(
        &mut writer,
        json!({
            "jsonrpc":"2.0",
            "id":"prompt-2",
            "method":"session/prompt",
            "params":{
                "sessionId": session_id,
                "prompt":[{"type":"text","text":"fake-write-file"}]
            }
        }),
    )
    .await;
    wait_for_stdout_contains(&stdout_buffer, "write_file").await;
    child_stdin
        .write_all(b"y\n")
        .await
        .expect("approve permission");

    loop {
        let message = read_next(&mut lines).await;
        if message.get("id").and_then(Value::as_str) == Some("prompt-2") {
            assert_eq!(message["result"]["stopReason"], "stop");
            break;
        }
    }
    let written = tokio::fs::read_to_string(root.path().join("native-agent-permission.txt"))
        .await
        .expect("permission-gated write should complete");
    assert_eq!(written, "approved by native agent\n");

    tokio::time::sleep(Duration::from_millis(100)).await;
    let _ = child.kill().await;
    let _ = child.wait().await.expect("wait for child");
    stdout_task.await.expect("stdout capture");
    let stdout_bytes = stdout_buffer.lock().await.clone();
    let stdout = String::from_utf8_lossy(&stdout_bytes);
    assert!(stdout.contains("Brehon Native Agent"), "{stdout}");
    assert!(stdout.contains("Chat"), "{stdout}");
    assert!(stdout.contains("connected"), "{stdout}");
    assert!(stdout.contains("hello supervised sidecar"), "{stdout}");
    assert!(
        stdout.contains("approved once  write: write_file"),
        "{stdout}"
    );
}

async fn wait_for_ready(path: &std::path::Path) {
    let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
    loop {
        if tokio::fs::metadata(path).await.is_ok() {
            return;
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "timed out waiting for {}",
            path.display()
        );
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
}

async fn write_rpc<W>(writer: &mut W, value: Value)
where
    W: AsyncWrite + Unpin,
{
    let mut data = serde_json::to_vec(&value).expect("serialize rpc");
    data.push(b'\n');
    writer.write_all(&data).await.expect("write rpc");
    writer.flush().await.expect("flush rpc");
}

async fn capture_output<R>(mut reader: R, buffer: Arc<Mutex<Vec<u8>>>)
where
    R: AsyncRead + Unpin,
{
    let mut chunk = [0_u8; 4096];
    loop {
        match reader.read(&mut chunk).await {
            Ok(0) => break,
            Ok(n) => buffer.lock().await.extend_from_slice(&chunk[..n]),
            Err(_) => break,
        }
    }
}

async fn wait_for_stdout_contains(buffer: &Arc<Mutex<Vec<u8>>>, needle: &str) {
    let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
    loop {
        let contains = {
            let bytes = buffer.lock().await;
            String::from_utf8_lossy(&bytes).contains(needle)
        };
        if contains {
            return;
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "timed out waiting for stdout to contain {needle}"
        );
        tokio::time::sleep(Duration::from_millis(25)).await;
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
    let line = lines
        .next_line()
        .await
        .expect("read rpc line")
        .expect("rpc line");
    serde_json::from_str(&line).expect("decode rpc")
}
