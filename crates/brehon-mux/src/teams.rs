//! Native Agent Teams integration for Claude Code.
//!
//! Manages Claude Code's native Agent Teams file structure:
//! - `~/.claude/teams/{team-name}/config.json` — team member registry
//! - `~/.claude/teams/{team-name}/inboxes/{agent-name}.json` — per-agent inbox files
//!
//! This provides structured prompt delivery through Claude Code's native
//! inbox polling mechanism, replacing PTY text injection for Claude agents.

use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::fmt;
#[cfg(test)]
use std::io;
use std::path::{Path, PathBuf};
#[cfg(test)]
use std::sync::{Arc, Mutex};

use crate::error::{Error, Result};
use crate::pty::TeamsSpawnConfig;

/// Colors assigned to team members (matches Claude Code's palette).
const AGENT_COLORS: &[InboxMessageColor] = &[
    InboxMessageColor::Green,
    InboxMessageColor::Blue,
    InboxMessageColor::Yellow,
    InboxMessageColor::Cyan,
    InboxMessageColor::Magenta,
    InboxMessageColor::Red,
    InboxMessageColor::White,
];

/// Normal Teams member used as the sender identity for Brehon-generated prompts.
///
/// Claude Code treats some Teams member types as semantic roles. Keep automated
/// runtime prompts on an ordinary teammate identity so Anthropic-compatible
/// provider shims do not receive mid-conversation `system` messages.
pub const AUTOMATION_AGENT_NAME: &str = "brehon";
/// Legacy director sender name. Older runtime state may still contain unread
/// messages from this identity, so cleanup still treats it as Brehon-owned.
pub const DIRECTOR_AGENT_NAME: &str = "director";
/// Default supervisor name when callers do not override it.
pub const SUPERVISOR_TEAM_MEMBER_NAME: &str = "supervisor";

fn is_brehon_runtime_sender(name: &str) -> bool {
    matches!(name, AUTOMATION_AGENT_NAME | DIRECTOR_AGENT_NAME)
}

fn normalized_member_alias(name: &str) -> Option<&'static str> {
    match name {
        "claude" => Some(SUPERVISOR_TEAM_MEMBER_NAME),
        _ => None,
    }
}

/// Display color names accepted by Claude Code inbox/team JSON.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum InboxMessageColor {
    Green,
    Blue,
    Yellow,
    Cyan,
    Magenta,
    Red,
    White,
}

impl InboxMessageColor {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Green => "green",
            Self::Blue => "blue",
            Self::Yellow => "yellow",
            Self::Cyan => "cyan",
            Self::Magenta => "magenta",
            Self::Red => "red",
            Self::White => "white",
        }
    }
}

impl fmt::Display for InboxMessageColor {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// A single message in a Teams inbox file.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct InboxMessage {
    /// Sender agent name.
    pub from: String,
    /// Full message text.
    pub text: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    /// Optional short summary of the message.
    pub summary: Option<String>,
    /// ISO 8601 timestamp of when the message was sent.
    pub timestamp: String,
    /// Display color for this sender.
    pub color: String,
    /// Whether the recipient has read this message.
    pub read: bool,
}

/// Team member entry in config.json.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TeamMember {
    /// Unique agent ID in `name@team` format.
    pub agent_id: String,
    /// Display name for this member.
    pub name: String,
    /// Role type (e.g., "team-lead", "general-purpose").
    pub agent_type: String,
    /// Model identifier override.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
    /// System prompt override.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub prompt: Option<String>,
    /// Display color name.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub color: Option<String>,
    /// Whether plan mode is required for this member.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub plan_mode_required: Option<bool>,
    /// Unix timestamp (ms) when this member joined.
    pub joined_at: u64,
    /// Tmux pane identifier (or "tmux" placeholder).
    pub tmux_pane_id: String,
    /// Working directory for this member.
    pub cwd: String,
    /// Event subscription topics.
    #[serde(default)]
    pub subscriptions: Vec<String>,
    /// Backend type identifier (e.g., "tmux").
    #[serde(skip_serializing_if = "Option::is_none")]
    pub backend_type: Option<String>,
}

/// Team config.json structure.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TeamConfig {
    /// Team name (typically the factory session name).
    pub name: String,
    /// Human-readable description of this team.
    pub description: String,
    /// Unix timestamp (ms) when the team was created.
    pub created_at: u64,
    /// Agent ID of the team lead.
    pub lead_agent_id: String,
    /// Session ID of the team lead.
    pub lead_session_id: String,
    /// All registered team members.
    pub members: Vec<TeamMember>,
}

#[derive(Clone, Debug)]
pub(crate) struct TeamsPaths {
    team_dir: PathBuf,
}

impl TeamsPaths {
    /// Create Teams paths for the given factory session using the process home directory.
    pub(crate) fn for_session(session_name: &str) -> Self {
        let home = dirs::home_dir().unwrap_or_else(|| PathBuf::from("."));
        Self::for_session_with_home(session_name, home)
    }

    /// Create Teams paths for the given factory session rooted under a custom home directory.
    pub(crate) fn for_session_with_home(session_name: &str, home: PathBuf) -> Self {
        Self {
            team_dir: home.join(".claude").join("teams").join(session_name),
        }
    }

    /// Path to the team directory for this session.
    pub(crate) fn team_dir(&self) -> &Path {
        &self.team_dir
    }

    /// Path to the inboxes directory for this session.
    pub(crate) fn inboxes_dir(&self) -> PathBuf {
        self.team_dir.join("inboxes")
    }

    /// Path to the team config file for this session.
    pub(crate) fn config(&self) -> PathBuf {
        self.team_dir.join("config.json")
    }

    /// Path to a specific member inbox file for this session.
    pub(crate) fn inbox_for(&self, member_name: &str) -> anyhow::Result<PathBuf> {
        anyhow::ensure!(
            !member_name.is_empty() && !member_name.contains(['/', '\\', '\0']),
            "member name must be a simple file stem, got: {member_name}"
        );
        Ok(self.inboxes_dir().join(format!("{member_name}.json")))
    }
}

/// Manages the native Agent Teams file structure for a factory session.
#[derive(Clone)]
pub struct TeamsManager {
    team_name: String,
    paths: TeamsPaths,
}

impl TeamsManager {
    /// Create a new TeamsManager for the given factory session.
    ///
    /// Files are stored at `~/.claude/teams/{team-name}/`.
    pub fn new(session_name: &str) -> Self {
        Self::new_with_paths(session_name, TeamsPaths::for_session(session_name))
    }

    fn new_with_paths(session_name: &str, paths: TeamsPaths) -> Self {
        Self {
            team_name: session_name.to_string(),
            paths,
        }
    }

    #[cfg(test)]
    pub(crate) fn new_for_test(session_name: &str, home: PathBuf) -> Self {
        Self::new_with_paths(
            session_name,
            TeamsPaths::for_session_with_home(session_name, home),
        )
    }

    /// Get the team name for this session.
    pub fn team_name(&self) -> &str {
        &self.team_name
    }

    /// Format an agent ID: `{name}@{team-name}`.
    pub fn agent_id_for(&self, name: &str) -> String {
        format!("{}@{}", name, self.team_name)
    }

    /// Assign a color to an agent based on its index.
    pub fn color_for_index(index: usize) -> InboxMessageColor {
        AGENT_COLORS[index % AGENT_COLORS.len()]
    }

    /// Build a `TeamsSpawnConfig` for spawning an agent with native teams CLI flags.
    pub fn spawn_config_for(
        &self,
        name: &str,
        display_name: Option<&str>,
        agent_type: &str,
        color: InboxMessageColor,
        parent_session_id: Option<&str>,
    ) -> TeamsSpawnConfig {
        TeamsSpawnConfig {
            team_name: self.team_name.clone(),
            agent_id: self.agent_id_for(name),
            agent_name: display_name.unwrap_or(name).to_string(),
            agent_color: color.to_string(),
            agent_type: agent_type.to_string(),
            parent_session_id: parent_session_id.map(|s| s.to_string()),
        }
    }

    /// Build teams_configs HashMap for MuxConfig before agents are spawned.
    ///
    /// Returns `(configs_map, lead_session_id)`.
    pub fn build_configs_for_mux(
        session_name: &str,
        supervisor_name: &str,
        worker_names: &[String],
    ) -> (HashMap<String, TeamsSpawnConfig>, String) {
        let mut configs = HashMap::new();
        let lead_session_id = uuid::Uuid::new_v4().to_string();

        // Supervisor
        configs.insert(
            supervisor_name.to_string(),
            TeamsSpawnConfig {
                team_name: session_name.to_string(),
                agent_id: format!("{SUPERVISOR_TEAM_MEMBER_NAME}@{session_name}"),
                agent_name: SUPERVISOR_TEAM_MEMBER_NAME.to_string(),
                agent_color: InboxMessageColor::Green.to_string(),
                agent_type: "team-lead".to_string(),
                parent_session_id: None,
            },
        );

        // Workers
        for (i, name) in worker_names.iter().enumerate() {
            configs.insert(
                name.clone(),
                TeamsSpawnConfig {
                    team_name: session_name.to_string(),
                    agent_id: format!("{name}@{session_name}"),
                    agent_name: name.clone(),
                    agent_color: Self::color_for_index(i).to_string(),
                    agent_type: "general-purpose".to_string(),
                    parent_session_id: Some(lead_session_id.clone()),
                },
            );
        }

        (configs, lead_session_id)
    }

    /// Initialize the team directory and write config.json.
    pub fn init_team_config(
        &self,
        supervisor_name: &str,
        member_names: &[String],
        project_cwd: &std::path::Path,
        member_cwds: &HashMap<String, PathBuf>,
        lead_session_id: &str,
    ) -> anyhow::Result<()> {
        std::fs::create_dir_all(self.paths.inboxes_dir())?;

        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_millis() as u64;

        let project_cwd_str = project_cwd.to_string_lossy().to_string();
        let member_cwd = |name: &str| {
            member_cwds
                .get(name)
                .map(|p| p.to_string_lossy().to_string())
                .unwrap_or_else(|| project_cwd_str.clone())
        };

        // Supervisor
        let mut members = vec![TeamMember {
            agent_id: self.agent_id_for(supervisor_name),
            name: supervisor_name.to_string(),
            agent_type: "team-lead".to_string(),
            model: None,
            prompt: None,
            color: Some(InboxMessageColor::Green.to_string()),
            plan_mode_required: None,
            joined_at: now,
            tmux_pane_id: "tmux".to_string(),
            cwd: member_cwd(supervisor_name),
            subscriptions: Vec::new(),
            backend_type: Some("tmux".to_string()),
        }];

        // Brehon automation identity for daemon/runtime prompts.
        members.push(TeamMember {
            agent_id: self.agent_id_for(AUTOMATION_AGENT_NAME),
            name: AUTOMATION_AGENT_NAME.to_string(),
            agent_type: "general-purpose".to_string(),
            model: None,
            prompt: None,
            color: Some(InboxMessageColor::White.to_string()),
            plan_mode_required: None,
            joined_at: now,
            tmux_pane_id: "tmux".to_string(),
            cwd: project_cwd_str.clone(),
            subscriptions: Vec::new(),
            backend_type: Some("tmux".to_string()),
        });

        // Non-director members (workers, reviewers, non-Claude roster entries).
        for (i, member_name) in member_names.iter().enumerate() {
            members.push(TeamMember {
                agent_id: self.agent_id_for(member_name),
                name: member_name.clone(),
                agent_type: "general-purpose".to_string(),
                model: None,
                prompt: None,
                color: Some(Self::color_for_index(i).to_string()),
                plan_mode_required: Some(false),
                joined_at: now,
                tmux_pane_id: "tmux".to_string(),
                cwd: member_cwd(member_name),
                subscriptions: Vec::new(),
                backend_type: Some("tmux".to_string()),
            });
        }

        let config = TeamConfig {
            name: self.team_name.clone(),
            description: format!("Brehon factory session {}", self.team_name),
            created_at: now,
            lead_agent_id: self.agent_id_for(supervisor_name),
            lead_session_id: lead_session_id.to_string(),
            members,
        };

        let config_path = self.paths.config();
        std::fs::write(&config_path, serde_json::to_string_pretty(&config)?)?;

        // Create empty inbox files
        self.ensure_inbox(supervisor_name)?;
        self.ensure_inbox(AUTOMATION_AGENT_NAME)?;
        for name in member_names {
            self.ensure_inbox(name)?;
        }

        tracing::info!(
            path = ?config_path,
            members = 2 + member_names.len(),
            "Initialized Teams config"
        );

        Ok(())
    }

    /// Write a message to a target agent's inbox file.
    pub fn write_to_inbox(
        &self,
        target: &str,
        from: &str,
        message: &str,
        summary: Option<&str>,
    ) -> Result<()> {
        let resolved_target = self.resolve_member_name(target);
        let resolved_from = self.resolve_member_name(from);
        let resolved_summary = summary.unwrap_or(message).to_string();
        let summary_chars = resolved_summary.chars().count();
        let inbox_path = self
            .paths
            .inbox_for(&resolved_target)
            .map_err(|e| Error::terminal(e.to_string()))?;

        if let Some(parent) = inbox_path.parent() {
            std::fs::create_dir_all(parent)?;
        }

        if !inbox_path.exists() {
            std::fs::write(&inbox_path, "[]")?;
        }

        let content = std::fs::read_to_string(&inbox_path)?;
        let mut messages: Vec<InboxMessage> = match serde_json::from_str(&content) {
            Ok(msgs) => msgs,
            Err(parse_err) => {
                let timestamp = corrupt_inbox_timestamp();
                let corrupt_path = inbox_path
                    .with_file_name(format!("{}.json.corrupt-{}", resolved_target, timestamp));
                std::fs::rename(&inbox_path, &corrupt_path)?;
                tracing::info!(
                    team = %self.team_name,
                    agent = %resolved_target,
                    from = %resolved_from,
                    summary_chars,
                    file_size_after = content.len(),
                    error = %parse_err,
                    "Failed to append message to Teams inbox because inbox JSON was corrupt"
                );
                return Err(Error::corrupt_inbox(
                    &corrupt_path,
                    format!("Inbox JSON parse error: {parse_err}"),
                ));
            }
        };

        let now = chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Millis, true);

        messages.push(InboxMessage {
            from: resolved_from.clone(),
            text: message.to_string(),
            summary: Some(resolved_summary),
            timestamp: now,
            color: InboxMessageColor::Green.to_string(),
            read: false,
        });

        let serialized = serde_json::to_string_pretty(&messages)?;
        let file_size_after = serialized.len();
        std::fs::write(&inbox_path, serialized)?;

        tracing::info!(
            team = %self.team_name,
            agent = %resolved_target,
            from = %resolved_from,
            summary_chars,
            file_size_after,
            "Wrote message to Teams inbox"
        );
        Ok(())
    }

    /// Clean up the team directory on shutdown.
    ///
    /// If any inbox contains an unread message from a sender other than the
    /// director, the directory is archived instead of deleted.
    pub fn cleanup(&self) {
        if !self.paths.team_dir().exists() {
            return;
        }

        let has_unread = self.has_unread_non_runtime_messages();

        if has_unread {
            let timestamp = chrono::Utc::now()
                .to_rfc3339_opts(chrono::SecondsFormat::Millis, true)
                .replace(':', "-");
            let team_name = &self.team_name;
            let archive_name = format!("{team_name}-{timestamp}");
            let archive_dir = self
                .paths
                .team_dir()
                .parent()
                .map(|p| p.join(".archived").join(&archive_name))
                .unwrap_or_else(|| PathBuf::from(&archive_name));

            if let Err(e) = std::fs::create_dir_all(&archive_dir) {
                tracing::warn!(path = ?archive_dir, "Failed to create archive dir: {}", e);
                return;
            }

            // Move team dir contents into the archive dir
            let move_result = (|| -> std::io::Result<()> {
                for entry in std::fs::read_dir(self.paths.team_dir())? {
                    let entry = entry?;
                    let dest = archive_dir.join(entry.file_name());
                    std::fs::rename(entry.path(), dest)?;
                }
                std::fs::remove_dir(self.paths.team_dir())
            })();

            if let Err(e) = move_result {
                tracing::warn!(path = ?self.paths.team_dir(), archive = ?archive_dir, "Failed to archive teams dir: {}", e);
            } else {
                tracing::info!(archive = ?archive_dir, "Archived non-empty team directory");
            }
        } else if let Err(e) = std::fs::remove_dir_all(self.paths.team_dir()) {
            tracing::warn!(path = ?self.paths.team_dir(), "Failed to clean up teams dir: {}", e);
        }
    }

    fn has_unread_non_runtime_messages(&self) -> bool {
        let inboxes_dir = self.paths.inboxes_dir();
        let Ok(entries) = std::fs::read_dir(&inboxes_dir) else {
            return false;
        };

        for entry in entries.flatten() {
            let path = entry.path();
            let Some(name) = path.file_name().and_then(|n| n.to_str()) else {
                continue;
            };
            if name.contains(".corrupt-") {
                return true;
            }
            if !name.ends_with(".json") {
                continue;
            }
            let Ok(content) = std::fs::read_to_string(&path) else {
                continue;
            };
            let Ok(messages): std::result::Result<Vec<InboxMessage>, _> =
                serde_json::from_str(&content)
            else {
                continue;
            };
            for msg in &messages {
                if !msg.read && !is_brehon_runtime_sender(&msg.from) {
                    return true;
                }
            }
        }

        false
    }

    fn ensure_inbox(&self, name: &str) -> anyhow::Result<()> {
        let inbox_path = self.paths.inbox_for(name)?;
        if let Some(parent) = inbox_path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        if !inbox_path.exists() {
            std::fs::write(&inbox_path, "[]")?;
        }
        Ok(())
    }

    fn resolve_member_name(&self, name: &str) -> String {
        if let Ok(direct_inbox) = self.paths.inbox_for(name)
            && direct_inbox.exists()
        {
            return name.to_string();
        }

        if let Some(alias) = normalized_member_alias(name)
            && let Ok(alias_inbox) = self.paths.inbox_for(alias)
            && alias_inbox.exists()
        {
            return alias.to_string();
        }

        let agent_id = self.agent_id_for(name);
        self.load_config()
            .ok()
            .and_then(|config| {
                config
                    .members
                    .into_iter()
                    .find(|m| m.agent_id == agent_id)
                    .map(|m| m.name)
            })
            .or_else(|| normalized_member_alias(name).map(str::to_string))
            .unwrap_or_else(|| name.to_string())
    }

    fn load_config(&self) -> anyhow::Result<TeamConfig> {
        let config_path = self.paths.config();
        let json = std::fs::read_to_string(config_path)?;
        Ok(serde_json::from_str(&json)?)
    }
}

fn corrupt_inbox_timestamp() -> String {
    chrono::Utc::now()
        .to_rfc3339_opts(chrono::SecondsFormat::Millis, true)
        .replace(':', "-")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[derive(Clone)]
    struct InfoLogCapture(Arc<Mutex<Vec<u8>>>);

    struct InfoLogCaptureWriter(Arc<Mutex<Vec<u8>>>);

    impl<'a> tracing_subscriber::fmt::MakeWriter<'a> for InfoLogCapture {
        type Writer = InfoLogCaptureWriter;

        fn make_writer(&'a self) -> Self::Writer {
            InfoLogCaptureWriter(self.0.clone())
        }
    }

    impl io::Write for InfoLogCaptureWriter {
        fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
            self.0
                .lock()
                .expect("log capture mutex poisoned")
                .extend_from_slice(buf);
            Ok(buf.len())
        }

        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }

    fn capture_info_logs(run: impl FnOnce()) -> String {
        let captured = Arc::new(Mutex::new(Vec::new()));
        let subscriber = tracing_subscriber::fmt()
            .without_time()
            .with_target(false)
            .with_ansi(false)
            .with_writer(InfoLogCapture(captured.clone()))
            .with_max_level(tracing::Level::INFO)
            .finish();

        tracing::subscriber::with_default(subscriber, run);

        String::from_utf8(captured.lock().expect("log capture mutex poisoned").clone())
            .expect("captured logs should be utf-8")
    }

    fn assert_inbox_write_log_fields(
        logs: &str,
        expected_message: &str,
        summary_chars: usize,
        include_error: bool,
    ) {
        let event_count = logs
            .lines()
            .filter(|line| line.contains(expected_message))
            .count();
        assert_eq!(
            event_count, 1,
            "expected exactly one '{expected_message}' event, got logs: {logs}"
        );
        assert!(
            logs.contains("team=test-session"),
            "missing team field in logs: {logs}"
        );
        assert!(
            logs.contains("agent=worker-1"),
            "missing agent field in logs: {logs}"
        );
        assert!(
            logs.contains("from=brehon"),
            "missing from field in logs: {logs}"
        );
        assert!(
            logs.contains(&format!("summary_chars={summary_chars}")),
            "missing summary_chars field in logs: {logs}"
        );
        assert!(
            logs.contains("file_size_after="),
            "missing file_size_after field in logs: {logs}"
        );
        assert_eq!(
            logs.contains("error="),
            include_error,
            "unexpected error field presence in logs: {logs}"
        );
    }

    fn test_manager(dir: &std::path::Path) -> TeamsManager {
        TeamsManager::new_with_paths(
            "test-session",
            TeamsPaths {
                team_dir: dir.join("teams").join("test-session"),
            },
        )
    }

    #[test]
    fn cleanup_archives_non_empty_team_dir() {
        let dir = std::env::temp_dir().join(format!("brehon-teams-test-{}", uuid::Uuid::new_v4()));
        let manager = test_manager(&dir);

        manager
            .init_team_config(
                SUPERVISOR_TEAM_MEMBER_NAME,
                &["worker-1".to_string()],
                dir.as_path(),
                &HashMap::new(),
                "lead-session",
            )
            .unwrap();

        // Write an unread message from a non-director sender.
        manager
            .write_to_inbox("worker-1", "worker-2", "hello worker", None)
            .unwrap();

        assert!(manager.paths.team_dir().exists());

        manager.cleanup();

        // Original dir should be gone.
        assert!(!manager.paths.team_dir().exists());

        // Archive should exist.
        let archive_parent = dir.join("teams").join(".archived");
        assert!(archive_parent.exists());

        let entries: Vec<_> = std::fs::read_dir(&archive_parent).unwrap().collect();
        assert_eq!(entries.len(), 1);

        let archive_dir = entries[0].as_ref().unwrap().path();
        assert!(archive_dir.exists());
        assert!(archive_dir.join("inboxes").join("worker-1.json").exists());

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn init_and_write_to_inbox() {
        let dir = std::env::temp_dir().join(format!("brehon-teams-test-{}", uuid::Uuid::new_v4()));
        let manager = test_manager(&dir);

        manager
            .init_team_config(
                SUPERVISOR_TEAM_MEMBER_NAME,
                &["worker-1".to_string()],
                dir.as_path(),
                &HashMap::new(),
                "lead-session",
            )
            .unwrap();

        assert!(manager.paths.config().exists());
        assert!(manager.paths.inbox_for("supervisor").unwrap().exists());
        assert!(
            manager
                .paths
                .inbox_for(AUTOMATION_AGENT_NAME)
                .unwrap()
                .exists()
        );
        assert!(manager.paths.inbox_for("worker-1").unwrap().exists());

        manager
            .write_to_inbox("worker-1", AUTOMATION_AGENT_NAME, "hello worker", None)
            .unwrap();

        let messages: Vec<InboxMessage> = serde_json::from_str(
            &std::fs::read_to_string(manager.paths.inbox_for("worker-1").unwrap()).unwrap(),
        )
        .unwrap();
        assert_eq!(messages.len(), 1);
        assert_eq!(messages[0].text, "hello worker");
        assert_eq!(messages[0].from, AUTOMATION_AGENT_NAME);

        let config = manager.load_config().unwrap();
        let automation = config
            .members
            .iter()
            .find(|member| member.name == AUTOMATION_AGENT_NAME)
            .expect("automation team member");
        assert_eq!(automation.agent_type, "general-purpose");
        assert!(
            config
                .members
                .iter()
                .all(|member| member.agent_type != "director"),
            "Teams config must not register a semantic director member"
        );

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn inbox_message_color_serializes_to_expected_lowercase_strings() {
        let cases = [
            (InboxMessageColor::Green, "green"),
            (InboxMessageColor::Blue, "blue"),
            (InboxMessageColor::Yellow, "yellow"),
            (InboxMessageColor::Cyan, "cyan"),
            (InboxMessageColor::Magenta, "magenta"),
            (InboxMessageColor::Red, "red"),
            (InboxMessageColor::White, "white"),
        ];

        for (color, expected) in cases {
            assert_eq!(color.as_str(), expected);
            assert_eq!(color.to_string(), expected);
        }
    }

    #[test]
    fn teams_json_round_trip_preserves_serialized_shape() {
        let dir = std::env::temp_dir().join(format!("brehon-teams-test-{}", uuid::Uuid::new_v4()));
        let manager = test_manager(&dir);

        manager
            .init_team_config(
                SUPERVISOR_TEAM_MEMBER_NAME,
                &["worker-1".to_string()],
                dir.as_path(),
                &HashMap::new(),
                "lead-session",
            )
            .unwrap();

        let config_json = std::fs::read_to_string(manager.paths.config()).unwrap();
        let config: TeamConfig = serde_json::from_str(&config_json).unwrap();
        assert_eq!(config_json, serde_json::to_string_pretty(&config).unwrap());

        manager
            .write_to_inbox("worker-1", AUTOMATION_AGENT_NAME, "hello worker", None)
            .unwrap();

        let inbox_json =
            std::fs::read_to_string(manager.paths.inbox_for("worker-1").unwrap()).unwrap();
        let inbox_messages: Vec<InboxMessage> = serde_json::from_str(&inbox_json).unwrap();
        assert_eq!(
            inbox_json,
            serde_json::to_string_pretty(&inbox_messages).unwrap()
        );

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn claude_supervisor_alias_resolves_to_supervisor_inbox() {
        let dir = std::env::temp_dir().join(format!("brehon-teams-test-{}", uuid::Uuid::new_v4()));
        let manager = test_manager(&dir);

        manager
            .init_team_config(
                "claude-code",
                &[],
                dir.as_path(),
                &HashMap::new(),
                "lead-session",
            )
            .unwrap();

        manager
            .write_to_inbox(
                "claude-code",
                AUTOMATION_AGENT_NAME,
                "hello supervisor",
                None,
            )
            .unwrap();

        let messages: Vec<InboxMessage> = serde_json::from_str(
            &std::fs::read_to_string(manager.paths.inbox_for("claude-code").unwrap()).unwrap(),
        )
        .unwrap();
        assert_eq!(messages.len(), 1);
        assert_eq!(messages[0].text, "hello supervisor");

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn init_team_config_uses_scoped_cwds_for_supervisor_and_reviewer() {
        let dir = std::env::temp_dir().join(format!("brehon-teams-test-{}", uuid::Uuid::new_v4()));
        let manager = test_manager(&dir);
        let project_root = dir.join("project");
        let supervisor_cwd = project_root.join(".brehon/worktrees/supervisor/claude-code");
        let reviewer_cwd = project_root.join(".brehon/worktrees/reviewer/reviewer-1");
        let mut member_cwds = HashMap::new();
        member_cwds.insert("claude-code".to_string(), supervisor_cwd.clone());
        member_cwds.insert("reviewer-1".to_string(), reviewer_cwd.clone());

        manager
            .init_team_config(
                "claude-code",
                &["reviewer-1".to_string()],
                project_root.as_path(),
                &member_cwds,
                "lead-session",
            )
            .unwrap();

        let config = manager.load_config().unwrap();
        let supervisor = config
            .members
            .iter()
            .find(|member| member.name == "claude-code")
            .expect("supervisor entry");
        let reviewer = config
            .members
            .iter()
            .find(|member| member.name == "reviewer-1")
            .expect("reviewer entry");

        assert_eq!(supervisor.cwd, supervisor_cwd.to_string_lossy());
        assert_eq!(reviewer.cwd, reviewer_cwd.to_string_lossy());

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn inbox_for_rejects_path_traversal() {
        let dir = std::env::temp_dir().join(format!("brehon-teams-test-{}", uuid::Uuid::new_v4()));
        let paths = TeamsPaths {
            team_dir: dir.join("teams").join("test-session"),
        };

        assert!(paths.inbox_for("").is_err());
        assert!(paths.inbox_for("../x").is_err());
        assert!(paths.inbox_for("foo/bar").is_err());
        assert!(paths.inbox_for("a\\b").is_err());
        assert!(paths.inbox_for("a\0b").is_err());
        assert!(paths.inbox_for("valid-name").is_ok());
        assert!(
            paths
                .inbox_for("valid-name")
                .unwrap()
                .ends_with("valid-name.json")
        );

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn write_to_inbox_rejects_corrupt_json_and_renames_file() {
        let dir = std::env::temp_dir().join(format!("brehon-teams-test-{}", uuid::Uuid::new_v4()));
        let manager = test_manager(&dir);

        manager
            .init_team_config(
                SUPERVISOR_TEAM_MEMBER_NAME,
                &["worker-1".to_string()],
                dir.as_path(),
                &HashMap::new(),
                "lead-session",
            )
            .unwrap();

        let inbox_path = manager.paths.inbox_for("worker-1").unwrap();
        let garbage = "this is not json {{[";
        std::fs::write(&inbox_path, garbage).unwrap();

        let result =
            manager.write_to_inbox("worker-1", AUTOMATION_AGENT_NAME, "hello worker", None);
        let corrupt_path = match result {
            Err(Error::CorruptInbox { path, reason }) => {
                assert!(
                    reason.contains("Inbox JSON parse error"),
                    "expected parse error reason, got: {reason}"
                );
                path
            }
            Err(other) => panic!("expected CorruptInbox error, got: {other:?}"),
            Ok(_) => panic!("write_to_inbox should return Err for corrupt inbox JSON"),
        };

        assert!(
            !inbox_path.exists(),
            "original corrupt inbox should be renamed"
        );
        let corrupt_name = corrupt_path
            .file_name()
            .and_then(|name| name.to_str())
            .expect("corrupt file name should be valid UTF-8");
        assert!(
            corrupt_name.starts_with("worker-1.json.corrupt-"),
            "unexpected corrupt file name: {corrupt_name}"
        );
        assert!(
            !corrupt_name.contains(':'),
            "corrupt file name should avoid Windows-invalid colons: {corrupt_name}"
        );
        assert_eq!(
            std::fs::read_to_string(&corrupt_path).unwrap(),
            garbage,
            "corrupt backup should contain original garbage"
        );

        let inboxes_dir = manager.paths.inboxes_dir();
        let mut found_corrupt = false;
        for entry in std::fs::read_dir(&inboxes_dir).unwrap() {
            let entry = entry.unwrap();
            let name = entry.file_name().to_string_lossy().to_string();
            if entry.path() == corrupt_path {
                let contents = std::fs::read_to_string(entry.path()).unwrap();
                assert_eq!(
                    contents, garbage,
                    "corrupt backup should contain original garbage"
                );
                found_corrupt = true;
            } else {
                assert_ne!(
                    name, "worker-1.json",
                    "original inbox path should not remain beside corrupt backup"
                );
            }
        }
        assert!(found_corrupt, "expected a worker-1.json.corrupt-* sibling");

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn write_to_inbox_emits_structured_info_logs_for_success_and_corrupt_inbox() {
        let _lock = brehon_test_harness::TEST_ENV_LOCK
            .lock()
            .unwrap_or_else(|err| err.into_inner());
        let dir = std::env::temp_dir().join(format!("brehon-teams-test-{}", uuid::Uuid::new_v4()));
        let manager = test_manager(&dir);
        let summary = "hello summary";

        manager
            .init_team_config(
                SUPERVISOR_TEAM_MEMBER_NAME,
                &["worker-1".to_string()],
                dir.as_path(),
                &HashMap::new(),
                "lead-session",
            )
            .unwrap();

        let success_logs = capture_info_logs(|| {
            manager
                .write_to_inbox(
                    "worker-1",
                    AUTOMATION_AGENT_NAME,
                    "hello worker",
                    Some(summary),
                )
                .unwrap();
        });
        assert_inbox_write_log_fields(
            &success_logs,
            "Wrote message to Teams inbox",
            summary.chars().count(),
            false,
        );

        let inbox_path = manager.paths.inbox_for("worker-1").unwrap();
        std::fs::write(&inbox_path, "this is not json {{[").unwrap();

        let failure_logs = capture_info_logs(|| {
            let result = manager.write_to_inbox(
                "worker-1",
                AUTOMATION_AGENT_NAME,
                "hello worker",
                Some(summary),
            );
            assert!(matches!(result, Err(Error::CorruptInbox { .. })));
        });
        assert_inbox_write_log_fields(
            &failure_logs,
            "Failed to append message to Teams inbox because inbox JSON was corrupt",
            summary.chars().count(),
            true,
        );

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn cleanup_archives_team_dir_with_unread_non_director_message() {
        let dir = std::env::temp_dir().join(format!("brehon-teams-test-{}", uuid::Uuid::new_v4()));
        let manager = test_manager(&dir);

        manager
            .init_team_config(
                SUPERVISOR_TEAM_MEMBER_NAME,
                &["worker-1".to_string()],
                dir.as_path(),
                &HashMap::new(),
                "lead-session",
            )
            .unwrap();

        manager
            .write_to_inbox("worker-1", "supervisor", "unread task", None)
            .unwrap();

        let team_dir = manager.paths.team_dir().to_path_buf();
        assert!(team_dir.exists(), "team dir should exist before cleanup");

        manager.cleanup();

        assert!(
            !team_dir.exists(),
            "original team dir should be removed after cleanup"
        );

        let archived_parent = dir.join("teams").join(".archived");
        assert!(archived_parent.exists(), "archive parent dir should exist");

        let mut found_archive = false;
        for entry in std::fs::read_dir(&archived_parent).unwrap() {
            let entry = entry.unwrap();
            let name = entry.file_name().to_string_lossy().to_string();
            if name.starts_with("test-session-") {
                found_archive = true;
                let archived_inbox = entry.path().join("inboxes").join("worker-1.json");
                assert!(archived_inbox.exists(), "archived inbox should exist");
                let messages: Vec<InboxMessage> =
                    serde_json::from_str(&std::fs::read_to_string(&archived_inbox).unwrap())
                        .unwrap();
                assert_eq!(messages.len(), 1);
                assert_eq!(messages[0].text, "unread task");
                assert!(!messages[0].read);
                break;
            }
        }
        assert!(found_archive, "expected an archived team directory");

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn cleanup_deletes_team_dir_when_all_messages_are_read() {
        let dir = std::env::temp_dir().join(format!("brehon-teams-test-{}", uuid::Uuid::new_v4()));
        let manager = test_manager(&dir);

        manager
            .init_team_config(
                SUPERVISOR_TEAM_MEMBER_NAME,
                &["worker-1".to_string()],
                dir.as_path(),
                &HashMap::new(),
                "lead-session",
            )
            .unwrap();

        manager
            .write_to_inbox("worker-1", "supervisor", "read task", None)
            .unwrap();
        let inbox_path = manager.paths.inbox_for("worker-1").unwrap();
        let messages: Vec<InboxMessage> =
            serde_json::from_str(&std::fs::read_to_string(&inbox_path).unwrap()).unwrap();
        let read_messages: Vec<InboxMessage> = messages
            .into_iter()
            .map(|mut m| {
                m.read = true;
                m
            })
            .collect();
        std::fs::write(
            &inbox_path,
            serde_json::to_string_pretty(&read_messages).unwrap(),
        )
        .unwrap();

        let team_dir = manager.paths.team_dir().to_path_buf();
        assert!(team_dir.exists(), "team dir should exist before cleanup");

        manager.cleanup();

        assert!(
            !team_dir.exists(),
            "team dir should be deleted when all messages are read"
        );
        let archived_parent = dir.join("teams").join(".archived");
        assert!(
            !archived_parent.exists(),
            "archive dir should not exist when everything is read"
        );

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn cleanup_archives_team_dir_when_corrupt_backup_exists() {
        let dir = std::env::temp_dir().join(format!("brehon-teams-test-{}", uuid::Uuid::new_v4()));
        let manager = test_manager(&dir);

        manager
            .init_team_config(
                SUPERVISOR_TEAM_MEMBER_NAME,
                &["worker-1".to_string()],
                dir.as_path(),
                &HashMap::new(),
                "lead-session",
            )
            .unwrap();

        let corrupt_path = manager
            .paths
            .inboxes_dir()
            .join("worker-1.json.corrupt-2024-01-01");
        std::fs::write(&corrupt_path, "garbage").unwrap();

        let team_dir = manager.paths.team_dir().to_path_buf();
        assert!(team_dir.exists(), "team dir should exist before cleanup");

        manager.cleanup();

        assert!(
            !team_dir.exists(),
            "original team dir should be removed after cleanup"
        );

        let archived_parent = dir.join("teams").join(".archived");
        assert!(archived_parent.exists(), "archive parent dir should exist");

        let mut found_archive = false;
        for entry in std::fs::read_dir(&archived_parent).unwrap() {
            let entry = entry.unwrap();
            let name = entry.file_name().to_string_lossy().to_string();
            if name.starts_with("test-session-") {
                found_archive = true;
                let archived_corrupt = entry
                    .path()
                    .join("inboxes")
                    .join("worker-1.json.corrupt-2024-01-01");
                assert!(
                    archived_corrupt.exists(),
                    "corrupt backup should be archived"
                );
                break;
            }
        }
        assert!(found_archive, "expected an archived team directory");

        let _ = std::fs::remove_dir_all(&dir);
    }
}
