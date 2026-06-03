//! Go build and vet runner for Layer 1.
//!
//! Parses single-line `go build` and `go vet` diagnostics that match
//! the `path:line:col: message` format.  Multi-line diagnostics
//! (indented continuation lines such as `\thave (int)\n\twant ()`)
//! are not buffered; only the first line is captured.

use std::path::Path;

use async_trait::async_trait;
use tokio::process::Command;
use tokio::time::{timeout, Duration};

use crate::findings::{Finding, Severity};
use crate::runner::{Runner, RunnerError, RunnerInputs, RunnerMetadata, RunnerOutcome};

/// Default wall-clock timeout for `go build` / `go vet`.
const COMMAND_TIMEOUT: Duration = Duration::from_secs(300);

/// Runs `go build ./...` and `go vet ./...` in a Go module and
/// parses compiler / vet diagnostics into structured findings.
///
/// Findings are truncated to the top-N errors to avoid flooding the
/// report when a large workspace fails to compile.
#[derive(Debug, Clone)]
pub struct GoBuildRunner {
    /// Default maximum number of error lines to convert into findings.
    pub max_errors: usize,
}

impl GoBuildRunner {
    /// Create a runner with the default error limit (20).
    pub fn new() -> Self {
        Self { max_errors: 20 }
    }

    /// Create a runner with a custom default error limit.
    pub fn with_max_errors(max_errors: usize) -> Self {
        Self { max_errors }
    }
}

impl Default for GoBuildRunner {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait]
impl Runner for GoBuildRunner {
    fn name(&self) -> &str {
        "go-build"
    }

    async fn run(
        &self,
        workdir: &Path,
        inputs: &RunnerInputs,
    ) -> Result<RunnerOutcome, RunnerError> {
        let max_errors = inputs
            .config
            .get("max_errors")
            .and_then(|v| v.as_u64().map(|n| n as usize))
            .unwrap_or(self.max_errors);

        let mut findings = Vec::new();

        // 1. go build ./...
        let build_success = match run_go_command(workdir, "build").await {
            Ok((success, output)) => {
                if !success {
                    findings.extend(handle_command_result(
                        success,
                        &output,
                        "build",
                        "go-build",
                        max_errors,
                        Severity::Blocking,
                    ));
                }
                success
            }
            Err(e) => {
                findings.push(Finding::new(
                    format!("Failed to run `go build ./...`: {e}"),
                    "go-build",
                    Severity::Blocking,
                ));
                false
            }
        };

        // 2. go vet ./... — only if build succeeded (vet needs compilable code)
        if build_success {
            match run_go_command(workdir, "vet").await {
                Ok((success, output)) => {
                    if !success {
                        findings.extend(handle_command_result(
                            success,
                            &output,
                            "vet",
                            "go-vet",
                            max_errors,
                            Severity::Suggestion,
                        ));
                    }
                }
                Err(e) => {
                    findings.push(Finding::new(
                        format!("Failed to run `go vet ./...`: {e}"),
                        "go-vet",
                        Severity::Suggestion,
                    ));
                }
            }
        }

        let metadata = RunnerMetadata {
            runner_name: self.name().to_string(),
            duration_ms: None,
        };

        Ok(RunnerOutcome::new(findings, metadata))
    }
}

/// Convert a `go` sub-command result into findings.
///
/// When the command exits non-zero, attempt to parse structured
/// diagnostics.  If parsing yields nothing, emit a single fallback
/// finding — even when the command produced no output.
fn handle_command_result(
    success: bool,
    output: &str,
    subcommand: &str,
    layer: &str,
    max_errors: usize,
    severity: Severity,
) -> Vec<Finding> {
    let mut findings = Vec::new();
    if !success {
        let parsed = parse_go_errors(output, layer, max_errors, severity);
        if parsed.is_empty() {
            let msg = if output.trim().is_empty() {
                format!("`go {subcommand} ./...` exited with non-zero status")
            } else {
                format!(
                    "`go {subcommand} ./...` failed: {}",
                    truncate_raw(output, 500)
                )
            };
            findings.push(Finding::new(msg, layer, severity));
        } else {
            findings.extend(parsed);
        }
    }
    findings
}

/// Errors that can occur while running a `go` sub-command.
#[derive(Debug)]
enum GoCommandError {
    Io(std::io::Error),
    TimedOut,
}

impl std::fmt::Display for GoCommandError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            GoCommandError::Io(e) => write!(f, "I/O error: {e}"),
            GoCommandError::TimedOut => {
                write!(f, "command timed out after {}s", COMMAND_TIMEOUT.as_secs())
            }
        }
    }
}

impl std::error::Error for GoCommandError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            GoCommandError::Io(e) => Some(e),
            GoCommandError::TimedOut => None,
        }
    }
}

/// Spawn `go <subcommand> ./...` in `workdir` and return
/// `(exit_success, combined_output)`.
///
/// Stderr and stdout are merged because `go` prints diagnostics to
/// stderr but some tool wrappers (e.g. `go vet` in module mode) may
/// also write to stdout.
///
/// The child process is killed if the future is dropped (e.g. on
/// timeout) so runaway processes cannot leak.
async fn run_go_command(
    workdir: &Path,
    subcommand: &str,
) -> Result<(bool, String), GoCommandError> {
    let future = Command::new("go")
        .args([subcommand, "./..."])
        .current_dir(workdir)
        .kill_on_drop(true)
        .output();

    let output = timeout(COMMAND_TIMEOUT, future)
        .await
        .map_err(|_| GoCommandError::TimedOut)?
        .map_err(GoCommandError::Io)?;

    let mut combined = String::from_utf8_lossy(&output.stderr).into_owned();
    let stdout = String::from_utf8_lossy(&output.stdout);
    if !stdout.is_empty() {
        if !combined.is_empty() {
            combined.push('\n');
        }
        combined.push_str(&stdout);
    }

    Ok((output.status.success(), combined))
}

/// Truncate raw command output to a reasonable length for a finding
/// description.
///
/// Truncation is safe for multi-byte UTF-8 characters.
fn truncate_raw(s: &str, max_len: usize) -> String {
    let s = s.trim();
    if s.len() <= max_len {
        s.to_string()
    } else {
        let end = s
            .char_indices()
            .take_while(|(i, _)| *i <= max_len)
            .last()
            .map(|(i, c)| i + c.len_utf8())
            .unwrap_or(0);
        format!("{}...", &s[..end])
    }
}

/// Parse Go compiler / vet output into structured findings.
///
/// Skips empty lines and package headers (`# pkg`).  Strips an
/// optional `vet: ` prefix.  Limits output to `max_errors` findings.
fn parse_go_errors(
    output: &str,
    layer: &str,
    max_errors: usize,
    severity: Severity,
) -> Vec<Finding> {
    let mut findings = Vec::new();

    for line in output.lines() {
        if findings.len() >= max_errors {
            break;
        }

        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }

        // Strip optional "vet: " prefix
        let line = if let Some(rest) = line.strip_prefix("vet: ") {
            rest
        } else {
            line
        };

        if let Some(finding) = parse_go_error_line(line, layer, severity) {
            findings.push(finding);
        }
    }

    findings
}

/// Parse a single line of Go diagnostic output.
///
/// Expected formats:
/// - `path/to/file.go:123:45: message`
/// - `path/to/file.go:123: message`
/// - `./file.go:123:45: message`
fn parse_go_error_line(line: &str, layer: &str, severity: Severity) -> Option<Finding> {
    let first_colon = line.find(':')?;
    let file = &line[..first_colon];

    // Heuristic: a valid file path contains '/' or '.' (for .go)
    if !file.contains('/') && !file.contains('.') {
        return None;
    }

    let after_first = &line[first_colon + 1..];
    let second_colon = after_first.find(':')?;
    let line_str = &after_first[..second_colon];
    let line_num: u32 = line_str.parse().ok()?;

    let after_second = &after_first[second_colon + 1..];

    // Distinguish column number from message:
    // `path:line:col: message`  vs  `path:line: message`
    let message = if let Some(third_colon) = after_second.find(':') {
        let maybe_col = &after_second[..third_colon];
        if maybe_col.trim().parse::<u32>().is_ok() {
            after_second[third_colon + 1..].trim()
        } else {
            after_second.trim()
        }
    } else {
        after_second.trim()
    };

    if message.is_empty() {
        return None;
    }

    Some(Finding::new(message, layer, severity).with_location(file, line_num))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    fn write_go_mod(dir: &tempfile::TempDir, module: &str) {
        let path = dir.path().join("go.mod");
        let mut f = std::fs::File::create(&path).unwrap();
        writeln!(f, "module {module}\n\ngo 1.21\n").unwrap();
    }

    #[tokio::test]
    async fn compile_error_yields_single_finding_with_file_and_line() {
        let dir = tempfile::tempdir().unwrap();
        write_go_mod(&dir, "testcompile");

        let main_go = dir.path().join("main.go");
        std::fs::write(
            &main_go,
            "package main\n\nfunc main() {\n\tvar x int = \"hello\"\n\t_ = x\n}\n",
        )
        .unwrap();

        let runner = GoBuildRunner::new();
        let outcome = runner
            .run(dir.path(), &RunnerInputs::default())
            .await
            .unwrap();

        assert_eq!(
            outcome.findings.len(),
            1,
            "expected one finding, got: {:?}",
            outcome.findings
        );
        let finding = &outcome.findings[0];
        assert_eq!(finding.file.as_deref(), Some("./main.go"));
        assert_eq!(finding.line, Some(4));
        assert!(finding.description.contains("cannot use"));
        assert_eq!(finding.layer, "go-build");
        assert_eq!(finding.severity, Severity::Blocking);
    }

    #[tokio::test]
    async fn clean_project_yields_no_findings() {
        let dir = tempfile::tempdir().unwrap();
        write_go_mod(&dir, "testclean");

        let main_go = dir.path().join("main.go");
        std::fs::write(
            &main_go,
            "package main\n\nimport \"fmt\"\n\nfunc main() {\n\tfmt.Println(\"hello\")\n}\n",
        )
        .unwrap();

        let runner = GoBuildRunner::new();
        let outcome = runner
            .run(dir.path(), &RunnerInputs::default())
            .await
            .unwrap();

        assert!(
            outcome.findings.is_empty(),
            "expected no findings, got: {:?}",
            outcome.findings
        );
    }

    #[tokio::test]
    async fn vet_errors_are_captured_as_findings() {
        let dir = tempfile::tempdir().unwrap();
        write_go_mod(&dir, "testvet");

        let main_go = dir.path().join("main.go");
        std::fs::write(
            &main_go,
            "package main\n\nimport \"fmt\"\n\nfunc main() {\n\tfmt.Printf(\"%s\", 42)\n}\n",
        )
        .unwrap();

        let runner = GoBuildRunner::new();
        let outcome = runner
            .run(dir.path(), &RunnerInputs::default())
            .await
            .unwrap();

        assert!(!outcome.findings.is_empty(), "expected vet findings");
        let finding = &outcome.findings[0];
        assert_eq!(finding.file.as_deref(), Some("./main.go"));
        assert_eq!(finding.line, Some(6));
        assert!(finding.description.contains("Printf"));
        assert_eq!(finding.layer, "go-vet");
        assert_eq!(finding.severity, Severity::Suggestion);
    }

    #[tokio::test]
    async fn multiple_errors_are_truncated_to_top_n() {
        let dir = tempfile::tempdir().unwrap();
        write_go_mod(&dir, "testtruncate");

        let main_go = dir.path().join("main.go");
        std::fs::write(
            &main_go,
            "package main\n\nfunc main() {\n\tvar a int = \"hello\"\n\tvar b string = 42\n\tundefinedFunc()\n\t_ = a\n\t_ = b\n}\n",
        )
        .unwrap();

        let runner = GoBuildRunner::with_max_errors(2);
        let outcome = runner
            .run(dir.path(), &RunnerInputs::default())
            .await
            .unwrap();

        assert_eq!(
            outcome.findings.len(),
            2,
            "expected exactly 2 findings due to truncation"
        );
    }

    #[tokio::test]
    async fn inputs_can_override_max_errors() {
        let dir = tempfile::tempdir().unwrap();
        write_go_mod(&dir, "testinputs");

        let main_go = dir.path().join("main.go");
        std::fs::write(
            &main_go,
            "package main\n\nfunc main() {\n\tvar a int = \"hello\"\n\tvar b string = 42\n\tundefinedFunc()\n\t_ = a\n\t_ = b\n}\n",
        )
        .unwrap();

        let mut inputs = RunnerInputs::default();
        inputs.config.insert(
            "max_errors".to_string(),
            serde_json::json!(1),
        );
        let runner = GoBuildRunner::with_max_errors(100);
        let outcome = runner.run(dir.path(), &inputs).await.unwrap();

        assert_eq!(
            outcome.findings.len(),
            1,
            "expected exactly 1 finding because inputs override runner default"
        );
    }

    #[tokio::test]
    async fn mixed_package_failure_emits_fallback_finding() {
        let dir = tempfile::tempdir().unwrap();
        write_go_mod(&dir, "testmixed");

        std::fs::write(
            dir.path().join("main.go"),
            "package main\n\nfunc main() {}\n",
        )
        .unwrap();
        std::fs::write(
            dir.path().join("other.go"),
            "package other\n",
        )
        .unwrap();

        let runner = GoBuildRunner::new();
        let outcome = runner
            .run(dir.path(), &RunnerInputs::default())
            .await
            .unwrap();

        assert!(
            !outcome.findings.is_empty(),
            "expected a fallback finding for unparsable build error, got: {:?}",
            outcome.findings
        );
        let finding = &outcome.findings[0];
        assert_eq!(finding.layer, "go-build");
        assert_eq!(finding.severity, Severity::Blocking);
        assert!(finding.description.contains("go build"));
        assert!(
            finding.description.contains("found packages")
                || finding.description.contains("main")
                || finding.description.contains("other"),
            "fallback should contain raw output: {}",
            finding.description
        );
    }

    #[tokio::test]
    async fn build_failure_skips_vet() {
        let dir = tempfile::tempdir().unwrap();
        write_go_mod(&dir, "testskipvet");

        std::fs::write(
            dir.path().join("main.go"),
            "package main\n\nfunc main() {\n\tundefinedFunc()\n}\n",
        )
        .unwrap();

        let runner = GoBuildRunner::new();
        let outcome = runner
            .run(dir.path(), &RunnerInputs::default())
            .await
            .unwrap();

        assert!(
            outcome.findings.iter().all(|f| f.layer == "go-build"),
            "vet should be skipped when build fails, got: {:?}",
            outcome.findings
        );
    }

    #[test]
    fn handle_command_result_parses_structured_errors() {
        let output = "./main.go:10:5: undefined: foo\n./main.go:20:3: bar";
        let findings =
            handle_command_result(false, output, "build", "go-build", 10, Severity::Blocking);
        assert_eq!(findings.len(), 2);
        assert_eq!(findings[0].description, "undefined: foo");
        assert_eq!(findings[1].description, "bar");
    }

    #[test]
    fn handle_command_result_fallback_on_unparsable_output() {
        let output = "found packages main and other";
        let findings =
            handle_command_result(false, output, "build", "go-build", 10, Severity::Blocking);
        assert_eq!(findings.len(), 1);
        assert_eq!(findings[0].layer, "go-build");
        assert_eq!(findings[0].severity, Severity::Blocking);
        assert!(findings[0].description.contains("go build"));
        assert!(findings[0].description.contains("found packages"));
    }

    #[test]
    fn handle_command_result_fallback_on_empty_output() {
        let findings =
            handle_command_result(false, "", "build", "go-build", 10, Severity::Blocking);
        assert_eq!(findings.len(), 1);
        assert_eq!(findings[0].layer, "go-build");
        assert_eq!(findings[0].severity, Severity::Blocking);
        assert!(findings[0].description.contains("exited with non-zero status"));
    }

    #[test]
    fn handle_command_result_vet_fallback_on_empty_output() {
        let findings =
            handle_command_result(false, "", "vet", "go-vet", 10, Severity::Suggestion);
        assert_eq!(findings.len(), 1);
        assert_eq!(findings[0].layer, "go-vet");
        assert_eq!(findings[0].severity, Severity::Suggestion);
        assert!(findings[0].description.contains("exited with non-zero status"));
    }

    #[test]
    fn handle_command_result_success_yields_nothing() {
        let findings =
            handle_command_result(true, "", "build", "go-build", 10, Severity::Blocking);
        assert!(findings.is_empty());
    }

    #[test]
    fn parse_go_error_line_handles_col_and_no_col() {
        // With column
        let f =
            parse_go_error_line("./main.go:10:5: undefined: foo", "go-build", Severity::Blocking)
                .unwrap();
        assert_eq!(f.file.as_deref(), Some("./main.go"));
        assert_eq!(f.line, Some(10));
        assert_eq!(f.description, "undefined: foo");

        // Without column
        let f = parse_go_error_line("./main.go:10: syntax error", "go-build", Severity::Blocking)
            .unwrap();
        assert_eq!(f.file.as_deref(), Some("./main.go"));
        assert_eq!(f.line, Some(10));
        assert_eq!(f.description, "syntax error");
    }

    #[test]
    fn parse_go_error_line_skips_invalid_lines() {
        assert!(parse_go_error_line("", "go-build", Severity::Blocking).is_none());
        assert!(
            parse_go_error_line("not an error line", "go-build", Severity::Blocking).is_none()
        );
    }

    #[test]
    fn parse_go_errors_strips_vet_prefix() {
        let output = "vet: ./main.go:10:5: some message";
        let findings = parse_go_errors(output, "go-vet", 10, Severity::Suggestion);
        assert_eq!(findings.len(), 1);
        assert_eq!(findings[0].file.as_deref(), Some("./main.go"));
        assert_eq!(findings[0].line, Some(10));
        assert_eq!(findings[0].description, "some message");
        assert_eq!(findings[0].severity, Severity::Suggestion);
    }

    #[test]
    fn parse_go_errors_skips_package_headers() {
        let output = "# example.com/pkg\n./main.go:10:5: error here";
        let findings = parse_go_errors(output, "go-build", 10, Severity::Blocking);
        assert_eq!(findings.len(), 1);
        assert_eq!(findings[0].description, "error here");
    }

    #[test]
    fn parse_go_errors_respects_max_errors() {
        let output = "./a.go:1:1: err1\n./b.go:2:2: err2\n./c.go:3:3: err3";
        let findings = parse_go_errors(output, "go-build", 2, Severity::Blocking);
        assert_eq!(findings.len(), 2);
    }

    #[test]
    fn truncate_raw_does_not_panic_on_multibyte_utf8() {
        let s = "αβγδε"; // each greek letter is 2 bytes
        // max_len=3 would split the second character (β at bytes 2-3)
        let result = truncate_raw(s, 3);
        assert!(result.ends_with("..."));
        assert!(!result.is_empty());
        // Should not panic — that's the main assertion
    }

    #[test]
    fn truncate_raw_short_string_unchanged() {
        let s = "hello";
        assert_eq!(truncate_raw(s, 10), "hello");
    }
}
