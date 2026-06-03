//! Nudge generation and sending for the supervisor.
//!
//! Generates and sends nudges to stuck agents via AgentGateway.

use std::sync::Arc;

use chrono::Utc;
use tracing::debug;

use brehon_ports::{AgentGateway, PortError};
use brehon_types::{MessageKind, PromptId, PromptTurn, SessionId};

const DEFAULT_HISTORY_LIMIT: usize = 1000;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NudgeKind {
    Soft,
    Guidance,
    Redirect,
    Resume,
}

impl NudgeKind {
    pub fn as_str(&self) -> &'static str {
        match self {
            NudgeKind::Soft => "soft",
            NudgeKind::Guidance => "guidance",
            NudgeKind::Redirect => "redirect",
            NudgeKind::Resume => "resume",
        }
    }
}

#[derive(Debug, Clone)]
pub struct Nudge {
    pub kind: NudgeKind,
    pub session_id: SessionId,
    pub content: String,
}

#[derive(Debug, Clone)]
pub struct NudgeId(pub String);

impl NudgeId {
    pub fn new(session_id: &str, kind: &str, sent_at: chrono::DateTime<Utc>) -> Self {
        Self(format!(
            "{}-{}-{}",
            session_id,
            kind,
            sent_at
                .timestamp_nanos_opt()
                .unwrap_or_else(|| sent_at.timestamp())
        ))
    }
}

#[derive(Debug, Clone)]
pub struct NudgeHistoryEntry {
    pub nudge_id: NudgeId,
    pub nudge: Nudge,
    pub sent_at: chrono::DateTime<Utc>,
    pub success: bool,
}

impl NudgeHistoryEntry {
    pub fn new(nudge: Nudge) -> Self {
        let sent_at = Utc::now();
        Self {
            nudge_id: NudgeId::new(nudge.session_id.as_str(), nudge.kind.as_str(), sent_at),
            nudge,
            sent_at,
            success: true,
        }
    }
}

pub struct NudgeGenerator {
    soft_templates: Vec<String>,
    guidance_templates: Vec<String>,
    redirect_templates: Vec<String>,
    resume_templates: Vec<String>,
}

impl Default for NudgeGenerator {
    fn default() -> Self {
        Self::new()
    }
}

impl NudgeGenerator {
    pub fn new() -> Self {
        Self {
            soft_templates: vec![
                "Are you still working on this task? Please provide a status update.".into(),
                "I haven't seen activity in a while. How's the progress?".into(),
                "Checking in - is there anything blocking you?".into(),
            ],
            guidance_templates: vec![
                "I noticed you might be stuck. Consider reviewing the requirements again and breaking down the task into smaller steps.".into(),
                "Looking at your progress, you may want to try a different approach. Would you like to discuss alternatives?".into(),
                "The current approach doesn't seem to be working. Perhaps try examining the error messages more closely or consider edge cases.".into(),
            ],
            redirect_templates: vec![
                "This approach isn't progressing well. Please stop the current work and try a simpler solution first.".into(),
                "I'm redirecting you to work on a different subtask. Please wrap up cleanly before switching.".into(),
                "The current thread isn't productive. Step back and reassess the situation before continuing.".into(),
            ],
            resume_templates: vec![
                "The operation has completed. You can now continue with your work.".into(),
                "The long-running process has finished. Please proceed with the next steps.".into(),
                "Ready to continue. Please resume your task from where you left off.".into(),
            ],
        }
    }

    pub fn generate(&self, kind: NudgeKind, context: Option<&str>) -> String {
        let templates = match kind {
            NudgeKind::Soft => &self.soft_templates,
            NudgeKind::Guidance => &self.guidance_templates,
            NudgeKind::Redirect => &self.redirect_templates,
            NudgeKind::Resume => &self.resume_templates,
        };

        let base = &templates[0];

        if let Some(ctx) = context {
            format!("{}\n\nContext: {}", base, ctx)
        } else {
            base.clone()
        }
    }

    pub fn generate_with_pattern(&self, kind: NudgeKind, pattern: Option<&str>) -> String {
        match pattern {
            Some(p) if p.contains("error") => {
                format!(
                    "I noticed you're encountering repeated errors ({}). Please carefully review the error message and consider if there's a different approach.",
                    p
                )
            }
            Some(p) if p.contains("retry") => {
                "You seem to be stuck in a retry loop. Consider whether the current approach is viable, or if you need to try something different.".into()
            }
            Some(p) => {
                self.generate(kind, Some(&format!("Pattern detected: {}", p)))
            }
            None => self.generate(kind, None),
        }
    }
}

pub struct NudgeSender {
    gateway: Arc<dyn AgentGateway>,
    generator: NudgeGenerator,
    history: Vec<NudgeHistoryEntry>,
    #[allow(dead_code)]
    history_limit: usize,
}

#[derive(Debug, Clone)]
pub struct PreparedNudge {
    pub session_id: SessionId,
    pub kind: NudgeKind,
    pub content: String,
    pub prompt: PromptTurn,
}

impl NudgeSender {
    pub fn new(gateway: Arc<dyn AgentGateway>) -> Self {
        Self {
            gateway,
            generator: NudgeGenerator::new(),
            history: Vec::new(),
            history_limit: DEFAULT_HISTORY_LIMIT,
        }
    }

    #[allow(dead_code)]
    pub fn with_history_limit(mut self, limit: usize) -> Self {
        self.history_limit = limit;
        self
    }

    #[allow(dead_code)]
    pub async fn send_nudge(
        &mut self,
        session_id: &SessionId,
        kind: NudgeKind,
        context: Option<&str>,
    ) -> Result<(), PortError> {
        let content = self.generator.generate(kind, context);

        let prompt = PromptTurn {
            prompt_id: PromptId::new(uuid::Uuid::new_v4().to_string()),
            content: content.clone(),
            kind: MessageKind::Nudge,
            sent_at: Utc::now(),
        };

        debug!(
            session_id = %session_id,
            kind = ?kind,
            "Sending nudge"
        );

        let result = self.gateway.send_prompt(session_id, prompt).await;

        let success = result.is_ok();

        let mut entry = NudgeHistoryEntry::new(Nudge {
            kind,
            session_id: session_id.clone(),
            content,
        });
        entry.success = success;

        self.history.push(entry);

        if self.history_limit > 0 && self.history.len() > self.history_limit {
            let remove = self.history.len() - self.history_limit;
            self.history.drain(0..remove);
        }

        result.map(|_| ())
    }

    pub fn gateway(&self) -> Arc<dyn AgentGateway> {
        Arc::clone(&self.gateway)
    }

    pub fn prepare_nudge_with_pattern(
        &self,
        session_id: &SessionId,
        kind: NudgeKind,
        pattern: Option<&str>,
    ) -> PreparedNudge {
        let content = self.generator.generate_with_pattern(kind, pattern);
        let prompt = PromptTurn {
            prompt_id: PromptId::new(uuid::Uuid::new_v4().to_string()),
            content: content.clone(),
            kind: MessageKind::Nudge,
            sent_at: Utc::now(),
        };
        PreparedNudge {
            session_id: session_id.clone(),
            kind,
            content,
            prompt,
        }
    }

    pub fn record_prepared_nudge(&mut self, prepared: &PreparedNudge, success: bool) {
        let mut entry = NudgeHistoryEntry::new(Nudge {
            kind: prepared.kind,
            session_id: prepared.session_id.clone(),
            content: prepared.content.clone(),
        });
        entry.success = success;
        self.history.push(entry);

        if self.history_limit > 0 && self.history.len() > self.history_limit {
            let remove = self.history.len() - self.history_limit;
            self.history.drain(0..remove);
        }
    }

    pub async fn send_nudge_with_pattern(
        &mut self,
        session_id: &SessionId,
        kind: NudgeKind,
        pattern: Option<&str>,
    ) -> Result<(), PortError> {
        let prepared = self.prepare_nudge_with_pattern(session_id, kind, pattern);

        debug!(
            session_id = %session_id,
            kind = ?kind,
            pattern = ?pattern,
            "Sending nudge with pattern"
        );

        let result = self
            .gateway
            .send_prompt(session_id, prepared.prompt.clone())
            .await;

        let success = result.is_ok();
        self.record_prepared_nudge(&prepared, success);

        result.map(|_| ())
    }

    #[allow(dead_code)]
    pub fn history(&self) -> &[NudgeHistoryEntry] {
        &self.history
    }

    #[allow(dead_code)]
    pub fn nudge_count_for_session(&self, session_id: &str) -> usize {
        self.history
            .iter()
            .filter(|e| e.nudge.session_id.as_str() == session_id)
            .count()
    }

    pub fn last_nudge_for_session(&self, session_id: &str) -> Option<&NudgeHistoryEntry> {
        self.history
            .iter()
            .rev()
            .find(|e| e.nudge.session_id.as_str() == session_id)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use brehon_test_harness::MockGateway;
    use brehon_types::SessionSpec;

    #[test]
    fn generate_soft_nudge() {
        let generator = NudgeGenerator::new();
        let nudge = generator.generate(NudgeKind::Soft, None);
        assert!(nudge.contains("still working") || nudge.contains("progress"));
    }

    #[test]
    fn generate_guidance_nudge() {
        let generator = NudgeGenerator::new();
        let nudge = generator.generate(NudgeKind::Guidance, None);
        assert!(nudge.contains("stuck") || nudge.contains("approach"));
    }

    #[test]
    fn generate_with_context() {
        let generator = NudgeGenerator::new();
        let nudge = generator.generate(NudgeKind::Soft, Some("Task: implement auth"));
        assert!(nudge.contains("Context:") || nudge.contains("Task"));
    }

    #[test]
    fn generate_with_error_pattern() {
        let generator = NudgeGenerator::new();
        let nudge = generator.generate_with_pattern(NudgeKind::Soft, Some("error_loop"));
        assert!(nudge.contains("error"));
    }

    #[test]
    fn generate_with_retry_pattern() {
        let generator = NudgeGenerator::new();
        let nudge = generator.generate_with_pattern(NudgeKind::Guidance, Some("retry_loop"));
        assert!(nudge.contains("retry"));
    }

    #[test]
    fn nudge_kind_to_string() {
        assert_eq!(NudgeKind::Soft.as_str(), "soft");
        assert_eq!(NudgeKind::Guidance.as_str(), "guidance");
        assert_eq!(NudgeKind::Redirect.as_str(), "redirect");
        assert_eq!(NudgeKind::Resume.as_str(), "resume");
    }

    #[tokio::test]
    async fn send_nudge_via_gateway() {
        let gateway = Arc::new(MockGateway::new());
        let session_id = gateway
            .spawn(SessionSpec::new(
                brehon_types::AgentId::new("agent-1"),
                "worker".into(),
                "/tmp/test".into(),
            ))
            .await
            .unwrap();

        let mut sender = NudgeSender::new(gateway);
        let result = sender.send_nudge(&session_id, NudgeKind::Soft, None).await;
        assert!(result.is_ok());

        let history = sender.history();
        assert_eq!(history.len(), 1);
        assert!(history[0].success);
    }

    #[tokio::test]
    async fn send_nudge_with_pattern_via_gateway() {
        let gateway = Arc::new(MockGateway::new());
        let session_id = gateway
            .spawn(SessionSpec::new(
                brehon_types::AgentId::new("agent-1"),
                "worker".into(),
                "/tmp/test".into(),
            ))
            .await
            .unwrap();

        let mut sender = NudgeSender::new(gateway);
        let result = sender
            .send_nudge_with_pattern(&session_id, NudgeKind::Guidance, Some("error_loop"))
            .await;
        assert!(result.is_ok());

        let history = sender.history();
        assert_eq!(history.len(), 1);
        assert_eq!(history[0].nudge.kind, NudgeKind::Guidance);
    }

    #[tokio::test]
    async fn last_nudge_for_session() {
        let gateway = Arc::new(MockGateway::new());
        let session_id = gateway
            .spawn(SessionSpec::new(
                brehon_types::AgentId::new("agent-1"),
                "worker".into(),
                "/tmp/test".into(),
            ))
            .await
            .unwrap();

        let mut sender = NudgeSender::new(gateway);
        let _ = sender.send_nudge(&session_id, NudgeKind::Soft, None).await;

        let last = sender.last_nudge_for_session(session_id.as_str());
        assert!(last.is_some());
        assert_eq!(last.unwrap().nudge.kind, NudgeKind::Soft);
    }
}
