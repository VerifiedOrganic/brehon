// SPDX-FileCopyrightText: 2026 Andrei G <bug-ops>
// SPDX-License-Identifier: MIT OR Apache-2.0
//
// Adapted from Zeph's native response/tool loop in
// `zeph-core/src/agent/tool_execution/native.rs`.

use std::sync::Arc;
use std::time::Duration;

use serde_json::Value;
use tokio::time::{Instant, MissedTickBehavior};

use crate::agent_runtime::dispatch::{dispatch_tool_calls, emit_runtime_event};
use crate::agent_runtime::events::RuntimeEvent;
use crate::agent_runtime::executor::ToolExecutor;
use crate::agent_runtime::message::{
    apply_history_limits, sanitize_provider_message_history, AgentMessage,
};
use crate::agent_runtime::orchestrator::{ToolLoopControl, ToolOrchestrator};
use crate::agent_runtime::turn::{TurnState, TurnStopReason};
use crate::health::{refresh_agent_session, write_agent_health, AgentHealthStatus, AgentHeartbeat};
use crate::provider::{provider_error_to_string, ChatProvider, ProviderError, ProviderRequest};
use crate::runtime::CancellationToken;
use crate::server::RpcHandle;
use crate::tools::NativeTools;

const MAX_EMPTY_ASSISTANT_TURNS: usize = 2;
const PROVIDER_HEARTBEAT_SECS: u64 = 15;
const PROVIDER_RECOVERY_RETRIES: usize = 6;
#[cfg(not(test))]
const PROVIDER_RETRY_BASE_DELAY: Duration = Duration::from_secs(2);
#[cfg(test)]
const PROVIDER_RETRY_BASE_DELAY: Duration = Duration::from_millis(1);
#[cfg(not(test))]
const PROVIDER_RETRY_MAX_DELAY: Duration = Duration::from_secs(60);
#[cfg(test)]
const PROVIDER_RETRY_MAX_DELAY: Duration = Duration::from_millis(5);
const TOOL_REQUIRED_NUDGE: &str = "Please use the available tools to complete the task. \
Do not announce intentions; inspect state and execute the required work. \
If the work is already complete, use the appropriate Brehon tool to report that state.";

#[derive(Clone)]
pub(crate) struct AgentTurnConfig {
    pub(crate) model: String,
    pub(crate) reasoning_effort: Option<String>,
    pub(crate) reasoning_effort_param: Option<String>,
    pub(crate) extra_body: Option<Value>,
    pub(crate) max_history_messages: usize,
    /// Optional approximate token budget for retained history. When set, history
    /// is trimmed to fit this many tokens (in addition to the message-count cap)
    /// so small local context windows stay off the overflow cliff. `None`
    /// preserves the message-count-only behavior used by cloud endpoints.
    pub(crate) context_window_tokens: Option<usize>,
    pub(crate) provider_idle_timeout: Duration,
    pub(crate) max_parallel_tool_calls: usize,
    pub(crate) assistant_message_passthrough_fields: Vec<String>,
    pub(crate) role: String,
    pub(crate) agent_name: String,
    pub(crate) agent_type: String,
    pub(crate) nudge_text_only_first_turn: bool,
}

pub(crate) struct AgentTurnRunner {
    provider: Arc<dyn ChatProvider>,
    config: AgentTurnConfig,
    tools: NativeTools,
    tool_definitions: Vec<Value>,
}

impl AgentTurnRunner {
    pub(crate) fn new(
        provider: Arc<dyn ChatProvider>,
        config: AgentTurnConfig,
        tools: NativeTools,
    ) -> Self {
        let tool_definitions = ToolExecutor::tool_definitions(&tools);
        Self {
            provider,
            config,
            tools,
            tool_definitions,
        }
    }

    pub(crate) async fn run(
        &self,
        rpc: &RpcHandle,
        session_id: &str,
        cancel: &CancellationToken,
        mut messages: Vec<AgentMessage>,
    ) -> Result<AgentTurnOutcome, AgentTurnError> {
        let mut turn_state = TurnState::new(
            session_id,
            &self.config.role,
            &self.config.agent_name,
            &self.config.agent_type,
        );
        let mut orchestrator = ToolOrchestrator::new();
        orchestrator.begin_turn();

        let mut response_text = String::new();
        let mut tokens_used = None;
        let mut stop_reason = Some("stop".to_string());
        let mut empty_assistant_turns = 0usize;
        let mut text_only_tool_nudge_sent = false;
        let mut any_tool_called = false;
        let heartbeat = self.agent_heartbeat(session_id);
        refresh_agent_session(&heartbeat);
        write_agent_health(&heartbeat, AgentHealthStatus::Available, None, None, None);
        loop {
            turn_state.advance_round();
            refresh_agent_session(&heartbeat);
            if cancel.is_cancelled() {
                turn_state.set_stop_reason(TurnStopReason::Cancelled);
                stop_reason = Some("cancelled".to_string());
                return Ok(AgentTurnOutcome {
                    messages,
                    response: empty_to_none(response_text),
                    tokens_used,
                    stop_reason,
                    success: false,
                });
            }

            sanitize_provider_message_history(&mut messages);
            let assistant_result = self
                .complete_with_recovery(rpc, cancel, &messages, &mut turn_state, &heartbeat)
                .await;
            let assistant = match assistant_result {
                Ok(assistant) => assistant,
                Err(failure) => {
                    let message = failure.to_message();
                    if !matches!(failure, ModelCallFailure::Cancelled(_)) {
                        write_agent_health(
                            &heartbeat,
                            AgentHealthStatus::Unavailable,
                            Some(failure.reason()),
                            None,
                            Some(&message),
                        );
                    }
                    return Err(AgentTurnError::new(message, messages));
                }
            };
            write_agent_health(&heartbeat, AgentHealthStatus::Available, None, None, None);

            accumulate_tokens(&mut tokens_used, assistant.tokens_used);
            stop_reason = assistant.stop_reason.clone().or(stop_reason);
            let assistant_text = assistant
                .content
                .as_deref()
                .map(str::trim)
                .filter(|text| !text.is_empty())
                .map(str::to_string);

            if assistant_text.is_none() && assistant.tool_calls.is_empty() {
                empty_assistant_turns += 1;
                if empty_assistant_turns > MAX_EMPTY_ASSISTANT_TURNS {
                    turn_state.set_stop_reason(TurnStopReason::ProviderTimeout);
                    return Err(AgentTurnError::new(
                        "model returned empty assistant turns without tool calls".to_string(),
                        messages,
                    ));
                }

                emit_runtime_event(
                    rpc,
                    RuntimeEvent::Progress(
                        "model returned an empty turn; asking it to continue".to_string(),
                    ),
                )
                .await;
                messages.push(AgentMessage::user(
                    "Your previous assistant turn returned no content and no tool calls. \
Continue the same task now. Use available tools when the prompt requires tool-backed work; \
otherwise provide a concise final answer.",
                ));
                apply_history_limits(
                    &mut messages,
                    self.config.max_history_messages,
                    self.config.context_window_tokens,
                );
                continue;
            }
            empty_assistant_turns = 0;

            match orchestrator.observe_assistant_message(&assistant.history_message) {
                ToolLoopControl::Continue => {}
                ToolLoopControl::Stop(reason) => {
                    return Err(AgentTurnError::new(reason, messages));
                }
            }

            messages.push(assistant.history_message);
            apply_history_limits(
                &mut messages,
                self.config.max_history_messages,
                self.config.context_window_tokens,
            );

            if let Some(text) = assistant_text {
                let text = format!("{text}\n");
                response_text.push_str(&text);
                emit_runtime_event(rpc, RuntimeEvent::MessageChunk(text)).await;
            }

            if assistant.tool_calls.is_empty() {
                if self.config.nudge_text_only_first_turn
                    && !any_tool_called
                    && !text_only_tool_nudge_sent
                {
                    text_only_tool_nudge_sent = true;
                    emit_runtime_event(
                        rpc,
                        RuntimeEvent::Progress(
                            "model answered without tool calls; nudging once to use tools"
                                .to_string(),
                        ),
                    )
                    .await;
                    messages.push(AgentMessage::user(TOOL_REQUIRED_NUDGE));
                    apply_history_limits(
                        &mut messages,
                        self.config.max_history_messages,
                        self.config.context_window_tokens,
                    );
                    continue;
                }
                turn_state.set_stop_reason(TurnStopReason::Stop);
                return Ok(AgentTurnOutcome {
                    messages,
                    response: empty_to_none(response_text),
                    tokens_used,
                    stop_reason,
                    success: true,
                });
            }

            match orchestrator.observe_tool_calls(&assistant.tool_calls) {
                ToolLoopControl::Continue => {}
                ToolLoopControl::Stop(reason) => {
                    return Err(AgentTurnError::new(reason, messages));
                }
            }

            let tool_calls = assistant.tool_calls;
            any_tool_called = true;
            let tool_results = dispatch_tool_calls(
                rpc,
                session_id,
                cancel,
                &self.tools,
                self.config.max_parallel_tool_calls,
                tool_calls.clone(),
            )
            .await;
            for result in tool_results {
                refresh_agent_session(&heartbeat);
                messages.push(AgentMessage::tool_result(
                    result.tool_call_id,
                    result.content,
                    result.is_error,
                ));
                apply_history_limits(
                    &mut messages,
                    self.config.max_history_messages,
                    self.config.context_window_tokens,
                );
            }
        }
    }

    async fn complete_with_recovery(
        &self,
        rpc: &RpcHandle,
        cancel: &CancellationToken,
        messages: &[AgentMessage],
        turn_state: &mut TurnState,
        heartbeat: &AgentHeartbeat<'_>,
    ) -> Result<crate::agent_runtime::message::AssistantTurn, ModelCallFailure> {
        let mut failed_attempts = 0usize;
        loop {
            refresh_agent_session(heartbeat);
            let result = self
                .complete_once(rpc, cancel, messages, turn_state, heartbeat)
                .await;
            match result {
                Ok(assistant) => return Ok(assistant),
                Err(failure)
                    if failure.is_retryable() && failed_attempts < PROVIDER_RECOVERY_RETRIES =>
                {
                    failed_attempts += 1;
                    let delay = provider_retry_delay(failed_attempts);
                    let message = failure.to_message();
                    write_agent_health(
                        heartbeat,
                        AgentHealthStatus::Recovering,
                        Some(failure.reason()),
                        Some(failed_attempts),
                        Some(&message),
                    );
                    emit_runtime_event(
                        rpc,
                        RuntimeEvent::Progress(format!(
                            "model call hit recoverable {} failure; retry {}/{} in {}s: {}",
                            failure.reason(),
                            failed_attempts,
                            PROVIDER_RECOVERY_RETRIES,
                            delay.as_secs(),
                            message
                        )),
                    )
                    .await;
                    tokio::select! {
                        _ = tokio::time::sleep(delay) => {}
                        _ = cancel.cancelled() => {
                            turn_state.set_stop_reason(TurnStopReason::Cancelled);
                            return Err(ModelCallFailure::Cancelled(
                                "model recovery cancelled during native-agent turn".to_string(),
                            ));
                        }
                    }
                }
                Err(failure) => return Err(failure),
            }
        }
    }

    async fn complete_once(
        &self,
        rpc: &RpcHandle,
        cancel: &CancellationToken,
        messages: &[AgentMessage],
        turn_state: &mut TurnState,
        heartbeat: &AgentHeartbeat<'_>,
    ) -> Result<crate::agent_runtime::message::AssistantTurn, ModelCallFailure> {
        let provider_started_at = Instant::now();
        let (activity_tx, mut activity_rx) = tokio::sync::watch::channel(provider_started_at);
        let completion = self.provider.complete(
            ProviderRequest {
                model: &self.config.model,
                reasoning_effort: self.config.reasoning_effort.as_deref(),
                reasoning_effort_param: self.config.reasoning_effort_param.as_deref(),
                messages,
                tools: &self.tool_definitions,
                extra_body: self.config.extra_body.as_ref(),
                assistant_message_passthrough_fields: &self
                    .config
                    .assistant_message_passthrough_fields,
                activity: Some(activity_tx),
            },
            cancel,
        );
        tokio::pin!(completion);
        let mut last_activity_at = provider_started_at;
        let idle_timeout =
            tokio::time::sleep_until(last_activity_at + self.config.provider_idle_timeout);
        tokio::pin!(idle_timeout);
        let heartbeat_start = provider_started_at + Duration::from_secs(PROVIDER_HEARTBEAT_SECS);
        let mut heartbeat_tick = tokio::time::interval_at(
            heartbeat_start,
            Duration::from_secs(PROVIDER_HEARTBEAT_SECS),
        );
        heartbeat_tick.set_missed_tick_behavior(MissedTickBehavior::Delay);
        let mut activity_open = true;

        loop {
            tokio::select! {
                result = &mut completion => {
                    match result {
                        Ok(assistant) => return Ok(assistant),
                        Err(err) => return Err(ModelCallFailure::Provider(err)),
                    }
                }
                changed = activity_rx.changed(), if activity_open => {
                    if changed.is_ok() {
                        last_activity_at = *activity_rx.borrow_and_update();
                        idle_timeout
                            .as_mut()
                            .reset(last_activity_at + self.config.provider_idle_timeout);
                        refresh_agent_session(heartbeat);
                    } else {
                        activity_open = false;
                    }
                }
                _ = &mut idle_timeout => {
                    turn_state.set_stop_reason(TurnStopReason::ProviderTimeout);
                    let message = format!(
                        "model stream went idle for {}s during native-agent turn",
                        self.config.provider_idle_timeout.as_secs(),
                    );
                    emit_runtime_event(rpc, RuntimeEvent::Progress(message.clone())).await;
                    return Err(ModelCallFailure::IdleTimeout(message));
                }
                _ = cancel.cancelled() => {
                    turn_state.set_stop_reason(TurnStopReason::Cancelled);
                    let message = "model call cancelled during native-agent turn".to_string();
                    emit_runtime_event(rpc, RuntimeEvent::Progress(message.clone())).await;
                    return Err(ModelCallFailure::Cancelled(message));
                }
                _ = heartbeat_tick.tick() => {
                    refresh_agent_session(heartbeat);
                    emit_runtime_event(
                        rpc,
                        RuntimeEvent::Progress(format!(
                            "model stream active (elapsed {}s, idle {}s, idle-limit {}s)",
                            provider_started_at.elapsed().as_secs(),
                            last_activity_at.elapsed().as_secs(),
                            self.config.provider_idle_timeout.as_secs()
                        )),
                    )
                    .await;
                }
            }
        }
    }

    fn agent_heartbeat<'a>(&'a self, session_id: &'a str) -> AgentHeartbeat<'a> {
        AgentHeartbeat {
            agent_name: &self.config.agent_name,
            role: &self.config.role,
            agent_type: &self.config.agent_type,
            session_id,
            model: &self.config.model,
            reasoning_effort: self.config.reasoning_effort.as_deref(),
        }
    }
}

#[derive(Debug)]
pub(crate) struct AgentTurnOutcome {
    pub(crate) messages: Vec<AgentMessage>,
    pub(crate) response: Option<String>,
    pub(crate) tokens_used: Option<u64>,
    pub(crate) stop_reason: Option<String>,
    pub(crate) success: bool,
}

#[derive(Debug)]
pub(crate) struct AgentTurnError {
    pub(crate) message: String,
    pub(crate) messages: Vec<AgentMessage>,
}

impl AgentTurnError {
    fn new(message: String, messages: Vec<AgentMessage>) -> Self {
        Self { message, messages }
    }
}

fn empty_to_none(value: String) -> Option<String> {
    if value.trim().is_empty() {
        None
    } else {
        Some(value)
    }
}

fn accumulate_tokens(total: &mut Option<u64>, tokens: Option<u64>) {
    let Some(tokens) = tokens else {
        return;
    };
    *total = Some(total.unwrap_or(0).saturating_add(tokens));
}

#[derive(Debug)]
enum ModelCallFailure {
    Provider(ProviderError),
    IdleTimeout(String),
    Cancelled(String),
}

impl ModelCallFailure {
    fn is_retryable(&self) -> bool {
        match self {
            Self::Provider(err) => err.is_retryable(),
            Self::IdleTimeout(_) => true,
            Self::Cancelled(_) => false,
        }
    }

    fn reason(&self) -> &'static str {
        match self {
            Self::Provider(err) => err.category(),
            Self::IdleTimeout(_) => "stream_idle_timeout",
            Self::Cancelled(_) => "cancelled",
        }
    }

    fn to_message(&self) -> String {
        match self {
            Self::Provider(err) => provider_error_to_string(err.clone()),
            Self::IdleTimeout(message) | Self::Cancelled(message) => message.clone(),
        }
    }
}

fn provider_retry_delay(failed_attempt: usize) -> Duration {
    let shift = failed_attempt.saturating_sub(1).min(10) as u32;
    PROVIDER_RETRY_BASE_DELAY
        .saturating_mul(2u32.saturating_pow(shift))
        .min(PROVIDER_RETRY_MAX_DELAY)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::VecDeque;

    use async_trait::async_trait;
    use serde_json::json;
    use tokio::sync::Mutex;

    use crate::agent_runtime::message::{AssistantTurn, ToolUseRequest};
    use crate::permissions::new_permission_grant_store;
    use crate::provider::ProviderError;
    use crate::runtime::PermissionMode;
    use brehon_types::config::PermissionsConfig;

    struct ScriptedProvider {
        turns: Mutex<VecDeque<AssistantTurn>>,
        seen_prompts: Mutex<Vec<Vec<String>>>,
    }

    impl ScriptedProvider {
        fn new(turns: Vec<AssistantTurn>) -> Self {
            Self {
                turns: Mutex::new(turns.into()),
                seen_prompts: Mutex::new(Vec::new()),
            }
        }

        async fn seen_prompts(&self) -> Vec<Vec<String>> {
            self.seen_prompts.lock().await.clone()
        }
    }

    #[async_trait]
    impl ChatProvider for ScriptedProvider {
        async fn complete(
            &self,
            request: ProviderRequest<'_>,
            _cancel: &CancellationToken,
        ) -> Result<AssistantTurn, ProviderError> {
            self.seen_prompts.lock().await.push(
                request
                    .messages
                    .iter()
                    .map(AgentMessage::text_content)
                    .collect(),
            );
            self.turns
                .lock()
                .await
                .pop_front()
                .ok_or_else(|| ProviderError::Request("no scripted turn".to_string()))
        }
    }

    struct SlowProvider {
        delay: Duration,
    }

    #[async_trait]
    impl ChatProvider for SlowProvider {
        async fn complete(
            &self,
            _request: ProviderRequest<'_>,
            _cancel: &CancellationToken,
        ) -> Result<AssistantTurn, ProviderError> {
            tokio::time::sleep(self.delay).await;
            Ok(assistant_text("late response"))
        }
    }

    struct ActiveSlowProvider {
        activity_count: usize,
        activity_interval: Duration,
    }

    #[async_trait]
    impl ChatProvider for ActiveSlowProvider {
        async fn complete(
            &self,
            request: ProviderRequest<'_>,
            _cancel: &CancellationToken,
        ) -> Result<AssistantTurn, ProviderError> {
            for _ in 0..self.activity_count {
                tokio::time::sleep(self.activity_interval).await;
                if let Some(activity) = request.activity.as_ref() {
                    let _ = activity.send(Instant::now());
                }
            }
            Ok(assistant_text("stream completed"))
        }
    }

    struct FlakyProvider {
        outcomes: Mutex<VecDeque<Result<AssistantTurn, ProviderError>>>,
        calls: Mutex<usize>,
    }

    impl FlakyProvider {
        fn new(outcomes: Vec<Result<AssistantTurn, ProviderError>>) -> Self {
            Self {
                outcomes: Mutex::new(outcomes.into()),
                calls: Mutex::new(0),
            }
        }

        async fn calls(&self) -> usize {
            *self.calls.lock().await
        }
    }

    #[async_trait]
    impl ChatProvider for FlakyProvider {
        async fn complete(
            &self,
            _request: ProviderRequest<'_>,
            _cancel: &CancellationToken,
        ) -> Result<AssistantTurn, ProviderError> {
            *self.calls.lock().await += 1;
            self.outcomes
                .lock()
                .await
                .pop_front()
                .ok_or_else(|| ProviderError::Request("no scripted outcome".to_string()))?
        }
    }

    fn assistant_text(text: &str) -> AssistantTurn {
        assistant_text_with_tokens(text, 0)
    }

    fn assistant_text_with_tokens(text: &str, tokens_used: u64) -> AssistantTurn {
        AssistantTurn {
            content: Some(text.to_string()),
            tool_calls: Vec::new(),
            history_message: AgentMessage::assistant(Some(text.to_string()), Vec::new()),
            tokens_used: Some(tokens_used),
            stop_reason: Some("stop".to_string()),
        }
    }

    fn assistant_tool_call_with_tokens(
        id: &str,
        name: &str,
        arguments: Value,
        tokens_used: u64,
    ) -> AssistantTurn {
        let tool_calls = vec![ToolUseRequest::new(id, name, arguments)];
        AssistantTurn {
            content: None,
            tool_calls: tool_calls.clone(),
            history_message: AgentMessage::assistant(None, tool_calls),
            tokens_used: Some(tokens_used),
            stop_reason: Some("tool_calls".to_string()),
        }
    }

    fn assistant_empty() -> AssistantTurn {
        AssistantTurn {
            content: None,
            tool_calls: Vec::new(),
            history_message: AgentMessage::assistant(None, Vec::new()),
            tokens_used: Some(0),
            stop_reason: Some("stop".to_string()),
        }
    }

    fn test_runner(provider: Arc<dyn ChatProvider>, temp: &tempfile::TempDir) -> AgentTurnRunner {
        test_runner_with_timeout(provider, temp, Duration::from_secs(5))
    }

    fn test_runner_with_timeout(
        provider: Arc<dyn ChatProvider>,
        temp: &tempfile::TempDir,
        provider_idle_timeout: Duration,
    ) -> AgentTurnRunner {
        test_runner_with_options(provider, temp, provider_idle_timeout, false)
    }

    fn test_runner_with_options(
        provider: Arc<dyn ChatProvider>,
        temp: &tempfile::TempDir,
        provider_idle_timeout: Duration,
        nudge_text_only_first_turn: bool,
    ) -> AgentTurnRunner {
        let tools = NativeTools::new(
            temp.path().to_path_buf(),
            vec![
                (
                    "BREHON_ROOT".to_string(),
                    temp.path().join(".brehon").display().to_string(),
                ),
                ("BREHON_AGENT_ROLE".to_string(), "worker".to_string()),
                ("BREHON_AGENT_NAME".to_string(), "worker-1".to_string()),
                ("BREHON_SESSION_ID".to_string(), "session-1".to_string()),
            ],
            "mcp_brehon_".to_string(),
            true,
            PermissionMode::Bypass,
            PermissionsConfig::default(),
            new_permission_grant_store(),
        );
        AgentTurnRunner::new(
            provider,
            AgentTurnConfig {
                model: "fake-model".to_string(),
                reasoning_effort: None,
                reasoning_effort_param: None,
                extra_body: None,
                max_history_messages: 20,
                context_window_tokens: None,
                provider_idle_timeout,
                max_parallel_tool_calls: 8,
                assistant_message_passthrough_fields: Vec::new(),
                role: "worker".to_string(),
                agent_name: "worker-1".to_string(),
                agent_type: "native-worker".to_string(),
                nudge_text_only_first_turn,
            },
            tools,
        )
    }

    #[tokio::test]
    async fn plain_text_stop_is_not_overridden_by_prompt_text_parsing() {
        let temp = tempfile::tempdir().unwrap();
        let provider = Arc::new(ScriptedProvider::new(vec![assistant_text("I checked it.")]));
        let runner = test_runner(provider.clone(), &temp);
        let rpc = RpcHandle::new(tokio::io::sink());
        let cancel = CancellationToken::new();

        let outcome = runner
            .run(
                &rpc,
                "session-1",
                &cancel,
                vec![
                    AgentMessage::system("system"),
                    AgentMessage::user("You must call health action=check before stopping."),
                ],
            )
            .await
            .expect("plain text stop should be accepted by the generic agent loop");

        assert!(outcome.success);
        let seen = provider.seen_prompts().await;
        assert_eq!(seen.len(), 1);
        assert_eq!(outcome.response.as_deref(), Some("I checked it.\n"));
    }

    #[tokio::test]
    async fn empty_assistant_turn_gets_bounded_continue_prompt() {
        let temp = tempfile::tempdir().unwrap();
        let provider = Arc::new(ScriptedProvider::new(vec![
            assistant_empty(),
            assistant_text("continued"),
        ]));
        let runner = test_runner(provider.clone(), &temp);
        let rpc = RpcHandle::new(tokio::io::sink());
        let cancel = CancellationToken::new();

        let outcome = runner
            .run(
                &rpc,
                "session-1",
                &cancel,
                vec![
                    AgentMessage::system("system"),
                    AgentMessage::user("review the change"),
                ],
            )
            .await
            .expect("empty turn should be retried once");

        assert!(outcome.success);
        assert_eq!(outcome.response.as_deref(), Some("continued\n"));
        let seen = provider.seen_prompts().await;
        assert_eq!(seen.len(), 2);
        assert!(seen[1]
            .iter()
            .any(|message| message.contains("previous assistant turn returned no content")));
    }

    #[tokio::test]
    async fn repeated_empty_assistant_turns_fail_visibly() {
        let temp = tempfile::tempdir().unwrap();
        let provider = Arc::new(ScriptedProvider::new(vec![
            assistant_empty(),
            assistant_empty(),
            assistant_empty(),
        ]));
        let runner = test_runner(provider, &temp);
        let rpc = RpcHandle::new(tokio::io::sink());
        let cancel = CancellationToken::new();

        let err = runner
            .run(
                &rpc,
                "session-1",
                &cancel,
                vec![AgentMessage::system("system"), AgentMessage::user("do it")],
            )
            .await
            .expect_err("repeated empty turns should fail");

        assert!(err.message.contains("empty assistant turns"));
    }

    #[tokio::test]
    async fn text_only_first_turn_gets_one_tool_use_nudge_when_enabled() {
        let temp = tempfile::tempdir().unwrap();
        let provider = Arc::new(ScriptedProvider::new(vec![
            assistant_text("I will inspect it first."),
            assistant_text("I still cannot proceed."),
        ]));
        let runner =
            test_runner_with_options(provider.clone(), &temp, Duration::from_secs(5), true);
        let rpc = RpcHandle::new(tokio::io::sink());
        let cancel = CancellationToken::new();

        let outcome = runner
            .run(
                &rpc,
                "session-1",
                &cancel,
                vec![
                    AgentMessage::system("system"),
                    AgentMessage::user("You have been assigned a task."),
                ],
            )
            .await
            .expect("second text-only turn should be accepted after one nudge");

        assert!(outcome.success);
        let seen = provider.seen_prompts().await;
        assert_eq!(seen.len(), 2);
        assert!(
            seen[1]
                .iter()
                .any(|message| message
                    .contains("Please use the available tools to complete the task"))
        );
    }

    #[tokio::test]
    async fn tool_loop_accumulates_tokens_across_model_calls() {
        let temp = tempfile::tempdir().unwrap();
        let provider = Arc::new(ScriptedProvider::new(vec![
            assistant_tool_call_with_tokens("call-1", "list_files", json!({"path": "."}), 10),
            assistant_text_with_tokens("done", 20),
        ]));
        let runner = test_runner(provider.clone(), &temp);
        let rpc = RpcHandle::new(tokio::io::sink());
        let cancel = CancellationToken::new();

        let outcome = runner
            .run(
                &rpc,
                "session-1",
                &cancel,
                vec![
                    AgentMessage::system("system"),
                    AgentMessage::user("list files then summarize"),
                ],
            )
            .await
            .expect("tool turn should complete");

        assert!(outcome.success);
        assert_eq!(outcome.tokens_used, Some(30));
        assert_eq!(provider.seen_prompts().await.len(), 2);
    }

    #[tokio::test]
    async fn slow_provider_turn_times_out() {
        let temp = tempfile::tempdir().unwrap();
        let provider = Arc::new(SlowProvider {
            delay: Duration::from_secs(1),
        });
        let runner = test_runner_with_timeout(provider, &temp, Duration::from_millis(10));
        let rpc = RpcHandle::new(tokio::io::sink());
        let cancel = CancellationToken::new();

        let err = runner
            .run(
                &rpc,
                "session-1",
                &cancel,
                vec![AgentMessage::system("system"), AgentMessage::user("do it")],
            )
            .await
            .expect_err("slow provider should time out visibly");

        assert!(err.message.contains("model stream went idle"));
    }

    #[tokio::test]
    async fn retryable_provider_failure_recovers_without_failing_turn() {
        let temp = tempfile::tempdir().unwrap();
        let provider = Arc::new(FlakyProvider::new(vec![
            Err(ProviderError::Http("connection reset".to_string())),
            Err(ProviderError::Request(
                "status 429 Too Many Requests: rate limited".to_string(),
            )),
            Ok(assistant_text("recovered")),
        ]));
        let runner = test_runner(provider.clone(), &temp);
        let rpc = RpcHandle::new(tokio::io::sink());
        let cancel = CancellationToken::new();

        let outcome = runner
            .run(
                &rpc,
                "session-1",
                &cancel,
                vec![AgentMessage::system("system"), AgentMessage::user("do it")],
            )
            .await
            .expect("retryable provider failures should recover inside the native turn");

        assert!(outcome.success);
        assert_eq!(outcome.response.as_deref(), Some("recovered\n"));
        assert_eq!(provider.calls().await, 3);
    }

    #[tokio::test]
    async fn non_retryable_provider_failure_does_not_spin() {
        let temp = tempfile::tempdir().unwrap();
        let provider = Arc::new(FlakyProvider::new(vec![Err(ProviderError::Request(
            "status 400 Bad Request: malformed payload".to_string(),
        ))]));
        let runner = test_runner(provider.clone(), &temp);
        let rpc = RpcHandle::new(tokio::io::sink());
        let cancel = CancellationToken::new();

        let err = runner
            .run(
                &rpc,
                "session-1",
                &cancel,
                vec![AgentMessage::system("system"), AgentMessage::user("do it")],
            )
            .await
            .expect_err("non-retryable provider errors should fail immediately");

        assert!(err.message.contains("status 400"));
        assert_eq!(provider.calls().await, 1);
    }

    #[tokio::test]
    async fn provider_activity_resets_idle_timeout() {
        let temp = tempfile::tempdir().unwrap();
        let provider = Arc::new(ActiveSlowProvider {
            activity_count: 4,
            activity_interval: Duration::from_millis(10),
        });
        let runner = test_runner_with_timeout(provider, &temp, Duration::from_millis(25));
        let rpc = RpcHandle::new(tokio::io::sink());
        let cancel = CancellationToken::new();

        let outcome = runner
            .run(
                &rpc,
                "session-1",
                &cancel,
                vec![AgentMessage::system("system"), AgentMessage::user("do it")],
            )
            .await
            .expect("stream activity should keep the provider turn alive");

        assert!(outcome.success);
        assert_eq!(outcome.response.as_deref(), Some("stream completed\n"));
    }

    #[tokio::test]
    async fn provider_wait_can_be_cancelled_before_timeout() {
        let temp = tempfile::tempdir().unwrap();
        let provider = Arc::new(SlowProvider {
            delay: Duration::from_secs(5),
        });
        let runner = test_runner_with_timeout(provider, &temp, Duration::from_secs(60));
        let rpc = RpcHandle::new(tokio::io::sink());
        let cancel = CancellationToken::new();
        let run = runner.run(
            &rpc,
            "session-1",
            &cancel,
            vec![AgentMessage::system("system"), AgentMessage::user("do it")],
        );
        tokio::pin!(run);

        tokio::select! {
            _ = tokio::time::sleep(Duration::from_millis(10)) => cancel.cancel(),
            result = &mut run => panic!("provider completed before cancellation: {result:?}"),
        }

        let err = run
            .await
            .expect_err("cancelled provider wait should fail visibly");
        assert!(err.message.contains("cancelled"));
    }
}
