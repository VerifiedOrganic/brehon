//! Clean-room advisory semantic detection over runtime events.
//!
//! Detectors in this crate never mutate pane state and never send commands.
//! They consume runtime events and emit advisory `DetectionEvent` runtime events
//! for downstream policy, workflow, dashboard, or daemon consumers.

use std::collections::{HashMap, VecDeque};
use std::sync::Mutex;

use async_trait::async_trait;
use brehon_ports::{DetectionEngine, PortError, RuntimeEventSink, RuntimeEventStream};
use brehon_types::{
    DetectionEvent, DetectionSeverity, PaneOutputEvent, RuntimeEvent, RuntimeEventKind,
    RuntimeEventMeta, RuntimeSource, RuntimeTextSpan,
};

/// Default number of normalized output lines retained per pane.
pub const DEFAULT_MAX_LINES_PER_PANE: usize = 512;

/// Counters returned when a detection loop exits.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct DetectionLoopStats {
    pub observed_events: u64,
    pub emitted_detections: u64,
    pub stream_errors: u64,
    pub detection_errors: u64,
    pub publish_errors: u64,
}

/// Rule that matches lowercased normalized output text.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PatternRule {
    pub id: String,
    pub severity: DetectionSeverity,
    pub message: String,
    pub patterns: Vec<String>,
}

impl PatternRule {
    pub fn new(
        id: impl Into<String>,
        severity: DetectionSeverity,
        message: impl Into<String>,
        patterns: impl IntoIterator<Item = impl Into<String>>,
    ) -> Self {
        Self {
            id: id.into(),
            severity,
            message: message.into(),
            patterns: patterns
                .into_iter()
                .map(|pattern| pattern.into().to_ascii_lowercase())
                .collect(),
        }
    }
}

/// Advisory pattern detector for runtime output.
pub struct PatternDetectionEngine {
    rules: Vec<PatternRule>,
    max_lines_per_pane: usize,
    state: Mutex<HashMap<PaneKey, PaneDetectionState>>,
}

impl Default for PatternDetectionEngine {
    fn default() -> Self {
        Self::new(default_rules())
    }
}

impl PatternDetectionEngine {
    pub fn new(rules: Vec<PatternRule>) -> Self {
        Self {
            rules,
            max_lines_per_pane: DEFAULT_MAX_LINES_PER_PANE,
            state: Mutex::new(HashMap::new()),
        }
    }

    #[must_use]
    pub fn with_max_lines_per_pane(mut self, max_lines_per_pane: usize) -> Self {
        self.max_lines_per_pane = max_lines_per_pane.max(1);
        self
    }

    pub fn rules(&self) -> &[PatternRule] {
        &self.rules
    }

    fn observe_output(&self, event: RuntimeEvent, output: PaneOutputEvent) -> Vec<RuntimeEvent> {
        let text = output
            .text
            .unwrap_or_else(|| String::from_utf8_lossy(&output.bytes).into_owned());
        let normalized = normalize_terminal_text(&text);
        if normalized.trim().is_empty() {
            return Vec::new();
        }

        let key = PaneKey::from_meta(&event.meta);
        let mut state = self.state.lock().unwrap_or_else(|err| err.into_inner());
        let pane_state = state.entry(key).or_default();
        let lines = split_normalized_lines(&normalized);
        let mut detections = Vec::new();

        for line in lines {
            pane_state.next_line_number = pane_state.next_line_number.saturating_add(1);
            let line_number = pane_state.next_line_number;
            let lower = line.to_ascii_lowercase();

            for rule in &self.rules {
                if rule.patterns.iter().any(|pattern| lower.contains(pattern)) {
                    pane_state.next_detection_sequence =
                        pane_state.next_detection_sequence.saturating_add(1);
                    detections.push(detection_event(
                        &event.meta,
                        rule,
                        pane_state.next_detection_sequence,
                        line_number,
                    ));
                }
            }

            pane_state.lines.push_back(line);
            while pane_state.lines.len() > self.max_lines_per_pane {
                pane_state.lines.pop_front();
            }
        }

        detections
    }
}

#[async_trait]
impl DetectionEngine for PatternDetectionEngine {
    async fn observe(&self, event: RuntimeEvent) -> Result<Vec<RuntimeEvent>, PortError> {
        match event.kind.clone() {
            RuntimeEventKind::PaneOutput(output) => Ok(self.observe_output(event, output)),
            _ => Ok(Vec::new()),
        }
    }
}

/// Run advisory detection until the input stream closes.
///
/// Runtime stream errors are counted and the loop continues. This keeps
/// backpressure loss in a detector from becoming a mux or daemon outage.
pub async fn run_detection_loop(
    stream: &mut dyn RuntimeEventStream,
    engine: &dyn DetectionEngine,
    sink: &dyn RuntimeEventSink,
) -> DetectionLoopStats {
    let mut stats = DetectionLoopStats::default();

    loop {
        let event = match stream.next_event().await {
            Ok(Some(event)) => event,
            Ok(None) => break,
            Err(err) => {
                stats.stream_errors = stats.stream_errors.saturating_add(1);
                tracing::warn!(error = %err, "Runtime detection stream error");
                tokio::task::yield_now().await;
                continue;
            }
        };
        stats.observed_events = stats.observed_events.saturating_add(1);

        let detections = match engine.observe(event).await {
            Ok(detections) => detections,
            Err(err) => {
                stats.detection_errors = stats.detection_errors.saturating_add(1);
                tracing::warn!(error = %err, "Runtime detection engine error");
                continue;
            }
        };

        for detection in detections {
            match sink.publish(detection).await {
                Ok(()) => {
                    stats.emitted_detections = stats.emitted_detections.saturating_add(1);
                }
                Err(err) => {
                    stats.publish_errors = stats.publish_errors.saturating_add(1);
                    tracing::warn!(error = %err, "Failed to publish runtime detection");
                }
            }
        }
    }

    stats
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
struct PaneKey {
    session_id: String,
    pane_id: String,
}

impl PaneKey {
    fn from_meta(meta: &RuntimeEventMeta) -> Self {
        Self {
            session_id: meta.session_id.clone(),
            pane_id: meta.pane_id.clone(),
        }
    }
}

#[derive(Default)]
struct PaneDetectionState {
    lines: VecDeque<String>,
    next_line_number: usize,
    next_detection_sequence: u64,
}

fn detection_event(
    source_meta: &RuntimeEventMeta,
    rule: &PatternRule,
    sequence: u64,
    line_number: usize,
) -> RuntimeEvent {
    let mut meta = source_meta.clone();
    meta.event_id = None;
    meta.source = RuntimeSource::Detector;
    RuntimeEvent::new(
        meta,
        RuntimeEventKind::DetectionEvent(DetectionEvent {
            detection_id: format!(
                "{}:{}:{}:{}",
                source_meta.session_id, source_meta.pane_id, rule.id, sequence
            ),
            rule_id: rule.id.clone(),
            severity: rule.severity,
            message: rule.message.clone(),
            span: Some(RuntimeTextSpan {
                start_line: line_number,
                end_line: line_number,
            }),
        }),
    )
}

/// Strip common terminal control sequences and normalize line endings.
pub fn normalize_terminal_text(input: &str) -> String {
    let mut out = String::with_capacity(input.len());
    let mut chars = input.chars().peekable();

    while let Some(ch) = chars.next() {
        if ch == '\u{1b}' {
            match chars.peek().copied() {
                Some('[') => {
                    chars.next();
                    for c in chars.by_ref() {
                        if ('@'..='~').contains(&c) {
                            break;
                        }
                    }
                }
                Some(']') => {
                    chars.next();
                    let mut prev_was_escape = false;
                    for c in chars.by_ref() {
                        if c == '\u{7}' || (prev_was_escape && c == '\\') {
                            break;
                        }
                        prev_was_escape = c == '\u{1b}';
                    }
                }
                Some(_) => {
                    chars.next();
                }
                None => {}
            }
            continue;
        }

        match ch {
            '\r' => out.push('\n'),
            c if c.is_control() && c != '\n' && c != '\t' => {}
            c => out.push(c),
        }
    }

    out
}

fn split_normalized_lines(input: &str) -> Vec<String> {
    input
        .lines()
        .map(str::trim)
        .filter(|line| !line.is_empty())
        .map(ToOwned::to_owned)
        .collect()
}

pub fn default_rules() -> Vec<PatternRule> {
    vec![
        PatternRule::new(
            "approval.prompt",
            DetectionSeverity::Warning,
            "Agent is requesting approval",
            [
                "requires approval",
                "permission request",
                "do you want to allow",
                "approve this command",
            ],
        ),
        PatternRule::new(
            "rate_limit.warning",
            DetectionSeverity::Warning,
            "Agent output indicates rate limiting",
            [
                "rate limit",
                "rate-limit",
                "too many requests",
                "http 429",
                "status 429",
            ],
        ),
        PatternRule::new(
            "usage_limit.warning",
            DetectionSeverity::Warning,
            "Agent output indicates usage or quota exhaustion",
            [
                "usage limit",
                "quota exceeded",
                "credit balance",
                "context length exceeded",
                "token limit",
            ],
        ),
        PatternRule::new(
            "auth.failure",
            DetectionSeverity::Blocking,
            "Agent output indicates authentication failure",
            [
                "authentication failed",
                "invalid api key",
                "unauthorized",
                "status 401",
                "http 401",
            ],
        ),
        PatternRule::new(
            "process.crash",
            DetectionSeverity::Blocking,
            "Agent output indicates a process crash",
            [
                "segmentation fault",
                "thread 'main' panicked",
                "traceback (most recent call last)",
                "fatal error:",
            ],
        ),
        PatternRule::new(
            "agent.completion_marker",
            DetectionSeverity::Info,
            "Agent output includes a known completion marker",
            ["brehon_task_complete", "brehon_done"],
        ),
    ]
}

#[cfg(test)]
mod tests {
    use super::*;
    use brehon_types::{RuntimeEventMeta, RuntimeSource};

    fn output_event(text: &str) -> RuntimeEvent {
        RuntimeEvent::new(
            RuntimeEventMeta::new("session", "worker-1", 3, RuntimeSource::Mux, 42),
            RuntimeEventKind::PaneOutput(PaneOutputEvent {
                bytes: text.as_bytes().to_vec(),
                text: None,
            }),
        )
    }

    #[tokio::test]
    async fn detects_rate_limit_from_output_bytes() {
        let engine = PatternDetectionEngine::default();
        let detections = engine
            .observe(output_event("Error: HTTP 429 rate limit exceeded\n"))
            .await
            .expect("observe");

        assert_eq!(detections.len(), 1);
        assert!(matches!(
            detections[0].kind,
            RuntimeEventKind::DetectionEvent(ref detection)
                if detection.rule_id == "rate_limit.warning"
        ));
        assert_eq!(detections[0].meta.source, RuntimeSource::Detector);
        assert_eq!(detections[0].meta.pane_id, "worker-1");
        assert_eq!(detections[0].meta.generation, 3);
    }

    #[tokio::test]
    async fn strips_ansi_before_matching() {
        let engine = PatternDetectionEngine::default();
        let detections = engine
            .observe(output_event("\u{1b}[31mAuthentication failed\u{1b}[0m\n"))
            .await
            .expect("observe");

        assert!(matches!(
            detections.first().map(|event| &event.kind),
            Some(RuntimeEventKind::DetectionEvent(detection))
                if detection.rule_id == "auth.failure"
        ));
    }

    #[tokio::test]
    async fn fixture_corpus_detects_representative_runtime_markers() {
        let engine = PatternDetectionEngine::default();
        let cases = [
            (
                include_str!("../tests/fixtures/rate_limit_pty.txt"),
                "rate_limit.warning",
            ),
            (
                include_str!("../tests/fixtures/approval_acp.txt"),
                "approval.prompt",
            ),
        ];

        for (fixture, expected_rule) in cases {
            let detections = engine
                .observe(output_event(fixture))
                .await
                .expect("observe fixture");
            assert!(
                detections.iter().any(|event| matches!(
                    &event.kind,
                    RuntimeEventKind::DetectionEvent(detection)
                        if detection.rule_id == expected_rule
                )),
                "fixture did not trigger {expected_rule}"
            );
        }
    }

    #[tokio::test]
    async fn fixture_corpus_ignores_success_noise() {
        let engine = PatternDetectionEngine::default();
        let detections = engine
            .observe(output_event(include_str!(
                "../tests/fixtures/success_noise.txt"
            )))
            .await
            .expect("observe fixture");

        assert!(detections.is_empty());
    }

    #[tokio::test]
    async fn ignores_non_output_events() {
        let engine = PatternDetectionEngine::default();
        let event = RuntimeEvent::new(
            RuntimeEventMeta::new("session", "worker-1", 3, RuntimeSource::Mux, 42),
            RuntimeEventKind::AgentTurnStarted(brehon_types::AgentTurnEvent {
                prompt_id: Some("p".to_string()),
                reason: None,
            }),
        );

        let detections = engine.observe(event).await.expect("observe");
        assert!(detections.is_empty());
    }

    #[test]
    fn normalizer_removes_csi_and_osc_sequences() {
        let input = "\u{1b}]0;title\u{7}\u{1b}[32mrate limit\u{1b}[0m\r\n";
        assert_eq!(normalize_terminal_text(input).trim(), "rate limit");
    }

    #[tokio::test]
    async fn detection_loop_forwards_detection_events_and_exits_on_close() {
        let input_bus = brehon_runtime::RuntimeEventBus::new(8);
        let output_bus = brehon_runtime::RuntimeEventBus::new(8);
        let mut input = input_bus.subscribe();
        let mut output = output_bus.subscribe();
        let engine = PatternDetectionEngine::default();

        input_bus
            .publish(output_event("too many requests: status 429\n"))
            .await
            .expect("publish input");
        drop(input_bus);

        let stats = run_detection_loop(&mut input, &engine, &output_bus).await;

        assert_eq!(stats.observed_events, 1);
        assert_eq!(stats.emitted_detections, 1);
        let detection = output
            .next_event()
            .await
            .expect("output stream")
            .expect("detection event");
        assert!(matches!(
            detection.kind,
            RuntimeEventKind::DetectionEvent(ref event)
                if event.rule_id == "rate_limit.warning"
        ));
    }
}
