//! Shared token-efficiency helpers for MCP context tools.

use std::io::{Read, Write};
use std::process::{Command, Stdio};
use std::thread;
use std::time::{Duration, Instant};

use brehon_types::config::{
    ContextCompressionConfig, ContextCompressionMode, ContextCompressionTarget,
    ContextRetrievalConfig, HeadroomCompressionConfig,
};

use crate::server::configured_project_root;

/// Effective context retrieval/compression settings.
#[derive(Debug, Clone)]
pub(crate) struct ContextToolOptions {
    pub(crate) retrieval: ContextRetrievalConfig,
    pub(crate) compression: ContextCompressionConfig,
    pub(crate) max_memories: usize,
}

impl Default for ContextToolOptions {
    fn default() -> Self {
        Self {
            retrieval: ContextRetrievalConfig::default(),
            compression: ContextCompressionConfig::default(),
            max_memories: 10_000,
        }
    }
}

impl ContextToolOptions {
    pub(crate) fn search_limit(&self, requested: Option<usize>) -> usize {
        let max_limit = self.retrieval.max_limit.max(1);
        let default_limit = self.retrieval.default_limit.max(1).min(max_limit);
        requested.unwrap_or(default_limit).max(1).min(max_limit)
    }

    pub(crate) fn snippet_chars(&self) -> usize {
        self.retrieval.snippet_chars.max(32)
    }

    pub(crate) fn should_compact_memories(&self) -> bool {
        self.compression.enabled && self.compression.compact_memories
    }

    pub(crate) fn should_compact_rules(&self) -> bool {
        self.compression.enabled && self.compression.compact_rules
    }

    pub(crate) fn should_compact_tasks(&self) -> bool {
        self.compression.enabled && self.compression.compact_tasks
    }
}

/// Load context tool options from the project config, falling back to defaults.
pub(crate) fn load_context_tool_options() -> ContextToolOptions {
    configured_project_root()
        .and_then(|root| brehon_config::load_config(Some(&root)).ok())
        .map(|config| ContextToolOptions {
            retrieval: config.context.retrieval,
            compression: config.context.compression,
            max_memories: config.context.max_memories as usize,
        })
        .unwrap_or_default()
}

/// Deterministically compact prose while preserving code-ish spans.
pub(crate) fn compact_deterministic_terse(input: &str) -> String {
    let mut output = String::with_capacity(input.len());
    let mut in_fence = false;

    for (line_index, line) in input.lines().enumerate() {
        if line_index > 0 {
            output.push('\n');
        }

        let trimmed_start = line.trim_start();
        if trimmed_start.starts_with("```") {
            in_fence = !in_fence;
            output.push_str(line);
            continue;
        }

        if in_fence || line.starts_with("    ") || line.starts_with('\t') {
            output.push_str(line);
            continue;
        }

        output.push_str(&compact_inline_prose(line));
    }

    if input.ends_with('\n') {
        output.push('\n');
    }

    output
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ModelContextCompressionOutcome {
    pub(crate) text: String,
    pub(crate) applied: bool,
    pub(crate) original_tokens: usize,
    pub(crate) compressed_tokens: usize,
    pub(crate) reason: Option<String>,
}

impl ModelContextCompressionOutcome {
    fn skipped(input: &str, original_tokens: usize, reason: impl Into<String>) -> Self {
        Self {
            text: input.to_string(),
            applied: false,
            original_tokens,
            compressed_tokens: original_tokens,
            reason: Some(reason.into()),
        }
    }
}

pub(crate) fn compact_text_with_config(
    input: &str,
    config: &ContextCompressionConfig,
    target: ContextCompressionTarget,
) -> String {
    compact_model_context(input, config, target).text
}

pub(crate) fn compact_model_context(
    input: &str,
    config: &ContextCompressionConfig,
    target: ContextCompressionTarget,
) -> ModelContextCompressionOutcome {
    let original_tokens = estimate_tokens(input);
    if !config.enabled {
        return ModelContextCompressionOutcome::skipped(input, original_tokens, "disabled");
    }
    if input.trim().is_empty() {
        return ModelContextCompressionOutcome::skipped(input, original_tokens, "empty");
    }
    if !target_allowed(config, target) {
        return ModelContextCompressionOutcome::skipped(
            input,
            original_tokens,
            "target_not_allowed",
        );
    }

    let candidate = match config.mode {
        ContextCompressionMode::DeterministicTerse => Ok(compact_deterministic_terse(input)),
        ContextCompressionMode::Headroom => {
            if original_tokens < config.min_tokens {
                return ModelContextCompressionOutcome::skipped(
                    input,
                    original_tokens,
                    "below_min_tokens",
                );
            }
            run_headroom_command(input, &config.headroom)
        }
    };

    let candidate = match candidate {
        Ok(candidate) => candidate,
        Err(reason) => {
            return ModelContextCompressionOutcome::skipped(input, original_tokens, reason);
        }
    };
    if candidate.trim().is_empty() {
        return ModelContextCompressionOutcome::skipped(input, original_tokens, "empty_output");
    }

    let compressed_tokens = estimate_tokens(&candidate);
    if compressed_tokens >= original_tokens {
        return ModelContextCompressionOutcome::skipped(input, original_tokens, "not_smaller");
    }

    ModelContextCompressionOutcome {
        text: candidate,
        applied: true,
        original_tokens,
        compressed_tokens,
        reason: None,
    }
}

pub(crate) fn compact_model_context_with_notice(
    input: &str,
    config: &ContextCompressionConfig,
    target: ContextCompressionTarget,
    original_ref: &str,
) -> String {
    let outcome = compact_model_context(input, config, target);
    if !outcome.applied {
        return outcome.text;
    }

    format!(
        "[Brehon context compression: mode={} target={} approx_tokens={}->{}; original retained in {}]\n{}",
        compression_mode_label(config.mode),
        compression_target_label(target),
        outcome.original_tokens,
        outcome.compressed_tokens,
        original_ref,
        outcome.text.trim()
    )
}

fn target_allowed(config: &ContextCompressionConfig, target: ContextCompressionTarget) -> bool {
    if config.never_compress.contains(&target) {
        return false;
    }

    match target {
        ContextCompressionTarget::Memory => config.compact_memories,
        ContextCompressionTarget::Rule => config.compact_rules,
        ContextCompressionTarget::TaskContext => config.compact_tasks,
        ContextCompressionTarget::ReviewHandoff
        | ContextCompressionTarget::ReviewResearch
        | ContextCompressionTarget::ResearchHandoff => config.prompt_contexts.contains(&target),
    }
}

fn estimate_tokens(input: &str) -> usize {
    let chars = input.chars().count();
    chars.div_ceil(4)
}

fn run_headroom_command(input: &str, config: &HeadroomCompressionConfig) -> Result<String, String> {
    let command = config.command.trim();
    if command.is_empty() {
        return Err("headroom_command_empty".to_string());
    }

    let mut child = Command::new(command)
        .args(&config.args)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|err| format!("headroom_spawn_failed: {err}"))?;

    let mut stdout = child
        .stdout
        .take()
        .ok_or_else(|| "headroom_stdout_unavailable".to_string())?;
    let mut stderr = child
        .stderr
        .take()
        .ok_or_else(|| "headroom_stderr_unavailable".to_string())?;
    let stdout_handle = thread::spawn(move || {
        let mut bytes = Vec::new();
        stdout.read_to_end(&mut bytes).map(|_| bytes)
    });
    let stderr_handle = thread::spawn(move || {
        let mut bytes = Vec::new();
        stderr.read_to_end(&mut bytes).map(|_| bytes)
    });

    if let Some(mut stdin) = child.stdin.take() {
        if let Err(err) = stdin.write_all(input.as_bytes()) {
            let _ = child.kill();
            let _ = child.wait();
            return Err(format!("headroom_stdin_failed: {err}"));
        }
    }

    let timeout = Duration::from_millis(config.timeout_ms.max(1));
    let start = Instant::now();
    let status = loop {
        match child.try_wait() {
            Ok(Some(status)) => break status,
            Ok(None) if start.elapsed() >= timeout => {
                let _ = child.kill();
                let _ = child.wait();
                return Err("headroom_timeout".to_string());
            }
            Ok(None) => thread::sleep(Duration::from_millis(10)),
            Err(err) => {
                let _ = child.kill();
                let _ = child.wait();
                return Err(format!("headroom_wait_failed: {err}"));
            }
        }
    };

    let stdout = stdout_handle
        .join()
        .map_err(|_| "headroom_stdout_thread_failed".to_string())?
        .map_err(|err| format!("headroom_stdout_failed: {err}"))?;
    let stderr = stderr_handle
        .join()
        .map_err(|_| "headroom_stderr_thread_failed".to_string())?
        .map_err(|err| format!("headroom_stderr_failed: {err}"))?;

    if !status.success() {
        return Err(format!(
            "headroom_exit_failed: status={status} stderr={}",
            truncate_snippet(&String::from_utf8_lossy(&stderr), 240)
        ));
    }

    String::from_utf8(stdout).map_err(|err| format!("headroom_utf8_failed: {err}"))
}

fn compression_mode_label(mode: ContextCompressionMode) -> &'static str {
    match mode {
        ContextCompressionMode::DeterministicTerse => "deterministic_terse",
        ContextCompressionMode::Headroom => "headroom",
    }
}

fn compression_target_label(target: ContextCompressionTarget) -> &'static str {
    match target {
        ContextCompressionTarget::Memory => "memory",
        ContextCompressionTarget::Rule => "rule",
        ContextCompressionTarget::TaskContext => "task_context",
        ContextCompressionTarget::ReviewHandoff => "review_handoff",
        ContextCompressionTarget::ReviewResearch => "review_research",
        ContextCompressionTarget::ResearchHandoff => "research_handoff",
    }
}

pub(crate) fn truncate_snippet(content: &str, max_chars: usize) -> String {
    let mut chars = content.chars();
    let truncated: String = chars.by_ref().take(max_chars).collect();
    if chars.next().is_some() {
        format!("{truncated}...")
    } else {
        truncated
    }
}

fn compact_inline_prose(line: &str) -> String {
    let mut result = String::with_capacity(line.len());
    let mut inline_code = false;
    let mut prose = String::new();

    for ch in line.chars() {
        if ch == '`' {
            if !prose.is_empty() {
                result.push_str(&compact_plain_segment(&prose));
                prose.clear();
            }
            inline_code = !inline_code;
            result.push(ch);
        } else if inline_code {
            result.push(ch);
        } else {
            prose.push(ch);
        }
    }

    if !prose.is_empty() {
        result.push_str(&compact_plain_segment(&prose));
    }

    result
}

fn compact_plain_segment(segment: &str) -> String {
    let leading_space = segment.chars().next().is_some_and(char::is_whitespace);
    let trailing_space = segment.chars().last().is_some_and(char::is_whitespace);
    let mut result = String::with_capacity(segment.len());
    let mut word = String::new();

    for ch in segment.chars() {
        if ch.is_alphanumeric() || ch == '\'' {
            word.push(ch);
        } else {
            flush_word(&mut result, &mut word);
            result.push(ch);
        }
    }
    flush_word(&mut result, &mut word);

    let cleaned = cleanup_spacing(&result);
    match (leading_space, trailing_space, cleaned.is_empty()) {
        (_, _, true) => String::new(),
        (true, true, false) => format!(" {cleaned} "),
        (true, false, false) => format!(" {cleaned}"),
        (false, true, false) => format!("{cleaned} "),
        (false, false, false) => cleaned,
    }
}

fn flush_word(result: &mut String, word: &mut String) {
    if word.is_empty() {
        return;
    }

    if let Some(compacted) = compact_word(word) {
        result.push_str(&compacted);
    }
    word.clear();
}

fn compact_word(word: &str) -> Option<String> {
    let lower = word.to_ascii_lowercase();
    if is_drop_word(&lower) {
        return None;
    }

    if protect_word(word) {
        return Some(word.to_string());
    }

    let replacement = match lower.as_str() {
        "approximately" => "approx",
        "authentication" => "auth",
        "authorization" => "authz",
        "configuration" => "config",
        "database" => "db",
        "directory" => "dir",
        "environment" => "env",
        "implementation" => "impl",
        "initialize" | "initialise" => "init",
        "initialization" | "initialisation" => "init",
        "middleware" => "mw",
        "repository" => "repo",
        "request" => "req",
        "response" => "resp",
        "synchronization" | "synchronisation" => "sync",
        "temporary" => "temp",
        "verification" => "verify",
        "because" => "bc",
        "before" => "pre",
        "without" => "w/o",
        "within" => "in",
        "cannot" => "can't",
        _ => return Some(word.to_string()),
    };

    Some(replacement.to_string())
}

fn protect_word(word: &str) -> bool {
    word.chars()
        .any(|ch| ch == '_' || ch == '-' || ch == '/' || ch == '.' || ch == ':')
        || word.chars().any(|ch| ch.is_ascii_uppercase())
        || word.chars().any(|ch| ch.is_ascii_digit())
}

fn is_drop_word(lower: &str) -> bool {
    matches!(
        lower,
        "a" | "an"
            | "the"
            | "very"
            | "really"
            | "basically"
            | "simply"
            | "just"
            | "probably"
            | "likely"
    )
}

fn cleanup_spacing(input: &str) -> String {
    let mut output = String::with_capacity(input.len());
    let mut prev_space = false;

    for ch in input.chars() {
        if ch.is_whitespace() {
            if !prev_space {
                output.push(' ');
                prev_space = true;
            }
            continue;
        }

        if matches!(ch, '.' | ',' | ';' | ':' | ')' | ']' | '}') && output.ends_with(' ') {
            output.pop();
        }
        output.push(ch);
        prev_space = false;
    }

    output.trim().to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn compacts_prose_but_preserves_code_and_paths() {
        let input = "The auth middleware uses `/tmp/app_config.yaml` because Result<T, E> must stay.\n```rust\nlet authentication = true;\n```";
        let compact = compact_deterministic_terse(input);

        assert!(compact.contains("auth mw uses `/tmp/app_config.yaml` bc Result<T, E> must stay."));
        assert!(compact.contains("let authentication = true;"));
    }

    #[test]
    fn limits_search_counts_to_configured_bounds() {
        let options = ContextToolOptions {
            retrieval: ContextRetrievalConfig {
                default_limit: 5,
                max_limit: 20,
                snippet_chars: 240,
            },
            compression: ContextCompressionConfig::default(),
            max_memories: 10_000,
        };

        assert_eq!(options.search_limit(None), 5);
        assert_eq!(options.search_limit(Some(0)), 1);
        assert_eq!(options.search_limit(Some(200)), 20);
    }

    #[test]
    fn default_context_options_do_not_compact() {
        let options = ContextToolOptions::default();

        assert!(!options.should_compact_memories());
        assert!(!options.should_compact_rules());
        assert!(!options.should_compact_tasks());
    }

    #[test]
    fn prompt_contexts_are_allow_listed() {
        let input = "The authentication middleware uses configuration because context matters.";
        let config = ContextCompressionConfig {
            enabled: true,
            prompt_contexts: Vec::new(),
            ..ContextCompressionConfig::default()
        };

        let outcome =
            compact_model_context(input, &config, ContextCompressionTarget::ReviewHandoff);

        assert!(!outcome.applied);
        assert_eq!(outcome.text, input);
        assert_eq!(outcome.reason.as_deref(), Some("target_not_allowed"));
    }

    #[test]
    fn never_compress_wins_over_prompt_allow_list() {
        let input = "The authentication middleware uses configuration because context matters.";
        let config = ContextCompressionConfig {
            enabled: true,
            prompt_contexts: vec![ContextCompressionTarget::ReviewHandoff],
            never_compress: vec![ContextCompressionTarget::ReviewHandoff],
            ..ContextCompressionConfig::default()
        };

        let outcome =
            compact_model_context(input, &config, ContextCompressionTarget::ReviewHandoff);

        assert!(!outcome.applied);
        assert_eq!(outcome.text, input);
        assert_eq!(outcome.reason.as_deref(), Some("target_not_allowed"));
    }

    #[test]
    fn headroom_mode_fails_closed_when_command_is_unavailable() {
        let input = "repeated verbose context ".repeat(200);
        let config = ContextCompressionConfig {
            enabled: true,
            mode: ContextCompressionMode::Headroom,
            min_tokens: 1,
            prompt_contexts: vec![ContextCompressionTarget::ReviewHandoff],
            headroom: HeadroomCompressionConfig {
                command: "/no/such/headroom".to_string(),
                args: Vec::new(),
                timeout_ms: 100,
            },
            ..ContextCompressionConfig::default()
        };

        let outcome =
            compact_model_context(&input, &config, ContextCompressionTarget::ReviewHandoff);

        assert!(!outcome.applied);
        assert_eq!(outcome.text, input);
        assert!(outcome
            .reason
            .as_deref()
            .is_some_and(|reason| reason.starts_with("headroom_spawn_failed")));
    }

    #[test]
    #[cfg(unix)]
    fn headroom_mode_applies_external_command_and_keeps_original_notice() {
        use std::fs;
        use std::os::unix::fs::PermissionsExt;

        let temp = tempfile::tempdir().unwrap();
        let script = temp.path().join("headroom-test");
        fs::write(
            &script,
            "#!/bin/sh\n/bin/cat >/dev/null\nprintf '%s\\n' x\n",
        )
        .unwrap();
        let mut permissions = fs::metadata(&script).unwrap().permissions();
        permissions.set_mode(0o755);
        fs::set_permissions(&script, permissions).unwrap();

        let input = "repeated verbose context ".repeat(200);
        let config = ContextCompressionConfig {
            enabled: true,
            mode: ContextCompressionMode::Headroom,
            min_tokens: 1,
            prompt_contexts: vec![ContextCompressionTarget::ReviewHandoff],
            headroom: HeadroomCompressionConfig {
                command: script.to_string_lossy().into_owned(),
                args: Vec::new(),
                timeout_ms: 10_000,
            },
            ..ContextCompressionConfig::default()
        };

        let outcome =
            compact_model_context(&input, &config, ContextCompressionTarget::ReviewHandoff);
        assert!(outcome.applied, "compression was not applied: {outcome:?}");
        assert!(outcome.compressed_tokens < outcome.original_tokens);
        assert!(!outcome.text.contains("repeated verbose context"));

        let with_notice = compact_model_context_with_notice(
            &input,
            &config,
            ContextCompressionTarget::ReviewHandoff,
            "round request metadata field `context`",
        );
        assert!(with_notice.contains("mode=headroom target=review_handoff"));
        assert!(with_notice.contains("original retained in round request metadata field `context`"));
    }
}
