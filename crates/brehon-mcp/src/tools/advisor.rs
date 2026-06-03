//! Advisor room tool.
//!
//! This is deliberately persistence-only: it records rooms, context, and
//! messages without waiting on model output. Dedicated advisor agents can watch
//! and post into the same room state, while the TUI remains responsive.

#[cfg(test)]
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use async_trait::async_trait;
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::error::McpError;
use crate::server::ToolResult;
use crate::tools::{error_result, text_result, Tool};

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
struct AdvisorContext {
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    docs: Vec<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    tasks: Vec<Value>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct AdvisorMessage {
    seq: u64,
    id: String,
    author: String,
    role: String,
    kind: String,
    content: String,
    created_at: DateTime<Utc>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct AdvisorRoomFile {
    room_id: String,
    title: Option<String>,
    turn_mode: String,
    #[serde(default)]
    participants: Vec<String>,
    #[serde(default)]
    context: AdvisorContext,
    #[serde(default)]
    messages: Vec<AdvisorMessage>,
    created_at: DateTime<Utc>,
    updated_at: DateTime<Utc>,
}

impl AdvisorRoomFile {
    fn new(room_id: String, title: Option<String>, turn_mode: Option<String>) -> Self {
        let now = Utc::now();
        Self {
            title,
            room_id,
            turn_mode: turn_mode.unwrap_or_else(|| "open_chat".to_string()),
            participants: Vec::new(),
            context: AdvisorContext::default(),
            messages: Vec::new(),
            created_at: now,
            updated_at: now,
        }
    }

    fn latest_seq(&self) -> u64 {
        self.messages.last().map_or(0, |message| message.seq)
    }
}

#[derive(Debug, Clone, Serialize)]
struct AdvisorRoomSummary {
    room_id: String,
    title: Option<String>,
    turn_mode: String,
    participants: Vec<String>,
    message_count: usize,
    latest_seq: u64,
    updated_at: DateTime<Utc>,
}

/// MCP tool for advisor rooms.
pub struct AdvisorTool;

impl Default for AdvisorTool {
    fn default() -> Self {
        Self::new()
    }
}

impl AdvisorTool {
    pub fn new() -> Self {
        Self
    }
}

#[async_trait]
impl Tool for AdvisorTool {
    fn name(&self) -> &str {
        "advisor"
    }

    fn description(&self) -> &str {
        "Persistent multi-agent advisor rooms for non-blocking brainstorming. \
         Actions: status, list_rooms, create_room, post, read, attach_context."
    }

    fn input_schema(&self) -> Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "action": {
                    "type": "string",
                    "description": "Action: status, list_rooms, create_room, post, read, attach_context"
                },
                "room_id": {
                    "type": "string",
                    "description": "Advisor room id"
                },
                "title": {
                    "type": "string",
                    "description": "Display title for create_room"
                },
                "turn_mode": {
                    "type": "string",
                    "description": "open_chat, round_robin, debate, synthesis, or watch"
                },
                "participants": {
                    "type": "array",
                    "items": {"type": "string"},
                    "description": "Advisor pool lanes or agent names for create_room"
                },
                "message": {
                    "type": "string",
                    "description": "Message content for post"
                },
                "author": {
                    "type": "string",
                    "description": "Message author; defaults to BREHON_AGENT_NAME or user"
                },
                "role": {
                    "type": "string",
                    "description": "Message role such as human, supervisor, advisor, system"
                },
                "kind": {
                    "type": "string",
                    "description": "Message kind, default message"
                },
                "after_seq": {
                    "type": "integer",
                    "description": "For read: return messages with seq greater than this"
                },
                "limit": {
                    "type": "integer",
                    "description": "For read: maximum messages to return, default 50"
                },
                "docs": {
                    "type": "array",
                    "items": {"type": "string"},
                    "description": "Repository-relative docs to attach"
                },
                "tasks": {
                    "type": "array",
                    "items": {},
                    "description": "Task ids or filter objects to attach"
                }
            },
            "required": ["action"]
        })
    }

    async fn execute(&self, args: Value) -> Result<ToolResult, McpError> {
        let action = args.get("action").and_then(Value::as_str).unwrap_or("");
        let result = match action {
            "status" => advisor_status(),
            "list_rooms" => list_rooms(),
            "create_room" => create_room(&args),
            "post" => post_message(&args),
            "read" => read_room(&args),
            "attach_context" => attach_context(&args),
            "" => Err("missing advisor action".to_string()),
            other => Err(format!("unknown advisor action '{other}'")),
        };

        match result {
            Ok(value) => Ok(text_result(
                serde_json::to_string_pretty(&value)
                    .map_err(|err| McpError::Serialization(err.to_string()))?,
            )),
            Err(message) => Ok(error_result(message)),
        }
    }
}

fn advisor_status() -> Result<Value, String> {
    let config = load_project_config();
    let rooms = read_room_summaries()?;
    let configured = config.as_ref().map(|config| {
        serde_json::json!({
            "enabled": config.advisors.enabled,
            "response_timeout_secs": config.advisors.response_timeout_secs,
            "default_turn_mode": config.advisors.default_turn_mode,
            "pools": config.advisors.pools.iter().map(|pool| {
                serde_json::json!({
                    "lane": pool.lane.clone(),
                    "min": pool.min,
                    "max": pool.max,
                    "rooms": pool.rooms.clone(),
                    "permissions": pool.permissions,
                })
            }).collect::<Vec<_>>(),
            "rooms": config.advisors.rooms.iter().map(|room| {
                serde_json::json!({
                    "id": room.id.clone(),
                    "title": room.title.clone(),
                    "turn_mode": room.turn_mode,
                    "participants": room.participants.clone(),
                    "context": room.context.clone(),
                })
            }).collect::<Vec<_>>(),
        })
    });

    Ok(serde_json::json!({
        "status": "ok",
        "brehon_root": brehon_root().display().to_string(),
        "configured": configured,
        "runtime_rooms": rooms,
        "next_action": "Use advisor action=post for human prompts and advisor replies; the tool returns immediately and does not wait on model output."
    }))
}

fn list_rooms() -> Result<Value, String> {
    Ok(serde_json::json!({
        "rooms": read_room_summaries()?,
    }))
}

fn create_room(args: &Value) -> Result<Value, String> {
    let room_id = required_room_id(args)?;
    let path = room_path(&room_id)?;
    let mut room = read_room_path(&path)?.unwrap_or_else(|| {
        AdvisorRoomFile::new(
            room_id.clone(),
            string_arg(args, "title"),
            string_arg(args, "turn_mode"),
        )
    });

    if let Some(title) = string_arg(args, "title") {
        room.title = Some(title);
    }
    if let Some(turn_mode) = string_arg(args, "turn_mode") {
        room.turn_mode = turn_mode;
    }
    if let Some(participants) = string_array_arg(args, "participants") {
        room.participants = participants;
    }
    room.updated_at = Utc::now();
    write_room_path(&path, &room)?;

    Ok(serde_json::json!({
        "status": "ok",
        "room": room_summary(&room),
        "created": room.messages.is_empty(),
        "next_action": "Post the user's question with advisor action=post; advisor agents should reply with advisor action=post."
    }))
}

fn post_message(args: &Value) -> Result<Value, String> {
    let room_id = required_room_id(args)?;
    let content = args
        .get("message")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .ok_or_else(|| "advisor action=post requires non-empty message".to_string())?;

    let path = room_path(&room_id)?;
    let mut room =
        read_room_path(&path)?.unwrap_or_else(|| AdvisorRoomFile::new(room_id.clone(), None, None));
    let seq = room.latest_seq() + 1;
    let now = Utc::now();
    let author = string_arg(args, "author")
        .or_else(|| std::env::var("BREHON_AGENT_NAME").ok())
        .unwrap_or_else(|| "user".to_string());
    let role = string_arg(args, "role")
        .or_else(|| std::env::var("BREHON_AGENT_ROLE").ok())
        .unwrap_or_else(|| "human".to_string());
    let kind = string_arg(args, "kind").unwrap_or_else(|| "message".to_string());

    room.messages.push(AdvisorMessage {
        seq,
        id: format!("m-{seq}-{}", now.timestamp_millis()),
        author,
        role,
        kind,
        content: content.to_string(),
        created_at: now,
    });
    room.updated_at = now;
    write_room_path(&path, &room)?;

    Ok(serde_json::json!({
        "status": "ok",
        "room_id": room.room_id,
        "latest_seq": room.latest_seq(),
        "message_count": room.messages.len(),
        "next_action": "Return to the caller immediately. Do not wait or poll; advisor replies will appear as new room messages."
    }))
}

fn read_room(args: &Value) -> Result<Value, String> {
    let room_id = required_room_id(args)?;
    let path = room_path(&room_id)?;
    let room =
        read_room_path(&path)?.ok_or_else(|| format!("advisor room '{room_id}' does not exist"))?;
    let after_seq = args.get("after_seq").and_then(Value::as_u64).unwrap_or(0);
    let limit = args
        .get("limit")
        .and_then(Value::as_u64)
        .unwrap_or(50)
        .clamp(1, 200) as usize;
    let messages: Vec<_> = room
        .messages
        .iter()
        .filter(|message| message.seq > after_seq)
        .take(limit)
        .cloned()
        .collect();

    Ok(serde_json::json!({
        "room": room_summary(&room),
        "messages": messages,
        "latest_seq": room.latest_seq(),
        "more": room.messages.iter().filter(|message| message.seq > after_seq).count() > limit,
    }))
}

fn attach_context(args: &Value) -> Result<Value, String> {
    let room_id = required_room_id(args)?;
    let path = room_path(&room_id)?;
    let mut room =
        read_room_path(&path)?.unwrap_or_else(|| AdvisorRoomFile::new(room_id.clone(), None, None));

    if let Some(docs) = string_array_arg(args, "docs") {
        for doc in docs {
            if !room.context.docs.contains(&doc) {
                room.context.docs.push(doc);
            }
        }
    }
    if let Some(tasks) = args.get("tasks").and_then(Value::as_array) {
        for task in tasks {
            if !room.context.tasks.contains(task) {
                room.context.tasks.push(task.clone());
            }
        }
    }
    room.updated_at = Utc::now();
    write_room_path(&path, &room)?;

    Ok(serde_json::json!({
        "status": "ok",
        "room": room_summary(&room),
        "context": room.context,
    }))
}

fn room_summary(room: &AdvisorRoomFile) -> AdvisorRoomSummary {
    AdvisorRoomSummary {
        room_id: room.room_id.clone(),
        title: room.title.clone(),
        turn_mode: room.turn_mode.clone(),
        participants: room.participants.clone(),
        message_count: room.messages.len(),
        latest_seq: room.latest_seq(),
        updated_at: room.updated_at,
    }
}

fn read_room_summaries() -> Result<Vec<AdvisorRoomSummary>, String> {
    let mut rooms = Vec::new();
    let dir = rooms_dir();
    let Ok(entries) = std::fs::read_dir(&dir) else {
        return Ok(rooms);
    };
    for entry in entries.flatten() {
        if entry.file_name().to_string_lossy().starts_with('.') {
            continue;
        }
        let path = entry.path();
        if path.extension().and_then(|ext| ext.to_str()) != Some("json") {
            continue;
        }
        if let Some(room) = read_room_path(&path)? {
            rooms.push(room_summary(&room));
        }
    }
    rooms.sort_by(|left, right| {
        right
            .updated_at
            .cmp(&left.updated_at)
            .then_with(|| left.room_id.cmp(&right.room_id))
    });
    Ok(rooms)
}

fn read_room_path(path: &Path) -> Result<Option<AdvisorRoomFile>, String> {
    if !path.exists() {
        return Ok(None);
    }
    let content = std::fs::read_to_string(path)
        .map_err(|err| format!("failed to read advisor room {}: {err}", path.display()))?;
    serde_json::from_str(&content)
        .map(Some)
        .map_err(|err| format!("failed to parse advisor room {}: {err}", path.display()))
}

fn write_room_path(path: &Path, room: &AdvisorRoomFile) -> Result<(), String> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).map_err(|err| {
            format!(
                "failed to create advisor room dir {}: {err}",
                parent.display()
            )
        })?;
    }
    let payload = serde_json::to_string_pretty(room)
        .map_err(|err| format!("failed to serialize advisor room: {err}"))?;
    let tmp = path.with_extension(format!("json.tmp-{}", std::process::id()));
    std::fs::write(&tmp, payload).map_err(|err| {
        format!(
            "failed to write advisor room temp file {}: {err}",
            tmp.display()
        )
    })?;
    std::fs::rename(&tmp, path)
        .map_err(|err| format!("failed to install advisor room {}: {err}", path.display()))
}

fn required_room_id(args: &Value) -> Result<String, String> {
    let raw = args
        .get("room_id")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .ok_or_else(|| "advisor action requires non-empty room_id".to_string())?;
    let sanitized = sanitize_room_id(raw);
    if sanitized != raw {
        return Err(format!(
            "advisor room_id '{raw}' contains unsupported characters; use '{sanitized}'"
        ));
    }
    Ok(sanitized)
}

fn sanitize_room_id(room_id: &str) -> String {
    room_id
        .chars()
        .map(|ch| {
            if ch.is_ascii_alphanumeric() || matches!(ch, '-' | '_' | '.') {
                ch
            } else {
                '_'
            }
        })
        .collect()
}

fn string_arg(args: &Value, key: &str) -> Option<String> {
    args.get(key)
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(str::to_string)
}

fn string_array_arg(args: &Value, key: &str) -> Option<Vec<String>> {
    let values = args.get(key)?.as_array()?;
    let out: Vec<String> = values
        .iter()
        .filter_map(Value::as_str)
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(str::to_string)
        .collect();
    Some(out)
}

fn room_path(room_id: &str) -> Result<PathBuf, String> {
    let file_name = format!("{}.json", sanitize_room_id(room_id));
    Ok(rooms_dir().join(file_name))
}

fn rooms_dir() -> PathBuf {
    brehon_root().join("runtime").join("advisors").join("rooms")
}

fn brehon_root() -> PathBuf {
    if let Ok(raw) = std::env::var("BREHON_ROOT") {
        let path = PathBuf::from(raw);
        if path.file_name().and_then(|name| name.to_str()) == Some(".brehon") {
            return path;
        }
        return path.join(".brehon");
    }
    std::env::current_dir()
        .unwrap_or_else(|_| PathBuf::from("."))
        .join(".brehon")
}

fn project_root_from_brehon_root() -> Option<PathBuf> {
    let root = brehon_root();
    if root.file_name().and_then(|name| name.to_str()) == Some(".brehon") {
        return root.parent().map(PathBuf::from);
    }
    Some(root)
}

fn load_project_config() -> Option<brehon_types::BrehonConfig> {
    let project_root = project_root_from_brehon_root()?;
    brehon_config::load_config(Some(&project_root)).ok()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::server::ContentBlock;
    use crate::tools::{Tool, TEST_ENV_LOCK};

    struct EnvGuard {
        previous: BTreeMap<String, Option<String>>,
    }

    impl EnvGuard {
        fn set(values: &[(&str, &str)]) -> Self {
            let previous = values
                .iter()
                .map(|(key, _)| ((*key).to_string(), std::env::var(key).ok()))
                .collect();
            for (key, value) in values {
                std::env::set_var(key, value);
            }
            Self { previous }
        }
    }

    impl Drop for EnvGuard {
        fn drop(&mut self) {
            for (key, value) in &self.previous {
                if let Some(value) = value {
                    std::env::set_var(key, value);
                } else {
                    std::env::remove_var(key);
                }
            }
        }
    }

    fn text_json(result: ToolResult) -> Value {
        match &result.content[0] {
            ContentBlock::Text { text } => serde_json::from_str(text).expect("json"),
            _ => panic!("expected text content"),
        }
    }

    #[tokio::test]
    async fn create_post_and_read_room() {
        let _lock = TEST_ENV_LOCK.lock().unwrap();
        let temp = tempfile::tempdir().expect("tempdir");
        let brehon_root = temp.path().join(".brehon");
        std::fs::create_dir_all(&brehon_root).expect("brehon root");
        let _env = EnvGuard::set(&[
            ("BREHON_ROOT", brehon_root.to_str().unwrap()),
            ("BREHON_AGENT_NAME", "advisor-1"),
            ("BREHON_AGENT_ROLE", "advisor"),
        ]);

        let tool = AdvisorTool::new();
        let create = text_json(
            tool.execute(serde_json::json!({
                "action": "create_room",
                "room_id": "release-war-room",
                "participants": ["kimi-advisor", "gpt53-advisor"]
            }))
            .await
            .unwrap(),
        );
        assert_eq!(create["status"], "ok");

        let post = text_json(
            tool.execute(serde_json::json!({
                "action": "post",
                "room_id": "release-war-room",
                "message": "What can still deadlock?"
            }))
            .await
            .unwrap(),
        );
        assert_eq!(post["latest_seq"], 1);

        let read = text_json(
            tool.execute(serde_json::json!({
                "action": "read",
                "room_id": "release-war-room"
            }))
            .await
            .unwrap(),
        );
        assert_eq!(read["latest_seq"], 1);
        assert_eq!(read["messages"][0]["author"], "advisor-1");
        assert_eq!(read["messages"][0]["role"], "advisor");
    }

    #[tokio::test]
    async fn attach_context_deduplicates_docs_and_tasks() {
        let _lock = TEST_ENV_LOCK.lock().unwrap();
        let temp = tempfile::tempdir().expect("tempdir");
        let brehon_root = temp.path().join(".brehon");
        std::fs::create_dir_all(&brehon_root).expect("brehon root");
        let _env = EnvGuard::set(&[("BREHON_ROOT", brehon_root.to_str().unwrap())]);

        let tool = AdvisorTool::new();
        let result = text_json(
            tool.execute(serde_json::json!({
                "action": "attach_context",
                "room_id": "release-war-room",
                "docs": ["docs/handoff.md", "docs/handoff.md"],
                "tasks": [{"status": "ready"}, {"status": "ready"}]
            }))
            .await
            .unwrap(),
        );

        assert_eq!(result["context"]["docs"].as_array().unwrap().len(), 1);
        assert_eq!(result["context"]["tasks"].as_array().unwrap().len(), 1);
    }
}
