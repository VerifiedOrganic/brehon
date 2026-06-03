// SPDX-FileCopyrightText: 2026 Andrei G <bug-ops>
// SPDX-License-Identifier: MIT OR Apache-2.0
//
// Adapted from Zeph `zeph-core/src/channel.rs` and
// `zeph-tui/src/channel.rs`.

use serde_json::Value;

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ToolStartEvent {
    pub(crate) tool_call_id: String,
    pub(crate) tool_name: String,
    pub(crate) display: String,
    pub(crate) input: Value,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ToolOutputEvent {
    pub(crate) tool_call_id: String,
    pub(crate) tool_name: String,
    pub(crate) display: String,
    pub(crate) output: String,
    pub(crate) is_error: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum RuntimeEvent {
    MessageChunk(String),
    ToolStart(ToolStartEvent),
    ToolOutput(ToolOutputEvent),
    Progress(String),
}
