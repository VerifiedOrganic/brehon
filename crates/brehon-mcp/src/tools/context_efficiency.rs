//! Shared token-efficiency helpers for MCP context tools.

use brehon_types::config::{
    ContextCompressionConfig, ContextCompressionMode, ContextRetrievalConfig,
};

use crate::server::configured_project_root;

/// Effective context retrieval/compression settings.
#[derive(Debug, Clone, Copy)]
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

pub(crate) fn compact_text_if_enabled(
    input: &str,
    enabled: bool,
    mode: ContextCompressionMode,
) -> String {
    if !enabled {
        return input.to_string();
    }

    match mode {
        ContextCompressionMode::DeterministicTerse => compact_deterministic_terse(input),
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
}
