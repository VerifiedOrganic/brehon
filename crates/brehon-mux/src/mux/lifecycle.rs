//! Pane lifecycle methods for Mux

use crate::error::{Error, Result};
use crate::harness::AgentAdapter;
use crate::pane::{DeathReason, Generation, Pane, PaneId, PaneKind, PaneState};
use crate::pty::TeamsSpawnConfig;
use brehon_types::{RuntimeCommandKind, RuntimePolicyContext, RuntimePolicyDecision};
use std::path::PathBuf;
use std::time::Instant;
use tokio::sync::mpsc;

use super::format::ensure_isolated_cwd_is_not_shared_root;
use super::types::{
    DEFAULT_EVENT_CHANNEL_CAPACITY, DEFAULT_MAX_QUEUED_EVENTS_PER_POLL, EVENTS_PER_WORKER,
    MuxEvent, QuarantineOutcome, TerminalHostAgentFactoryBlockedPane, TerminalHostAgentFactoryPlan,
};
use super::{AgentPaneMaterialization, Mux, MuxConfig};

impl Mux {
    pub(super) fn ensure_pane_uses_isolated_cwd(&self, pane_id: &str, role: &str) -> Result<()> {
        if !self.worktree_isolation {
            return Ok(());
        }

        let shared_root = self.shared_repo_root.as_ref().ok_or_else(|| {
            Error::terminal(
                "Worktree isolation is enabled, but the mux lost track of the shared repo root."
                    .to_string(),
            )
        })?;
        let pane = self
            .panes
            .get(pane_id)
            .ok_or_else(|| Error::pane_not_found(pane_id))?;
        let pane_cwd = if pane.is_gateway_backed() {
            pane.gateway_spawn_config()
                .map(|config| PathBuf::from(config.cwd.as_str()))
        } else {
            pane.pty_spawn_config
                .as_ref()
                .and_then(|config| config.cwd.clone())
        }
        .ok_or_else(|| {
            Error::terminal(format!(
                "Worktree isolation is enabled, but {role} '{pane_id}' has no restart cwd."
            ))
        })?;

        ensure_isolated_cwd_is_not_shared_root(shared_root, &pane_cwd, role, pane_id)
    }

    /// Set adapter used for worker pane spawns.
    pub fn set_worker_cli(&mut self, worker_cli: AgentAdapter) {
        self.worker_cli = worker_cli;
    }

    /// Set model used for worker pane spawns.
    pub fn set_worker_model(&mut self, model: Option<String>) {
        self.worker_model = model;
    }

    /// Set the agent gateway for structured ACP prompt delivery.
    pub fn set_gateway(&mut self, gateway: brehon_acp::AcpGateway) {
        self.gateway = Some(gateway);
    }

    /// Get a reference to the agent gateway.
    pub fn gateway(&self) -> Option<&brehon_acp::AcpGateway> {
        self.gateway.as_ref()
    }

    /// Create a multiplexer with factory configuration
    pub fn factory(config: MuxConfig) -> Result<Self> {
        let mut mux = Self::new(config.rows, config.cols);
        mux.set_worker_cli(config.worker_cli.clone());
        mux.set_worker_model(config.worker_model.clone());
        mux.supervisor_name = config.supervisor_name.clone();
        mux.session_name = config.session_name.clone();
        mux.shared_repo_root = Some(config.cwd.clone());
        mux.worktree_isolation = config.worktree_isolation;
        mux.direct_tool_bridge_factory = config.direct_tool_bridge_factory.clone();
        mux.runtime_event_sink = config.runtime_event_sink.clone();
        mux.policy_gate = config.policy_gate.clone();

        // Scale event channel capacity and poll limit with worker count to
        // prevent backpressure deadlocks under high throughput.
        let extra_capacity = config.workers.saturating_mul(EVENTS_PER_WORKER);
        let channel_capacity = (DEFAULT_EVENT_CHANNEL_CAPACITY + extra_capacity)
            .max(512)
            .min(4096);
        let (event_tx, event_rx) = mpsc::channel(channel_capacity);
        mux.event_tx = event_tx;
        mux.event_rx = event_rx;
        mux.max_queued_events_per_poll = (DEFAULT_MAX_QUEUED_EVENTS_PER_POLL + extra_capacity)
            .max(512)
            .min(4096);

        // Calculate pane sizes based on layout
        //
        // Layout (matches brehon-tui/src/run/layout.rs): the content area is
        // split horizontally into (100 - SUPERVISOR_PCT)% workers/reviewers
        // on the left and SUPERVISOR_PCT% (=40%) supervisor on the right.
        // Workers/reviewers are tabbed, so the active one takes the full
        // left column. The naive `config.cols / num_panes` division used
        // previously produced, e.g., 30 cols per pane on a 120-col terminal
        // with 3 workers — which is far too narrow for Claude Code's Ink
        // TUI to lay out. The CLI commits to that cramped geometry on
        // startup (via TIOCGWINSZ) and a later SIGWINCH can't fully undo
        // the damage, producing the garbled supervisor panel.
        //
        // Compute per-role dims that approximate the final TUI layout so
        // the child CLI opens at a sensible size. The exact layout-aware
        // resize still happens on the first draw frame.
        const SUPERVISOR_PCT: u32 = 40;
        let total_cols = config.cols as u32;
        let supervisor_cols = ((total_cols * SUPERVISOR_PCT / 100) as u16)
            .saturating_sub(2)
            .max(20);
        let left_cols = ((total_cols * (100 - SUPERVISOR_PCT) / 100) as u16)
            .saturating_sub(2)
            .max(20);
        // Tabs + status bar consume ~5 rows on the worker/reviewer side and
        // ~3 rows on the supervisor side (no sub-tabs).
        let worker_rows = config.rows.saturating_sub(5).max(10);
        let supervisor_rows = config.rows.saturating_sub(3).max(10);
        let pane_rows = worker_rows;
        let pane_cols = left_cols;

        // Create worker panes
        let worker_names: Vec<String> = if config.worker_names.is_empty() {
            (0..config.workers)
                .map(|i| format!("worker-{}", i + 1))
                .collect()
        } else {
            config.worker_names.clone()
        };

        for name in &worker_names {
            let worker_cwd = if config.worktree_isolation {
                config.worker_cwds.get(name).cloned().ok_or_else(|| {
                    Error::terminal(format!(
                        "Worktree isolation is enabled, but worker '{name}' has no isolated cwd."
                    ))
                })?
            } else {
                config
                    .worker_cwds
                    .get(name)
                    .cloned()
                    .unwrap_or_else(|| config.cwd.clone())
            };
            if config.worktree_isolation {
                ensure_isolated_cwd_is_not_shared_root(&config.cwd, &worker_cwd, "worker", name)?;
            }
            let teams = config.teams_configs.get(name);

            // Look up per-worker adapter, fall back to default
            let adapter = config
                .worker_cli_map
                .get(name)
                .cloned()
                .unwrap_or_else(|| config.worker_cli.clone());
            // Look up per-worker model, fall back to default
            let model = config
                .worker_model_map
                .get(name)
                .map(|s| s.as_str())
                .or(config.worker_model.as_deref());
            // Look up per-worker reasoning effort (for Gemini --thinking-budget)
            let reasoning_effort = config
                .worker_reasoning_effort_map
                .get(name)
                .map(|s| s.as_str());

            let pane = Pane::worker_with_agent_type_materialized(
                name,
                worker_cwd,
                config.session_name.as_deref(),
                config.brehon_root.as_ref(),
                &config.supervisor_name,
                &adapter,
                model,
                config.worker_server_url_map.get(name).map(|s| s.as_str()),
                pane_rows,
                pane_cols,
                teams,
                reasoning_effort,
                config.worker_agent_type_map.get(name).map(String::as_str),
                config
                    .worker_env_map
                    .get(name)
                    .map(Vec::as_slice)
                    .unwrap_or(&[]),
                config.pane_materialization,
            )?;
            mux.add_pane(pane);
        }

        let supervisor_cwd = if config.worktree_isolation {
            config.supervisor_cwd.clone().ok_or_else(|| {
                Error::terminal(
                    "Worktree isolation is enabled, but supervisor has no isolated cwd."
                        .to_string(),
                )
            })?
        } else {
            config
                .supervisor_cwd
                .clone()
                .unwrap_or_else(|| config.cwd.clone())
        };
        if config.worktree_isolation {
            ensure_isolated_cwd_is_not_shared_root(
                &config.cwd,
                &supervisor_cwd,
                "supervisor",
                &config.supervisor_name,
            )?;
        }

        let sup_teams = config.teams_configs.get(&config.supervisor_name);
        let supervisor = Pane::supervisor_with_agent_type_materialized(
            &config.supervisor_name,
            supervisor_cwd,
            config.session_name.as_deref(),
            config.brehon_root.as_ref(),
            supervisor_rows,
            supervisor_cols,
            &config.supervisor_cli,
            &config.worker_cli,
            &worker_names,
            config.supervisor_model.as_deref(),
            config.supervisor_server_url.as_deref(),
            sup_teams,
            &config.worker_cli_map,
            config.supervisor_agent_type.as_deref(),
            &config.worker_agent_type_map,
            config.supervisor_reasoning_effort.as_deref(),
            &config.supervisor_env,
            config.pane_materialization,
        )?;
        mux.add_pane(supervisor);

        let reviewer_names: Vec<String> = if config.reviewer_names.is_empty() {
            config.reviewer_name.iter().cloned().collect()
        } else {
            config.reviewer_names.clone()
        };

        for reviewer_name in &reviewer_names {
            let reviewer_cwd = if config.worktree_isolation {
                config.reviewer_cwds.get(reviewer_name).cloned().ok_or_else(|| {
                    Error::terminal(format!(
                        "Worktree isolation is enabled, but reviewer '{reviewer_name}' has no isolated cwd."
                    ))
                })?
            } else {
                config
                    .reviewer_cwds
                    .get(reviewer_name)
                    .cloned()
                    .unwrap_or_else(|| config.cwd.clone())
            };
            if config.worktree_isolation {
                ensure_isolated_cwd_is_not_shared_root(
                    &config.cwd,
                    &reviewer_cwd,
                    "reviewer",
                    reviewer_name,
                )?;
            }
            let mut reviewer = Pane::reviewer_with_agent_type_materialized(
                reviewer_name,
                reviewer_cwd,
                config.session_name.as_deref(),
                config.brehon_root.as_ref(),
                pane_rows,
                pane_cols,
                config
                    .reviewer_cli_map
                    .get(reviewer_name)
                    .unwrap_or(&config.reviewer_cli),
                config
                    .reviewer_model_map
                    .get(reviewer_name)
                    .map(String::as_str)
                    .or(config.reviewer_model.as_deref()),
                config
                    .reviewer_server_url_map
                    .get(reviewer_name)
                    .map(|s| s.as_str()),
                config.teams_configs.get(reviewer_name),
                config
                    .reviewer_agent_type_map
                    .get(reviewer_name)
                    .map(String::as_str),
                config
                    .reviewer_reasoning_effort_map
                    .get(reviewer_name)
                    .map(String::as_str),
                config
                    .reviewer_env_map
                    .get(reviewer_name)
                    .map(Vec::as_slice)
                    .unwrap_or(&[]),
                config.pane_materialization,
            )?;
            apply_reviewer_panel_metadata(
                &mut reviewer,
                config.reviewer_panel_map.get(reviewer_name),
                config.reviewer_panel_tab_map.get(reviewer_name),
            );
            mux.add_pane(reviewer);
        }

        for advisor_name in &config.advisor_names {
            let advisor_cwd = if config.worktree_isolation {
                config.advisor_cwds.get(advisor_name).cloned().ok_or_else(|| {
                    Error::terminal(format!(
                        "Worktree isolation is enabled, but advisor '{advisor_name}' has no isolated cwd."
                    ))
                })?
            } else {
                config
                    .advisor_cwds
                    .get(advisor_name)
                    .cloned()
                    .unwrap_or_else(|| config.cwd.clone())
            };
            if config.worktree_isolation {
                ensure_isolated_cwd_is_not_shared_root(
                    &config.cwd,
                    &advisor_cwd,
                    "advisor",
                    advisor_name,
                )?;
            }
            let advisor = Pane::advisor_with_agent_type_materialized(
                advisor_name,
                advisor_cwd,
                config.session_name.as_deref(),
                config.brehon_root.as_ref(),
                pane_rows,
                pane_cols,
                config
                    .advisor_cli_map
                    .get(advisor_name)
                    .unwrap_or(&config.advisor_cli),
                config
                    .advisor_model_map
                    .get(advisor_name)
                    .map(String::as_str)
                    .or(config.advisor_model.as_deref()),
                config
                    .advisor_server_url_map
                    .get(advisor_name)
                    .map(String::as_str),
                config.teams_configs.get(advisor_name),
                config
                    .advisor_agent_type_map
                    .get(advisor_name)
                    .map(String::as_str),
                config
                    .advisor_reasoning_effort_map
                    .get(advisor_name)
                    .map(String::as_str),
                config
                    .advisor_env_map
                    .get(advisor_name)
                    .map(Vec::as_slice)
                    .unwrap_or(&[]),
                config.pane_materialization,
            )?;
            mux.add_pane(advisor);
        }

        // Create director pane (no PTY)
        if config.include_director {
            let director = Pane::director("director", pane_rows, pane_cols)?;
            mux.add_pane(director);
        }

        // Focus the first worker
        if let Some(first) = worker_names.first() {
            mux.focus(first);
        }

        Ok(mux)
    }

    /// Build a terminal-host launch plan from factory configuration without
    /// starting mux-owned agent processes.
    pub fn terminal_host_agent_factory_plan_from_config(
        mut config: MuxConfig,
        session_id: &str,
    ) -> Result<TerminalHostAgentFactoryPlan> {
        let session_id = session_id.trim();
        if session_id.is_empty() {
            return Err(Error::terminal(
                "terminal host agent factory planning requires a non-empty session id".to_string(),
            ));
        }

        if let Some(config_session_name) = config
            .session_name
            .as_deref()
            .map(str::trim)
            .filter(|value| !value.is_empty())
            && config_session_name != session_id
        {
            return Err(Error::terminal(format!(
                "terminal host agent factory planning session mismatch: config session '{config_session_name}' does not match requested session '{session_id}'"
            )));
        }

        config.session_name = Some(session_id.to_string());
        config.pane_materialization = AgentPaneMaterialization::PlanOnly;
        let mux = Self::factory(config)?;
        Ok(mux.terminal_host_agent_factory_plan(session_id))
    }

    pub fn add_pane(&mut self, pane: Pane) {
        let id = pane.id().to_string();
        self.panes.insert(id.clone(), pane);

        // If no pane is focused, focus this one
        if self.focused.is_none() {
            self.focused = Some(id.clone());
            if let Some(pane) = self.panes.get_mut(&id) {
                pane.set_focused(true);
            }
        }

        let _ = self.event_tx.try_send(MuxEvent::PaneAdded { pane_id: id });
    }

    /// Spawn a pane into the multiplexer.
    pub fn spawn_pane(&mut self, pane: Pane) -> PaneId {
        let id = pane.id().to_string();
        self.add_pane(pane);
        id
    }

    /// Return the logical runtime session name for this mux instance.
    pub fn session_name(&self) -> Option<&str> {
        self.session_name.as_deref()
    }

    /// Remove a pane
    pub fn remove_pane(&mut self, id: &str) -> Option<Pane> {
        let pane = self.panes.shift_remove(id);
        self.recycle_markers.remove(id);

        // If we removed the focused pane, focus the next one
        if self.focused.as_deref() == Some(id) {
            self.focused = self.panes.keys().next().cloned();
            if let Some(new_focus) = &self.focused
                && let Some(pane) = self.panes.get_mut(new_focus)
            {
                pane.set_focused(true);
            }
        }

        if pane.is_some() {
            let _ = self.event_tx.try_send(MuxEvent::PaneRemoved {
                pane_id: id.to_string(),
            });
        }

        pane
    }

    /// Destroy a pane by ID, returning it if it existed.
    pub fn destroy_pane(&mut self, pane_id: &str) -> Option<Pane> {
        self.remove_pane(pane_id)
    }

    /// Get a pane by ID
    pub fn get(&self, id: &str) -> Option<&Pane> {
        self.panes.get(id)
    }

    /// Get a mutable pane by ID
    pub fn get_mut(&mut self, id: &str) -> Option<&mut Pane> {
        self.panes.get_mut(id)
    }

    /// Get the focused pane
    pub fn focused(&self) -> Option<&Pane> {
        self.focused.as_ref().and_then(|id| self.panes.get(id))
    }

    /// Get the focused pane mutably
    pub fn focused_mut(&mut self) -> Option<&mut Pane> {
        if let Some(id) = self.focused.clone() {
            self.panes.get_mut(&id)
        } else {
            None
        }
    }

    /// Get the focused pane ID
    pub fn focused_id(&self) -> Option<&str> {
        self.focused.as_deref()
    }

    /// Focus a pane by ID
    pub fn focus(&mut self, id: &str) -> bool {
        if !self.panes.contains_key(id) {
            return false;
        }

        let old_focus = self.focused.take();

        // Unfocus old pane
        if let Some(old_id) = &old_focus
            && let Some(pane) = self.panes.get_mut(old_id)
        {
            pane.set_focused(false);
        }

        // Focus new pane
        self.focused = Some(id.to_string());
        if let Some(pane) = self.panes.get_mut(id) {
            pane.set_focused(true);
        }

        let _ = self.event_tx.try_send(MuxEvent::FocusChanged {
            from: old_focus,
            to: id.to_string(),
        });

        true
    }

    /// Focus the next pane
    pub fn focus_next(&mut self) {
        let ids: Vec<_> = self.panes.keys().cloned().collect();
        if ids.is_empty() {
            return;
        }

        let current_idx = self
            .focused
            .as_ref()
            .and_then(|f| ids.iter().position(|id| id == f))
            .unwrap_or(0);

        let next_idx = (current_idx + 1) % ids.len();
        self.focus(&ids[next_idx]);
    }

    /// Focus the previous pane
    pub fn focus_prev(&mut self) {
        let ids: Vec<_> = self.panes.keys().cloned().collect();
        if ids.is_empty() {
            return;
        }

        let current_idx = self
            .focused
            .as_ref()
            .and_then(|f| ids.iter().position(|id| id == f))
            .unwrap_or(0);

        let prev_idx = if current_idx == 0 {
            ids.len() - 1
        } else {
            current_idx - 1
        };
        self.focus(&ids[prev_idx]);
    }

    /// Get all pane IDs
    pub fn pane_ids(&self) -> Vec<&str> {
        self.panes.keys().map(|s| s.as_str()).collect()
    }

    /// Get all panes
    pub fn panes(&self) -> impl Iterator<Item = &Pane> {
        self.panes.values()
    }

    /// Get all panes mutably
    pub fn panes_mut(&mut self) -> impl Iterator<Item = &mut Pane> {
        self.panes.values_mut()
    }

    /// Get panes of a specific kind
    pub fn panes_by_kind(&self, kind: PaneKind) -> impl Iterator<Item = &Pane> {
        self.panes.values().filter(move |p| *p.kind() == kind)
    }

    /// Get worker panes
    pub fn workers(&self) -> impl Iterator<Item = &Pane> {
        self.panes_by_kind(PaneKind::Worker)
    }

    /// Get the count of all panes
    pub fn pane_count(&self) -> usize {
        self.panes.len()
    }

    pub fn terminal_host_agent_factory_plan(
        &self,
        session_id: &str,
    ) -> TerminalHostAgentFactoryPlan {
        let mut launch_specs = Vec::new();
        let mut blocked_panes = Vec::new();
        let mut total_panes = 0usize;

        for pane in self.panes.values() {
            if !matches!(
                pane.kind(),
                PaneKind::Worker | PaneKind::Reviewer | PaneKind::Advisor | PaneKind::Supervisor
            ) {
                continue;
            }
            total_panes += 1;
            match pane.terminal_host_launch_plan(session_id) {
                crate::pane::AgentTerminalLaunchPlan::TerminalHost(spec) => {
                    launch_specs.push(spec);
                }
                crate::pane::AgentTerminalLaunchPlan::GatewayBacked { reason, .. }
                | crate::pane::AgentTerminalLaunchPlan::Unsupported { reason } => {
                    blocked_panes.push(TerminalHostAgentFactoryBlockedPane {
                        pane_id: pane.id().to_string(),
                        kind: pane.kind().as_str().to_string(),
                        reason,
                    });
                }
            }
        }

        TerminalHostAgentFactoryPlan {
            total_panes,
            launch_specs,
            blocked_panes,
        }
    }

    /// Synchronize mux-side generation fencing with a pane owned by an external
    /// terminal host.
    pub fn sync_terminal_host_pane_generation(
        &mut self,
        pane_id: &str,
        generation: u64,
    ) -> Result<()> {
        let pane = self
            .panes
            .get_mut(pane_id)
            .ok_or_else(|| Error::pane_not_found(pane_id))?;
        let generation = Generation(generation);
        if generation > pane.current_generation {
            pane.current_generation = generation;
        }
        Ok(())
    }

    /// Clear mux-local Teams nudge state after an external host delivered the
    /// wake-up keystroke.
    pub fn mark_terminal_host_inbox_nudge_dispatched(&mut self, pane_id: &str) -> Result<()> {
        let pane = self
            .panes
            .get_mut(pane_id)
            .ok_or_else(|| Error::pane_not_found(pane_id))?;
        pane.set_pending_inbox_nudge(false);
        Ok(())
    }

    /// Get the count of worker panes
    pub fn worker_count(&self) -> usize {
        self.panes
            .values()
            .filter(|p| *p.kind() == PaneKind::Worker)
            .count()
    }

    /// Add a worker pane at runtime
    ///
    /// This is the primary method for dynamic worker spawning.
    ///
    /// # Arguments
    /// * `name` - Worker name (also used as pane ID)
    /// * `cwd` - Working directory for the worker (typically a clone directory)
    /// * `brehon_root` - Optional path to .brehon directory for BREHON_ROOT env var
    /// * `supervisor_name` - Name of the supervisor (enables `target: supervisor` in message action)
    ///
    /// # Returns
    /// The pane ID on success
    #[allow(clippy::too_many_arguments)]
    pub fn add_worker(
        &mut self,
        name: &str,
        cwd: PathBuf,
        brehon_root: Option<&PathBuf>,
        supervisor_name: &str,
        teams: Option<&TeamsSpawnConfig>,
        cli: Option<&AgentAdapter>,
        model: Option<&str>,
        server_url: Option<&str>,
        reasoning_effort: Option<&str>,
    ) -> Result<PaneId> {
        // Check if pane with this name already exists
        if self.panes.contains_key(name) {
            return Err(Error::pty(format!("Pane '{name}' already exists")));
        }
        let command = self.runtime_command_for_session(RuntimeCommandKind::SpawnPane {
            kind: brehon_types::RuntimePaneKind::Worker,
            pane_id: Some(name.to_string()),
            title: Some(name.to_string()),
            cwd: Some(cwd.to_string_lossy().to_string()),
            command: Vec::new(),
            env: std::collections::BTreeMap::new(),
            rows: Some(self.rows),
            cols: Some(self.cols),
        });
        let decision =
            self.evaluate_runtime_policy_immediate(command, RuntimePolicyContext::default());
        if let Some(err) = Self::policy_decision_error("worker spawn", &decision) {
            return Err(err);
        }

        let effective_cli = cli.unwrap_or(&self.worker_cli);
        let effective_model = model.or(self.worker_model.as_deref());

        let pane = Pane::worker(
            name,
            cwd,
            brehon_root,
            supervisor_name,
            effective_cli,
            effective_model,
            server_url,
            self.rows,
            self.cols,
            teams,
            reasoning_effort,
        )?;
        let id = pane.id().to_string();
        self.add_pane(pane);
        Ok(id)
    }

    /// Add a new shell pane to the mux.
    pub fn add_shell(
        &mut self,
        name: &str,
        cwd: PathBuf,
        shell_command: Option<&str>,
    ) -> Result<PaneId> {
        if self.panes.contains_key(name) {
            return Err(Error::pty(format!("Pane '{name}' already exists")));
        }
        let command = self.runtime_command_for_session(RuntimeCommandKind::SpawnPane {
            kind: brehon_types::RuntimePaneKind::Shell,
            pane_id: Some(name.to_string()),
            title: Some(name.to_string()),
            cwd: Some(cwd.to_string_lossy().to_string()),
            command: shell_command
                .map(|command| vec!["sh".to_string(), "-c".to_string(), command.to_string()])
                .unwrap_or_default(),
            env: std::collections::BTreeMap::new(),
            rows: Some(self.rows),
            cols: Some(self.cols),
        });
        let decision =
            self.evaluate_runtime_policy_immediate(command, RuntimePolicyContext::default());
        if let Some(err) = Self::policy_decision_error("shell spawn", &decision) {
            return Err(err);
        }
        let pane = Pane::shell(name, cwd, shell_command, self.rows, self.cols)?;
        let id = pane.id().to_string();
        self.add_pane(pane);
        Ok(id)
    }

    /// Remove a shell pane by name.
    pub fn remove_shell(&mut self, name: &str) -> Result<()> {
        if let Some(pane) = self.panes.get(name) {
            if *pane.kind() != PaneKind::Shell {
                return Err(Error::pty(format!(
                    "Pane '{}' is not a shell (is {:?})",
                    name,
                    pane.kind()
                )));
            }
        } else {
            return Err(Error::pane_not_found(name));
        }
        let command = self.runtime_command_for_pane(
            name,
            RuntimeCommandKind::ClosePane {
                reason: "remove shell".to_string(),
            },
        );
        let context = self.runtime_policy_context_for_pane(name);
        let decision = self.evaluate_runtime_policy_immediate(command, context);
        if let Some(err) = Self::policy_decision_error("shell close", &decision) {
            return Err(err);
        }
        self.remove_pane(name);
        Ok(())
    }

    /// Remove a worker pane by name and cleanup its PTY
    ///
    /// This is the primary method for dynamic worker shutdown.
    /// The pane's PTY will be dropped, sending SIGHUP to the process.
    pub fn remove_worker(&mut self, name: &str) -> Result<()> {
        // Verify it's a worker
        if let Some(pane) = self.panes.get(name) {
            if *pane.kind() != PaneKind::Worker {
                return Err(Error::pty(format!(
                    "Pane '{}' is not a worker (is {:?})",
                    name,
                    pane.kind()
                )));
            }
        } else {
            return Err(Error::pane_not_found(name));
        }
        let command = self.runtime_command_for_pane(
            name,
            RuntimeCommandKind::ClosePane {
                reason: "remove worker".to_string(),
            },
        );
        let context = self.runtime_policy_context_for_pane(name);
        let decision = self.evaluate_runtime_policy_immediate(command, context);
        if let Some(err) = Self::policy_decision_error("worker close", &decision) {
            return Err(err);
        }

        // Remove the pane (this drops the PTY, sending SIGHUP)
        self.remove_pane(name);
        Ok(())
    }

    /// Get the supervisor pane
    pub fn supervisor(&self) -> Option<&Pane> {
        self.panes_by_kind(PaneKind::Supervisor).next()
    }

    /// Get the director pane
    pub fn director(&self) -> Option<&Pane> {
        self.panes_by_kind(PaneKind::Director).next()
    }

    /// Authoritatively quarantine a pane into a terminal dead state.
    ///
    /// Quarantine is idempotent. Once a pane is dead, subsequent quarantine
    /// requests preserve the original reason until the pane is replaced.
    pub fn quarantine(&mut self, pane_id: &str, reason: DeathReason) -> QuarantineOutcome {
        let command = self.runtime_command_for_pane(
            pane_id,
            RuntimeCommandKind::QuarantinePane {
                reason: format!("{reason:?}"),
            },
        );
        let context = self.runtime_policy_context_for_pane(pane_id);
        let decision = self.evaluate_runtime_policy_immediate(command, context);
        if !matches!(decision, RuntimePolicyDecision::Allow) {
            let blocked = Self::policy_rejection_reason(&decision);
            tracing::warn!(
                pane = %pane_id,
                reason = ?blocked,
                "Ignored quarantine request by policy"
            );
            return QuarantineOutcome {
                new_reason: blocked,
                was_already_dead: false,
                prior_reason: None,
            };
        }

        if let Some(existing_reason) =
            self.panes
                .get(pane_id)
                .and_then(|pane| match pane.pane_state() {
                    Some(PaneState::Dead {
                        reason: existing_reason,
                        ..
                    }) => Some(existing_reason.clone()),
                    _ => None,
                })
        {
            return QuarantineOutcome {
                new_reason: existing_reason.clone(),
                was_already_dead: true,
                prior_reason: Some(existing_reason),
            };
        }

        let quarantined_at = Instant::now();
        let Some(pane) = self.panes.get_mut(pane_id) else {
            tracing::warn!(
                pane = %pane_id,
                reason = ?reason,
                "Ignoring quarantine request for unknown pane"
            );
            return QuarantineOutcome {
                new_reason: reason,
                was_already_dead: false,
                prior_reason: None,
            };
        };

        pane.set_tool_executing(false);
        pane.set_pending_inbox_nudge(false);
        pane.set_pane_state(PaneState::Dead {
            reason: reason.clone(),
            at: quarantined_at,
        });
        pane.set_last_output_at(quarantined_at);
        pane.prompt_queue = Default::default();
        if pane.is_gateway_backed() {
            pane.clear_gateway_session();
            pane.set_gateway_event_bridge_started(false);
            if let Some(activity) = pane.activity_buffer_mut() {
                activity.clear();
            }
        }

        self.clear_active_gateway_operations(pane_id);
        self.recycle_markers.remove(pane_id);
        self.pending_delayed_prompts
            .retain(|pending| pending.pane_id != pane_id);

        QuarantineOutcome {
            new_reason: reason,
            was_already_dead: false,
            prior_reason: None,
        }
    }

    /// Authoritatively recycle a pane session and return the resulting generation.
    ///
    /// Recycle is unconditional: it never refuses and always returns a
    /// generation. If the same recycle request is replayed with no
    /// intervening pane activity, this returns the previous generation and
    /// avoids resetting backend session state again.
    pub async fn recycle(&mut self, pane_id: &str, reason: &str) -> Generation {
        if self.panes.contains_key(pane_id) {
            let command = self.runtime_command_for_pane(
                pane_id,
                RuntimeCommandKind::RecyclePane {
                    reason: reason.to_string(),
                },
            );
            let context = self.runtime_policy_context_for_pane(pane_id);
            let decision = self.evaluate_runtime_policy(command, context).await;
            if !matches!(decision, RuntimePolicyDecision::Allow) {
                tracing::warn!(
                    pane = %pane_id,
                    reason = ?Self::policy_rejection_reason(&decision),
                    "Ignored recycle request by policy"
                );
                return self
                    .panes
                    .get(pane_id)
                    .map(|pane| pane.current_generation())
                    .unwrap_or_default();
            }
        }

        let Some((kind, last_output_at)) = self
            .panes
            .get(pane_id)
            .map(|pane| (pane.kind().clone(), pane.last_output_at()))
        else {
            tracing::warn!(
                pane = %pane_id,
                reason = %reason,
                "Ignoring recycle request for unknown pane"
            );
            return Generation::default();
        };

        if let Some(marker) = self.recycle_markers.get(pane_id).copied()
            && last_output_at <= marker.at
        {
            tracing::debug!(
                pane = %pane_id,
                reason = %reason,
                generation = marker.generation.0,
                "Replaying idempotent recycle request"
            );
            return marker.generation;
        }

        let recycled_at = Instant::now();
        let generation = if let Some(pane) = self.panes.get_mut(pane_id) {
            let next_generation = Generation(pane.current_generation().0.saturating_add(1));
            pane.current_generation = next_generation;
            pane.set_tool_executing(false);
            pane.set_pending_inbox_nudge(false);
            pane.set_pane_state(PaneState::Ready { since: recycled_at });
            pane.set_last_output_at(recycled_at);
            next_generation
        } else {
            tracing::warn!(
                pane = %pane_id,
                reason = %reason,
                "Pane disappeared before recycle state update"
            );
            return Generation::default();
        };

        self.recycle_markers.insert(
            pane_id.to_string(),
            super::types::RecycleMarker {
                at: recycled_at,
                generation,
            },
        );

        self.pending_delayed_prompts
            .retain(|pending| pending.pane_id != pane_id);
        self.clear_active_gateway_operations(pane_id);

        let reset = match kind {
            PaneKind::Worker => self.reset_worker_gateway_session(pane_id).await,
            PaneKind::Reviewer => self.reset_reviewer_session(pane_id).await,
            PaneKind::Advisor => self.reset_advisor_session(pane_id).await,
            PaneKind::Supervisor => self.reset_supervisor_session(pane_id).await,
            _ => Ok(()),
        };
        if let Err(err) = reset {
            tracing::warn!(
                pane = %pane_id,
                reason = %reason,
                error = %err,
                "Recycle backend reset failed; keeping authoritative generation bump"
            );
        }

        if let Some(pane) = self.panes.get_mut(pane_id) {
            pane.current_generation = generation;
            pane.set_tool_executing(false);
            pane.set_pending_inbox_nudge(false);
            pane.set_pane_state(PaneState::Ready { since: recycled_at });
            pane.set_last_output_at(recycled_at);
            if pane.is_gateway_backed() {
                pane.clear_gateway_session();
                pane.set_gateway_event_bridge_started(false);
                if let Some(activity) = pane.activity_buffer_mut() {
                    activity.clear();
                }
            }
        }
        self.publish_runtime_pane_spawned(pane_id);

        generation
    }

    /// Hard-reset a worker session while keeping the pane slot and task
    /// assignment intact.
    ///
    /// Gateway-backed workers have their session killed and restarted on
    /// demand. Native PTY workers are restarted from their stored PTY spawn
    /// config. Used for recovery from provider failures such as context-length
    /// overruns where a fresh session against the same worktree can continue
    /// safely.
    /// Recycle a pane session, keeping the visible pane slot.
    pub async fn recycle_pane(&mut self, pane_id: &str) -> Result<()> {
        let kind = self
            .panes
            .get(pane_id)
            .ok_or_else(|| Error::pane_not_found(pane_id))?
            .kind()
            .clone();
        let command = self.runtime_command_for_pane(
            pane_id,
            RuntimeCommandKind::RecyclePane {
                reason: "manual recycle".to_string(),
            },
        );
        let context = self.runtime_policy_context_for_pane(pane_id);
        let decision = self.evaluate_runtime_policy(command, context).await;
        if let Some(err) = Self::policy_decision_error("pane recycle", &decision) {
            return Err(err);
        }
        match kind {
            PaneKind::Worker => self.reset_worker_gateway_session(pane_id).await,
            PaneKind::Reviewer => self.reset_reviewer_session(pane_id).await,
            PaneKind::Advisor => self.reset_advisor_session(pane_id).await,
            PaneKind::Supervisor => self.reset_supervisor_session(pane_id).await,
            _ => Err(Error::pty(format!("Pane '{pane_id}' cannot be recycled"))),
        }
    }

    /// Resize all panes
    pub fn resize(&mut self, rows: u16, cols: u16) -> Result<()> {
        self.rows = rows;
        self.cols = cols;

        // Recalculate pane sizes
        let num_panes = self.panes.len();
        if num_panes == 0 {
            return Ok(());
        }

        let pane_cols = cols / num_panes as u16;
        let pane_rows = rows;

        for pane in self.panes.values_mut() {
            pane.resize(pane_rows, pane_cols)?;
        }

        Ok(())
    }

    /// Get the terminal size
    pub fn size(&self) -> (u16, u16) {
        (self.rows, self.cols)
    }
    /// Shut down all panes and best-effort terminate any live gateway sessions.
    ///
    /// PTY-backed panes are killed directly. Gateway-backed panes are first
    /// detached from mux runtime state and then their underlying sessions are
    /// terminated via the agent gateway. This is the normal shutdown path for
    /// the TUI so agent subprocesses do not outlive the main `brehon run`
    /// process.
    pub async fn shutdown_all(&mut self) {
        let gateway = self.gateway.clone();
        let mut gateway_sessions = Vec::new();
        let mut operation_keys_to_clear = Vec::new();

        for (pane_id, pane) in &mut self.panes {
            if let Some(session_id) = pane.gateway_session_id().map(brehon_types::SessionId::new) {
                gateway_sessions.push((pane_id.clone(), session_id));
            }

            operation_keys_to_clear.push(pane_id.clone());
            pane.set_tool_executing(false);
            pane.set_pending_inbox_nudge(false);

            if pane.is_gateway_backed() {
                pane.clear_gateway_session();
                pane.set_gateway_event_bridge_started(false);
                if let Some(activity) = pane.activity_buffer_mut() {
                    activity.clear();
                }
            }

            pane.kill();
        }
        for pane_id in operation_keys_to_clear {
            self.clear_active_gateway_operations(&pane_id);
        }
        self.recycle_markers.clear();

        let Some(gateway) = gateway else {
            return;
        };

        for (pane_id, session_id) in gateway_sessions {
            if let Err(err) = brehon_ports::AgentGateway::kill_session(&gateway, &session_id).await {
                let err_text = err.to_string();
                let lower = err_text.to_ascii_lowercase();
                if !(lower.contains("not found") || lower.contains("unknown session")) {
                    tracing::warn!(
                        pane = %pane_id,
                        session_id = %session_id,
                        error = %err_text,
                        "Failed to kill gateway session during shutdown"
                    );
                }
            }
        }
    }
    /// Get the stored supervisor name.
    pub fn supervisor_name(&self) -> &str {
        &self.supervisor_name
    }
}

fn apply_reviewer_panel_metadata(
    pane: &mut Pane,
    panel_name: Option<&String>,
    tab_name: Option<&String>,
) {
    if let Some(panel_name) = panel_name.map(String::as_str).map(str::trim)
        && !panel_name.is_empty()
    {
        set_pane_spawn_env(pane, "BREHON_REVIEW_PANEL", panel_name);
    }
    if let Some(tab_name) = tab_name.map(String::as_str).map(str::trim)
        && !tab_name.is_empty()
    {
        set_pane_spawn_env(pane, "BREHON_REVIEW_PANEL_TAB", tab_name);
    }
}

fn set_pane_spawn_env(pane: &mut Pane, key: &str, value: &str) {
    if let Some(config) = pane.pty_spawn_config.as_mut() {
        set_env_value(&mut config.env, key, value);
    }
    if let Some(config) = pane.gateway_spawn_config.as_mut() {
        set_env_value(&mut config.env, key, value);
    }
}

fn set_env_value(env: &mut Vec<(String, String)>, key: &str, value: &str) {
    if let Some((_, existing)) = env.iter_mut().find(|(existing, _)| existing == key) {
        *existing = value.to_string();
    } else {
        env.push((key.to_string(), value.to_string()));
    }
}
