// SPDX-FileCopyrightText: 2026 Andrei G <bug-ops>
// SPDX-License-Identifier: MIT OR Apache-2.0
//
// Adapted from Zeph's channel and agent-tools runtime scaffolding. The types in
// this module are Brehon-owned and intentionally avoid Zeph crate dependencies.

pub(crate) mod dispatch;
pub(crate) mod doom_loop;
pub(crate) mod events;
pub(crate) mod executor;
pub(crate) mod message;
pub(crate) mod orchestrator;
pub(crate) mod runner;
pub(crate) mod turn;
