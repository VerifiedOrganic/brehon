//! Activity stream for gateway-backed ACP panes.
//!
//! Provides structured activity tracking with:
//! - Bounded retention (configurable max entries)
//! - Active tool call tracking by tool_id
//! - Output chunk coalescing
//! - Ingestion timestamps for duration/ordering

use std::collections::{HashMap, VecDeque};
use std::time::Instant;

use super::types::Pane;

/// Default maximum number of entries retained in an `ActivityBuffer`.
pub const DEFAULT_MAX_ENTRIES: usize = 500;

/// Kind of activity event in the structured stream.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ActivityKind {
    /// An operation boundary (turn start/end).
    Operation,
    /// A permission request from the agent.
    Permission,
    /// Progress update (e.g., percentage or status message).
    Progress,
    /// A tool call start or completion.
    ToolCall,
    /// Streamed text output from the agent.
    Output,
}

/// A single entry in the structured activity stream.
#[derive(Debug, Clone)]
pub struct ActivityEntry {
    /// What kind of activity this entry represents.
    pub kind: ActivityKind,
    /// When this entry was ingested into the buffer.
    pub ingested_at: Instant,
    /// Tool call identifier, when applicable.
    pub tool_id: Option<String>,
    /// Tool name, when applicable.
    pub tool_name: Option<String>,
    /// Status string (e.g., "started", "completed", "failed").
    pub status: Option<String>,
    /// Human-readable message or description.
    pub message: Option<String>,
    /// Coalesced output text chunks for `Output` entries.
    pub output_chunks: Option<Vec<String>>,
    /// Duration of a completed tool call (set when ToolCallCompleted fires).
    pub duration: Option<std::time::Duration>,
}

/// An in-flight tool call being tracked by the activity buffer.
#[derive(Debug, Clone)]
pub struct ActiveToolCall {
    /// Unique identifier for this tool invocation.
    pub tool_id: String,
    /// Name of the tool being executed.
    pub tool_name: String,
    /// When this tool call started.
    pub started_at: Instant,
}

/// Bounded ring buffer of structured activity entries for a gateway-backed pane.
///
/// Tracks active tool calls, coalesces streamed output, and enforces retention limits.
pub struct ActivityBuffer {
    entries: VecDeque<ActivityEntry>,
    active_tools: HashMap<String, ActiveToolCall>,
    output_buffer: Option<String>,
    output_buffer_started_at: Option<Instant>,
    max_entries: usize,
}

impl Default for ActivityBuffer {
    fn default() -> Self {
        Self::new(DEFAULT_MAX_ENTRIES)
    }
}

impl ActivityBuffer {
    /// Create a new buffer with the given maximum entry capacity.
    pub fn new(max_entries: usize) -> Self {
        Self {
            entries: VecDeque::with_capacity(max_entries),
            active_tools: HashMap::new(),
            output_buffer: None,
            output_buffer_started_at: None,
            max_entries,
        }
    }

    /// Push an entry, evicting the oldest if at capacity.
    pub fn push(&mut self, entry: ActivityEntry) {
        if self.entries.len() >= self.max_entries {
            self.entries.pop_front();
        }
        self.entries.push_back(entry);
    }

    /// Flush the pending output buffer into a committed `Output` entry.
    pub fn flush_output_buffer(&mut self) {
        if let Some(text) = self.output_buffer.take()
            && !text.is_empty()
        {
            let entry = ActivityEntry {
                kind: ActivityKind::Output,
                ingested_at: self.output_buffer_started_at.unwrap_or_else(Instant::now),
                tool_id: None,
                tool_name: None,
                status: None,
                message: None,
                output_chunks: Some(vec![text]),
                duration: None,
            };
            self.push(entry);
        }
        self.output_buffer_started_at = None;
    }

    /// Append text to the pending output buffer for coalescing.
    pub fn append_output(&mut self, text: &str) {
        let now = Instant::now();
        if self.output_buffer.is_none() {
            self.output_buffer_started_at = Some(now);
        }
        self.output_buffer
            .get_or_insert_with(String::new)
            .push_str(text);
    }

    /// Record the start of a tool call.
    pub fn start_tool(&mut self, tool_id: String, tool_name: String) {
        let active = ActiveToolCall {
            tool_id: tool_id.clone(),
            tool_name: tool_name.clone(),
            started_at: Instant::now(),
        };
        self.active_tools.insert(tool_id, active);
    }

    /// Mark a tool call as completed, returning its tracking data if found.
    pub fn complete_tool(&mut self, tool_id: &str) -> Option<ActiveToolCall> {
        self.active_tools.remove(tool_id)
    }

    /// Iterate over committed entries in chronological order.
    pub fn entries(&self) -> impl Iterator<Item = &ActivityEntry> {
        self.entries.iter()
    }

    /// Build a synthetic entry for buffered (not yet flushed) output, if any.
    pub fn pending_output_entry(&self) -> Option<ActivityEntry> {
        self.output_buffer.as_ref().and_then(|text| {
            if text.is_empty() {
                None
            } else {
                Some(ActivityEntry {
                    kind: ActivityKind::Output,
                    ingested_at: self.output_buffer_started_at.unwrap_or_else(Instant::now),
                    tool_id: None,
                    tool_name: None,
                    status: None,
                    message: None,
                    output_chunks: Some(vec![text.clone()]),
                    duration: None,
                })
            }
        })
    }

    /// Return the last entry (committed or pending) without cloning the
    /// entire buffer.  Returns `Cow::Borrowed` for committed entries,
    /// `Cow::Owned` only when the pending output buffer is non-empty.
    pub fn last_entry_or_pending(&self) -> Option<std::borrow::Cow<'_, ActivityEntry>> {
        if let Some(ref text) = self.output_buffer
            && !text.is_empty()
        {
            return Some(std::borrow::Cow::Owned(ActivityEntry {
                kind: ActivityKind::Output,
                ingested_at: self.output_buffer_started_at.unwrap_or_else(Instant::now),
                tool_id: None,
                tool_name: None,
                status: None,
                message: None,
                output_chunks: Some(vec![text.clone()]),
                duration: None,
            }));
        }
        self.entries.back().map(std::borrow::Cow::Borrowed)
    }

    /// Clone all entries plus any pending output into a `Vec`.
    ///
    /// **Note:** Prefer `entries()` + `pending_output_entry()` in hot paths
    /// to avoid cloning the entire buffer every frame.
    pub fn entries_with_pending(&self) -> Vec<ActivityEntry> {
        let mut result: Vec<ActivityEntry> = self.entries.iter().cloned().collect();
        if let Some(pending) = self.pending_output_entry() {
            result.push(pending);
        }
        result
    }

    /// Number of committed entries in the buffer.
    pub fn len(&self) -> usize {
        self.entries.len()
    }

    /// Returns true if there are no committed entries.
    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    /// Look up an active (in-flight) tool call by ID.
    pub fn active_tool(&self, tool_id: &str) -> Option<&ActiveToolCall> {
        self.active_tools.get(tool_id)
    }

    /// Iterate over all currently active (in-flight) tool calls.
    pub fn active_tools(&self) -> impl Iterator<Item = &ActiveToolCall> {
        self.active_tools.values()
    }

    /// Return whether any tool call is currently active (in-flight).
    pub fn has_in_flight_tools(&self) -> bool {
        !self.active_tools.is_empty()
    }

    /// Returns the most recent operation that has started and not yet completed.
    pub fn active_operation(&self) -> Option<&str> {
        for entry in self.entries.iter().rev() {
            if entry.kind != ActivityKind::Operation {
                continue;
            }
            return match entry.status.as_deref() {
                Some("started") => entry.message.as_deref(),
                Some("completed" | "failed") => None,
                _ => None,
            };
        }
        None
    }

    /// Clear all entries, active tools, and pending output.
    pub fn clear(&mut self) {
        self.entries.clear();
        self.active_tools.clear();
        self.output_buffer = None;
        self.output_buffer_started_at = None;
    }

    /// Finalize the buffer by flushing any pending output.
    pub fn finalize(&mut self) {
        self.flush_output_buffer();
    }

    /// Remove active tool calls older than `threshold` and return their IDs.
    pub fn sweep_stale(&mut self, threshold: std::time::Duration) -> Vec<String> {
        let now = Instant::now();
        let stale_ids: Vec<String> = self
            .active_tools
            .iter()
            .filter(|(_, tool)| now.duration_since(tool.started_at) > threshold)
            .map(|(id, _)| id.clone())
            .collect();

        for id in &stale_ids {
            self.active_tools.remove(id);
        }

        stale_ids
    }
}

impl Pane {
    /// Set whether a tool is currently executing in this pane.
    pub(crate) fn set_tool_executing(&mut self, executing: bool) {
        self.is_tool_executing = executing;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn make_entry(kind: ActivityKind) -> ActivityEntry {
        ActivityEntry {
            kind,
            ingested_at: Instant::now(),
            tool_id: None,
            tool_name: None,
            status: None,
            message: None,
            output_chunks: None,
            duration: None,
        }
    }

    fn make_tool_entry(kind: ActivityKind, tool_id: &str, tool_name: &str) -> ActivityEntry {
        ActivityEntry {
            kind,
            ingested_at: Instant::now(),
            tool_id: Some(tool_id.to_string()),
            tool_name: Some(tool_name.to_string()),
            status: None,
            message: None,
            output_chunks: None,
            duration: None,
        }
    }

    #[test]
    fn test_retention_eviction() {
        let mut buf = ActivityBuffer::new(3);
        buf.push(make_entry(ActivityKind::Operation));
        buf.push(make_entry(ActivityKind::Operation));
        buf.push(make_entry(ActivityKind::Operation));
        assert_eq!(buf.len(), 3);

        buf.push(make_entry(ActivityKind::Progress));
        assert_eq!(buf.len(), 3);

        let kinds: Vec<_> = buf.entries().map(|e| e.kind).collect();
        assert_eq!(
            kinds,
            vec![
                ActivityKind::Operation,
                ActivityKind::Operation,
                ActivityKind::Progress
            ]
        );
    }

    #[test]
    fn test_tool_pairing() {
        let mut buf = ActivityBuffer::new(10);

        buf.start_tool("tool-1".to_string(), "bash".to_string());
        assert!(buf.active_tool("tool-1").is_some());
        assert!(buf.active_tool("tool-2").is_none());

        let completed = buf.complete_tool("tool-1");
        assert!(completed.is_some());
        assert_eq!(completed.unwrap().tool_name, "bash");
        assert!(buf.active_tool("tool-1").is_none());
    }

    #[test]
    fn test_output_coalescing() {
        let mut buf = ActivityBuffer::new(10);

        buf.append_output("hello ");
        buf.append_output("world");

        assert!(buf.output_buffer.is_some());
        assert_eq!(buf.output_buffer.as_ref().unwrap(), "hello world");

        buf.flush_output_buffer();
        assert!(buf.output_buffer.is_none());
        assert_eq!(buf.len(), 1);

        let entry = buf.entries().next().unwrap();
        assert_eq!(entry.kind, ActivityKind::Output);
        assert_eq!(
            entry.output_chunks.as_ref().unwrap(),
            &vec!["hello world".to_string()]
        );
    }

    #[test]
    fn test_entry_ordering() {
        let mut buf = ActivityBuffer::new(10);

        buf.push(make_tool_entry(ActivityKind::ToolCall, "t1", "tool1"));
        buf.push(make_entry(ActivityKind::Progress));
        buf.push(make_tool_entry(ActivityKind::ToolCall, "t2", "tool2"));

        let entries: Vec<_> = buf.entries().collect();
        assert_eq!(entries.len(), 3);
        assert_eq!(entries[0].tool_id, Some("t1".to_string()));
        assert_eq!(entries[1].tool_id, None);
        assert_eq!(entries[2].tool_id, Some("t2".to_string()));
    }

    #[test]
    fn test_empty_output_not_pushed() {
        let mut buf = ActivityBuffer::new(10);

        buf.append_output("");
        buf.flush_output_buffer();

        assert!(buf.is_empty());
    }

    #[test]
    fn test_finalize_flushes_trailing_output() {
        let mut buf = ActivityBuffer::new(10);

        buf.append_output("trailing ");
        buf.append_output("output");

        assert!(buf.output_buffer.is_some());

        buf.finalize();

        assert!(buf.output_buffer.is_none());
        assert_eq!(buf.len(), 1);

        let entry = buf.entries().next().unwrap();
        assert_eq!(entry.kind, ActivityKind::Output);
        assert_eq!(
            entry.output_chunks.as_ref().unwrap(),
            &vec!["trailing output".to_string()]
        );
    }

    #[test]
    fn test_complete_tool_removes_any_status() {
        let mut buf = ActivityBuffer::new(10);

        buf.start_tool("tool-1".to_string(), "bash".to_string());
        assert!(buf.active_tool("tool-1").is_some());

        let completed = buf.complete_tool("tool-1");
        assert!(completed.is_some());
        assert_eq!(completed.unwrap().tool_name, "bash");
        assert!(buf.active_tool("tool-1").is_none());
    }

    #[test]
    fn test_entries_with_pending_includes_buffered_output() {
        let mut buf = ActivityBuffer::new(10);

        buf.push(make_entry(ActivityKind::Operation));
        buf.append_output("hello ");
        buf.append_output("world");

        let entries = buf.entries_with_pending();
        assert_eq!(entries.len(), 2);

        assert_eq!(entries[0].kind, ActivityKind::Operation);
        assert_eq!(entries[1].kind, ActivityKind::Output);
        assert_eq!(
            entries[1].output_chunks.as_ref().unwrap(),
            &vec!["hello world".to_string()]
        );

        assert!(buf.output_buffer.is_some());
    }

    #[test]
    fn test_entries_with_pending_empty_when_no_buffer() {
        let mut buf = ActivityBuffer::new(10);

        buf.push(make_entry(ActivityKind::Operation));

        let entries = buf.entries_with_pending();
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].kind, ActivityKind::Operation);
    }

    #[test]
    fn test_tool_pairing_normal_lifecycle() {
        let mut buf = ActivityBuffer::new(10);

        buf.start_tool("tool-1".to_string(), "bash".to_string());
        assert!(buf.active_tool("tool-1").is_some());

        buf.push(ActivityEntry {
            kind: ActivityKind::ToolCall,
            ingested_at: Instant::now(),
            tool_id: Some("tool-1".to_string()),
            tool_name: Some("bash".to_string()),
            status: Some("started".to_string()),
            message: None,
            output_chunks: None,
            duration: None,
        });

        buf.flush_output_buffer();

        buf.push(ActivityEntry {
            kind: ActivityKind::ToolCall,
            ingested_at: Instant::now(),
            tool_id: Some("tool-1".to_string()),
            tool_name: Some("bash".to_string()),
            status: Some("completed".to_string()),
            message: None,
            output_chunks: None,
            duration: None,
        });
        buf.complete_tool("tool-1");

        assert!(buf.active_tool("tool-1").is_none());
        assert_eq!(buf.len(), 2);
    }

    #[test]
    fn test_tool_pairing_orphaned_start() {
        let mut buf = ActivityBuffer::new(10);

        buf.start_tool("orphan-1".to_string(), "bash".to_string());
        assert!(buf.active_tool("orphan-1").is_some());

        buf.push(ActivityEntry {
            kind: ActivityKind::ToolCall,
            ingested_at: Instant::now(),
            tool_id: Some("orphan-1".to_string()),
            tool_name: Some("bash".to_string()),
            status: Some("started".to_string()),
            message: None,
            output_chunks: None,
            duration: None,
        });

        assert_eq!(buf.len(), 1);

        assert!(buf.active_tool("orphan-1").is_some());

        buf.clear();
        assert!(buf.active_tool("orphan-1").is_none());
    }

    #[test]
    fn test_tool_pairing_out_of_order_complete() {
        let mut buf = ActivityBuffer::new(10);

        let result = buf.complete_tool("unknown-tool");
        assert!(result.is_none());

        assert_eq!(buf.len(), 0);
    }

    #[test]
    fn test_tool_pairing_duplicate_ids() {
        let mut buf = ActivityBuffer::new(10);

        buf.start_tool("tool-1".to_string(), "bash".to_string());
        let first_start = buf.active_tool("tool-1").unwrap().started_at;

        std::thread::sleep(std::time::Duration::from_millis(50));

        buf.start_tool("tool-1".to_string(), "bash".to_string());

        let second_start = buf.active_tool("tool-1").unwrap().started_at;
        assert!(
            second_start > first_start,
            "duplicate start should overwrite with newer timestamp"
        );

        assert!(buf.active_tool("tool-1").is_some());

        buf.complete_tool("tool-1");
        assert!(buf.active_tool("tool-1").is_none());
    }

    #[test]
    fn test_tool_pairing_interleaved_tools() {
        let mut buf = ActivityBuffer::new(10);

        buf.start_tool("tool-1".to_string(), "bash".to_string());
        buf.start_tool("tool-2".to_string(), "read".to_string());

        assert!(buf.active_tool("tool-1").is_some());
        assert!(buf.active_tool("tool-2").is_some());

        buf.push(ActivityEntry {
            kind: ActivityKind::ToolCall,
            ingested_at: Instant::now(),
            tool_id: Some("tool-1".to_string()),
            tool_name: Some("bash".to_string()),
            status: Some("started".to_string()),
            message: None,
            output_chunks: None,
            duration: None,
        });
        buf.push(ActivityEntry {
            kind: ActivityKind::ToolCall,
            ingested_at: Instant::now(),
            tool_id: Some("tool-2".to_string()),
            tool_name: Some("read".to_string()),
            status: Some("started".to_string()),
            message: None,
            output_chunks: None,
            duration: None,
        });

        buf.flush_output_buffer();
        buf.push(ActivityEntry {
            kind: ActivityKind::ToolCall,
            ingested_at: Instant::now(),
            tool_id: Some("tool-2".to_string()),
            tool_name: Some("read".to_string()),
            status: Some("completed".to_string()),
            message: None,
            output_chunks: None,
            duration: None,
        });
        buf.complete_tool("tool-2");

        assert!(buf.active_tool("tool-1").is_some());
        assert!(buf.active_tool("tool-2").is_none());

        buf.flush_output_buffer();
        buf.push(ActivityEntry {
            kind: ActivityKind::ToolCall,
            ingested_at: Instant::now(),
            tool_id: Some("tool-1".to_string()),
            tool_name: Some("bash".to_string()),
            status: Some("completed".to_string()),
            message: None,
            output_chunks: None,
            duration: None,
        });
        buf.complete_tool("tool-1");

        assert!(buf.active_tool("tool-1").is_none());
        assert!(buf.active_tool("tool-2").is_none());

        assert_eq!(buf.len(), 4);

        let kinds: Vec<_> = buf.entries().map(|e| e.kind).collect();
        assert_eq!(kinds, vec![ActivityKind::ToolCall; 4]);
    }

    #[test]
    fn test_output_coalescing_single_chunk() {
        let mut buf = ActivityBuffer::new(10);

        buf.append_output("single line");
        assert!(buf.output_buffer.is_some());

        buf.flush_output_buffer();
        assert!(buf.output_buffer.is_none());
        assert_eq!(buf.len(), 1);

        let entry = buf.entries().next().unwrap();
        assert_eq!(entry.kind, ActivityKind::Output);
        assert_eq!(
            entry.output_chunks.as_ref().unwrap(),
            &vec!["single line".to_string()]
        );
    }

    #[test]
    fn test_output_coalescing_multiple_chunks() {
        let mut buf = ActivityBuffer::new(10);

        buf.append_output("line one\n");
        buf.append_output("line two\n");
        buf.append_output("line three");

        assert!(buf.output_buffer.is_some());
        assert_eq!(
            buf.output_buffer.as_ref().unwrap(),
            "line one\nline two\nline three"
        );

        buf.flush_output_buffer();
        assert_eq!(buf.len(), 1);

        let entry = buf.entries().next().unwrap();
        assert_eq!(entry.kind, ActivityKind::Output);
        assert_eq!(
            entry.output_chunks.as_ref().unwrap(),
            &vec!["line one\nline two\nline three".to_string()]
        );
    }

    #[test]
    fn test_output_coalescing_interrupted_by_tool() {
        let mut buf = ActivityBuffer::new(10);

        buf.append_output("output before tool\n");
        assert!(buf.output_buffer.is_some());

        buf.flush_output_buffer();

        buf.start_tool("tool-1".to_string(), "bash".to_string());
        buf.push(ActivityEntry {
            kind: ActivityKind::ToolCall,
            ingested_at: Instant::now(),
            tool_id: Some("tool-1".to_string()),
            tool_name: Some("bash".to_string()),
            status: Some("started".to_string()),
            message: None,
            output_chunks: None,
            duration: None,
        });

        buf.flush_output_buffer();
        buf.append_output("output after tool\n");
        buf.flush_output_buffer();

        assert_eq!(buf.len(), 3);
        let kinds: Vec<_> = buf.entries().map(|e| e.kind).collect();
        assert_eq!(
            kinds,
            vec![
                ActivityKind::Output,
                ActivityKind::ToolCall,
                ActivityKind::Output
            ]
        );
    }

    #[test]
    fn test_retention_fill_to_capacity() {
        let mut buf = ActivityBuffer::new(5);

        for i in 0..5 {
            buf.push(ActivityEntry {
                kind: ActivityKind::Progress,
                ingested_at: Instant::now(),
                tool_id: None,
                tool_name: None,
                status: Some(format!("{}%", i * 20)),
                message: Some(format!("progress {}", i)),
                output_chunks: None,
                duration: None,
            });
        }

        assert_eq!(buf.len(), 5);

        buf.push(ActivityEntry {
            kind: ActivityKind::Progress,
            ingested_at: Instant::now(),
            tool_id: None,
            tool_name: None,
            status: Some("100%".to_string()),
            message: Some("progress 5".to_string()),
            output_chunks: None,
            duration: None,
        });

        assert_eq!(buf.len(), 5);

        let messages: Vec<_> = buf
            .entries()
            .map(|e| e.message.as_deref().unwrap_or(""))
            .collect();
        assert_eq!(
            messages,
            vec![
                "progress 1",
                "progress 2",
                "progress 3",
                "progress 4",
                "progress 5"
            ]
        );
    }

    #[test]
    fn test_retention_eviction_order() {
        let mut buf = ActivityBuffer::new(3);

        buf.push(make_entry(ActivityKind::Operation));
        buf.push(make_entry(ActivityKind::Progress));
        buf.push(make_entry(ActivityKind::Permission));

        assert_eq!(buf.len(), 3);

        buf.push(make_entry(ActivityKind::ToolCall));
        buf.push(make_entry(ActivityKind::Output));

        assert_eq!(buf.len(), 3);

        let kinds: Vec<_> = buf.entries().map(|e| e.kind).collect();
        assert_eq!(
            kinds,
            vec![
                ActivityKind::Permission,
                ActivityKind::ToolCall,
                ActivityKind::Output
            ]
        );
    }

    #[test]
    fn test_retention_active_tools_survive_eviction() {
        let mut buf = ActivityBuffer::new(2);

        buf.start_tool("long-running".to_string(), "long-op".to_string());

        buf.push(ActivityEntry {
            kind: ActivityKind::ToolCall,
            ingested_at: Instant::now(),
            tool_id: Some("long-running".to_string()),
            tool_name: Some("long-op".to_string()),
            status: Some("started".to_string()),
            message: None,
            output_chunks: None,
            duration: None,
        });

        buf.push(ActivityEntry {
            kind: ActivityKind::Progress,
            ingested_at: Instant::now(),
            tool_id: None,
            tool_name: None,
            status: Some("50%".to_string()),
            message: Some("working".to_string()),
            output_chunks: None,
            duration: None,
        });

        buf.push(ActivityEntry {
            kind: ActivityKind::Progress,
            ingested_at: Instant::now(),
            tool_id: None,
            tool_name: None,
            status: Some("75%".to_string()),
            message: Some("almost done".to_string()),
            output_chunks: None,
            duration: None,
        });

        let tool_is_active = buf.active_tool("long-running").is_some();
        assert!(tool_is_active);

        assert_eq!(buf.entries().count(), 2);
    }

    #[test]
    fn test_deterministic_rendering_same_sequence() {
        let mut buf1 = ActivityBuffer::new(10);
        let mut buf2 = ActivityBuffer::new(10);

        let events = vec![
            (ActivityKind::ToolCall, "tool-1", "bash", "started"),
            (ActivityKind::Output, "", "", ""),
            (ActivityKind::ToolCall, "tool-1", "bash", "completed"),
            (ActivityKind::Progress, "", "", "100%"),
        ];

        for (kind, tool_id, tool_name, status) in &events {
            let entry = ActivityEntry {
                kind: *kind,
                ingested_at: Instant::now(),
                tool_id: if tool_id.is_empty() {
                    None
                } else {
                    Some(tool_id.to_string())
                },
                tool_name: if tool_name.is_empty() {
                    None
                } else {
                    Some(tool_name.to_string())
                },
                status: if status.is_empty() {
                    None
                } else {
                    Some(status.to_string())
                },
                message: None,
                output_chunks: if *kind == ActivityKind::Output {
                    Some(vec!["output".to_string()])
                } else {
                    None
                },
                duration: None,
            };
            buf1.push(entry.clone());
            buf2.push(entry.clone());
        }

        let entries1: Vec<_> = buf1.entries().collect();
        let entries2: Vec<_> = buf2.entries().collect();

        assert_eq!(entries1.len(), entries2.len());
        for (e1, e2) in entries1.iter().zip(entries2.iter()) {
            assert_eq!(e1.kind, e2.kind);
            assert_eq!(e1.tool_id, e2.tool_id);
            assert_eq!(e1.status, e2.status);
        }
    }

    #[test]
    fn test_clear_resets_all_state() {
        let mut buf = ActivityBuffer::new(10);

        buf.start_tool("tool-1".to_string(), "bash".to_string());
        buf.push(make_entry(ActivityKind::Operation));
        buf.append_output("some output");
        buf.push(make_entry(ActivityKind::Progress));

        assert!(buf.active_tool("tool-1").is_some());
        assert!(buf.output_buffer.is_some());
        assert_eq!(buf.len(), 2);

        buf.clear();

        assert!(buf.active_tool("tool-1").is_none());
        assert!(buf.output_buffer.is_none());
        assert!(buf.is_empty());
    }

    #[test]
    fn test_finalize_flushes_pending_output() {
        let mut buf = ActivityBuffer::new(10);

        buf.append_output("pending ");
        buf.append_output("output");

        assert!(buf.output_buffer.is_some());
        assert_eq!(buf.len(), 0);

        buf.finalize();

        assert!(buf.output_buffer.is_none());
        assert_eq!(buf.len(), 1);

        let entry = buf.entries().next().unwrap();
        assert_eq!(entry.kind, ActivityKind::Output);
    }

    #[test]
    fn test_multiple_concurrent_tools_tracked_independently() {
        let mut buf = ActivityBuffer::new(10);

        buf.start_tool("t1".to_string(), "read".to_string());
        buf.start_tool("t2".to_string(), "write".to_string());
        buf.start_tool("t3".to_string(), "bash".to_string());

        assert!(buf.active_tool("t1").is_some());
        assert!(buf.active_tool("t2").is_some());
        assert!(buf.active_tool("t3").is_some());

        assert_eq!(buf.active_tools().count(), 3);

        buf.complete_tool("t2");
        assert!(buf.active_tool("t2").is_none());
        assert_eq!(buf.active_tools().count(), 2);

        buf.complete_tool("t1");
        buf.complete_tool("t3");
        assert_eq!(buf.active_tools().count(), 0);
    }

    #[test]
    fn test_sweep_stale_removes_old_active_tools() {
        let mut buf = ActivityBuffer::new(10);

        buf.start_tool("stale-tool".to_string(), "old-op".to_string());

        std::thread::sleep(std::time::Duration::from_millis(100));

        buf.start_tool("fresh-tool".to_string(), "new-op".to_string());

        assert!(buf.active_tool("stale-tool").is_some());
        assert!(buf.active_tool("fresh-tool").is_some());

        let removed = buf.sweep_stale(std::time::Duration::from_millis(50));

        assert_eq!(removed, vec!["stale-tool".to_string()]);
        assert!(buf.active_tool("stale-tool").is_none());
        assert!(buf.active_tool("fresh-tool").is_some());
    }

    #[test]
    fn test_sweep_stale_preserves_recent_tools() {
        let mut buf = ActivityBuffer::new(10);

        buf.start_tool("tool-1".to_string(), "bash".to_string());
        std::thread::sleep(std::time::Duration::from_millis(50));
        buf.start_tool("tool-2".to_string(), "read".to_string());

        let removed = buf.sweep_stale(std::time::Duration::from_secs(60));

        assert!(removed.is_empty());
        assert!(buf.active_tool("tool-1").is_some());
        assert!(buf.active_tool("tool-2").is_some());
    }

    #[test]
    fn test_orphan_tool_lifecycle_without_completion() {
        let mut buf = ActivityBuffer::new(10);

        buf.start_tool("orphan-tool".to_string(), "bash".to_string());
        buf.push(ActivityEntry {
            kind: ActivityKind::ToolCall,
            ingested_at: Instant::now(),
            tool_id: Some("orphan-tool".to_string()),
            tool_name: Some("bash".to_string()),
            status: Some("started".to_string()),
            message: None,
            output_chunks: None,
            duration: None,
        });

        std::thread::sleep(std::time::Duration::from_millis(100));

        assert!(buf.active_tool("orphan-tool").is_some());

        let removed = buf.sweep_stale(std::time::Duration::from_millis(50));
        assert_eq!(removed.len(), 1);
        assert!(buf.active_tool("orphan-tool").is_none());

        assert_eq!(buf.len(), 1);
        assert_eq!(
            buf.entries().next().unwrap().status,
            Some("started".to_string())
        );
    }

    #[test]
    fn test_tool_duration_tracking() {
        let mut buf = ActivityBuffer::new(10);

        let start = Instant::now();
        buf.start_tool("timed-tool".to_string(), "bash".to_string());

        let tracked = buf.active_tool("timed-tool").unwrap();
        let tracked_start = tracked.started_at;
        assert!(tracked_start >= start);
        assert!(tracked_start <= Instant::now());

        std::thread::sleep(std::time::Duration::from_millis(50));

        let completed = buf.complete_tool("timed-tool");
        assert!(completed.is_some());

        let elapsed = Instant::now().duration_since(tracked_start);
        assert!(elapsed.as_millis() >= 50);
    }

    #[test]
    fn test_non_gateway_pane_activity_buffer_is_none() {
        use crate::harness::{AgentAdapter, SupervisorCli};
        use crate::pane::Pane;
        use std::path::PathBuf;

        let pane = Pane::director("test-director", 24, 80).expect("create director pane");
        assert!(pane.activity_buffer().is_none());

        let pane = Pane::worker(
            "claude-worker",
            PathBuf::from("/tmp"),
            None,
            "supervisor",
            &AgentAdapter::BuiltIn(SupervisorCli::Claude),
            None,
            None,
            24,
            80,
            None,
            None,
            None,
        )
        .expect("create claude worker pane");
        assert!(pane.activity_buffer().is_none());
    }

    #[test]
    fn test_gateway_pane_activity_buffer_allocated_on_demand() {
        use crate::harness::{AgentAdapter, SupervisorCli};
        use crate::pane::Pane;
        use std::path::PathBuf;

        let mut pane = Pane::worker(
            "codex-worker",
            PathBuf::from("/tmp"),
            None,
            "supervisor",
            &AgentAdapter::BuiltIn(SupervisorCli::Codex),
            None,
            None,
            24,
            80,
            None,
            None,
            None,
        )
        .expect("create codex worker pane");

        assert!(pane.activity_buffer().is_none());

        pane.ensure_activity_buffer();
        assert!(pane.activity_buffer().is_some());
    }

    #[test]
    fn test_active_operation_tracks_unfinished_operation() {
        let mut buf = ActivityBuffer::new(10);
        buf.push(ActivityEntry {
            kind: ActivityKind::Operation,
            ingested_at: Instant::now(),
            tool_id: None,
            tool_name: None,
            status: Some("started".to_string()),
            message: Some("opencode turn".to_string()),
            output_chunks: None,
            duration: None,
        });
        buf.append_output("working");
        assert_eq!(buf.active_operation(), Some("opencode turn"));

        buf.push(ActivityEntry {
            kind: ActivityKind::Operation,
            ingested_at: Instant::now(),
            tool_id: None,
            tool_name: None,
            status: Some("completed".to_string()),
            message: Some("opencode turn".to_string()),
            output_chunks: None,
            duration: None,
        });
        assert_eq!(buf.active_operation(), None);
    }
}
