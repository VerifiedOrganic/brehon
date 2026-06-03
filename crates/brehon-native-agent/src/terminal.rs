// SPDX-FileCopyrightText: 2026 Andrei G <bug-ops>
// SPDX-License-Identifier: MIT OR Apache-2.0
//
// Inspired by Zeph `zeph-acp/src/terminal.rs`, adapted for Brehon's native ACP
// server shape.

use std::collections::HashMap;
use std::sync::Arc;

use base64::{engine::general_purpose::STANDARD, Engine};
use serde_json::{json, Value};
use tokio::sync::Mutex;

const MAX_TERMINAL_INPUT_BYTES: usize = 65_536;

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct NativeTerminalSession {
    pub(crate) terminal_id: String,
    pub(crate) session_id: String,
    pub(crate) cols: u16,
    pub(crate) rows: u16,
}

#[derive(Debug, Clone, Default)]
pub(crate) struct NativeTerminalManager {
    sessions: Arc<Mutex<HashMap<String, NativeTerminalSession>>>,
}

impl NativeTerminalManager {
    pub(crate) async fn attach(
        &self,
        session_id: String,
        cols: Option<u16>,
        rows: Option<u16>,
    ) -> Value {
        let terminal_id = format!("native-term-{}", uuid::Uuid::new_v4());
        let terminal = NativeTerminalSession {
            terminal_id: terminal_id.clone(),
            session_id,
            cols: cols.unwrap_or(80).clamp(20, 400),
            rows: rows.unwrap_or(24).clamp(8, 200),
        };
        self.sessions
            .lock()
            .await
            .insert(terminal_id.clone(), terminal);
        json!({ "terminalId": terminal_id })
    }

    pub(crate) async fn decode_input(&self, params: &Value) -> Result<TerminalInput, String> {
        let terminal_id = string_param(params, &["terminalId", "terminal_id"])?;
        let encoded = string_param(params, &["input", "data"])?;
        let terminal = self
            .sessions
            .lock()
            .await
            .get(&terminal_id)
            .cloned()
            .ok_or_else(|| format!("unknown terminal {terminal_id}"))?;
        let data = STANDARD
            .decode(encoded.as_bytes())
            .map_err(|err| format!("invalid terminal input base64: {err}"))?;
        if data.len() > MAX_TERMINAL_INPUT_BYTES {
            return Err(format!(
                "terminal input too large: {} bytes exceeds {} byte limit",
                data.len(),
                MAX_TERMINAL_INPUT_BYTES
            ));
        }
        Ok(TerminalInput {
            terminal_id: terminal.terminal_id,
            session_id: terminal.session_id,
            data,
        })
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct TerminalInput {
    pub(crate) terminal_id: String,
    pub(crate) session_id: String,
    pub(crate) data: Vec<u8>,
}

fn string_param(params: &Value, keys: &[&str]) -> Result<String, String> {
    keys.iter()
        .find_map(|key| params.get(*key).and_then(Value::as_str))
        .map(str::to_string)
        .filter(|value| !value.trim().is_empty())
        .ok_or_else(|| format!("missing required string field '{}'", keys[0]))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn attaches_and_decodes_terminal_input() {
        let manager = NativeTerminalManager::default();
        let result = manager
            .attach("session-1".to_string(), Some(100), Some(40))
            .await;
        let terminal_id = result["terminalId"].as_str().unwrap();
        let input = manager
            .decode_input(&json!({
                "terminalId": terminal_id,
                "input": STANDARD.encode(b"y\n"),
            }))
            .await
            .unwrap();

        assert_eq!(input.session_id, "session-1");
        assert_eq!(input.data, b"y\n");
    }

    #[tokio::test]
    async fn rejects_unknown_terminal_input() {
        let manager = NativeTerminalManager::default();
        let err = manager
            .decode_input(&json!({
                "terminalId": "missing",
                "input": STANDARD.encode(b"x"),
            }))
            .await
            .unwrap_err();

        assert!(err.contains("unknown terminal"));
    }
}
