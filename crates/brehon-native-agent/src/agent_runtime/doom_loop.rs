// SPDX-FileCopyrightText: 2026 Andrei G <bug-ops>
// SPDX-License-Identifier: MIT OR Apache-2.0
//
// Adapted from Zeph `zeph-agent-tools/src/doom_loop.rs`.

//! Doom-loop detection for the native agent turn loop.
//!
//! The provider may keep emitting semantically identical turns with fresh tool
//! IDs. Hashing the stable parts lets the runtime stop that loop before it burns
//! the full tool-round budget.

const DEFAULT_REPEAT_THRESHOLD: usize = 3;

#[derive(Debug, Clone)]
pub(crate) struct DoomLoopDetector {
    last_hash: Option<u64>,
    repeats: usize,
    repeat_threshold: usize,
}

impl Default for DoomLoopDetector {
    fn default() -> Self {
        Self::new(DEFAULT_REPEAT_THRESHOLD)
    }
}

impl DoomLoopDetector {
    pub(crate) fn new(repeat_threshold: usize) -> Self {
        Self {
            last_hash: None,
            repeats: 0,
            repeat_threshold: repeat_threshold.max(2),
        }
    }

    pub(crate) fn observe(&mut self, content: &str) -> bool {
        let hash = doom_loop_hash(content);
        if self.last_hash == Some(hash) {
            self.repeats = self.repeats.saturating_add(1);
        } else {
            self.last_hash = Some(hash);
            self.repeats = 1;
        }
        self.repeats >= self.repeat_threshold
    }

    pub(crate) fn repeats(&self) -> usize {
        self.repeats
    }
}

/// Hash message content for doom-loop detection, skipping volatile IDs in-place.
///
/// Normalizes `[tool_result: <id>]` to `[tool_result]` and
/// `[tool_use: <name>(<id>)]` to `[tool_use: <name>]`.
#[must_use]
pub(crate) fn doom_loop_hash(content: &str) -> u64 {
    use std::hash::{DefaultHasher, Hasher};
    let mut hasher = DefaultHasher::new();
    let mut rest = content;
    while !rest.is_empty() {
        let r_pos = rest.find("[tool_result: ");
        let u_pos = rest.find("[tool_use: ");
        match (r_pos, u_pos) {
            (Some(r), Some(u)) if u < r => hash_tool_use_in_place(&mut hasher, &mut rest, u),
            (Some(r), _) => hash_tool_result_in_place(&mut hasher, &mut rest, r),
            (_, Some(u)) => hash_tool_use_in_place(&mut hasher, &mut rest, u),
            _ => {
                hasher.write(rest.as_bytes());
                break;
            }
        }
    }
    hasher.finish()
}

fn hash_tool_result_in_place(hasher: &mut impl std::hash::Hasher, rest: &mut &str, start: usize) {
    hasher.write(&rest.as_bytes()[..start]);
    if let Some(end) = rest[start..].find(']') {
        hasher.write(b"[tool_result]");
        *rest = &rest[start + end + 1..];
    } else {
        hasher.write(&rest.as_bytes()[start..]);
        *rest = "";
    }
}

fn hash_tool_use_in_place(hasher: &mut impl std::hash::Hasher, rest: &mut &str, start: usize) {
    hasher.write(&rest.as_bytes()[..start]);
    let tag = &rest[start..];
    if let Some(paren) = tag.find('(') {
        if let Some(bracket) = tag.find(']') {
            hasher.write(b"[tool_use: ");
            hasher.write(&tag.as_bytes()["[tool_use: ".len()..paren]);
            hasher.write(b"]");
            *rest = &rest[start + bracket + 1..];
        } else {
            hasher.write(&rest.as_bytes()[start..]);
            *rest = "";
        }
    } else if let Some(bracket) = tag.find(']') {
        hasher.write(&tag.as_bytes()[..=bracket]);
        *rest = &rest[start + bracket + 1..];
    } else {
        hasher.write(&rest.as_bytes()[start..]);
        *rest = "";
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn normalizes_volatile_tool_result_ids() {
        assert_eq!(
            doom_loop_hash("[tool_result: abc] output"),
            doom_loop_hash("[tool_result: xyz] output")
        );
    }

    #[test]
    fn normalizes_volatile_tool_use_ids() {
        assert_eq!(
            doom_loop_hash("[tool_use: bash(call-1)]"),
            doom_loop_hash("[tool_use: bash(call-2)]")
        );
    }

    #[test]
    fn detector_trips_after_threshold() {
        let mut detector = DoomLoopDetector::new(3);
        assert!(!detector.observe("same"));
        assert!(!detector.observe("same"));
        assert!(detector.observe("same"));
        assert_eq!(detector.repeats(), 3);
    }

    #[test]
    fn detector_resets_on_new_content() {
        let mut detector = DoomLoopDetector::new(3);
        assert!(!detector.observe("same"));
        assert!(!detector.observe("different"));
        assert_eq!(detector.repeats(), 1);
    }
}
