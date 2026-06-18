//! Live, fail-closed budget kill-switch for the TUI run loop.
//!
//! Brehon spends real money autonomously for hours-to-days unattended. This
//! module is the enforcement seam on the live spend path: a single pure,
//! injectable decision function [`evaluate_budget`] is shared by both the
//! new-prompt refusal seam (`dispatch_allowed`, called from
//! `dispatch_runtime_prompt`) and the periodic teardown tick ([`budget_tick`]).
//!
//! The spend signal is the **live per-task token rollup** persisted by
//! `brehon_types::record_task_token_usage` and read back via
//! `brehon_types::read_run_total_tokens`. We deliberately do *not* use the
//! `tokens_used: 0` `ResponseReceived` path nor the non-monotonic stability
//! counters — those are not a trustworthy spend signal.
//!
//! Token caps are the primary, trustworthy lever. The cost cap is a coarse
//! derived estimate (the pricing table falls back to a flat rate for the
//! models actually used) and is documented as approximate.
//!
//! Fail-closed: if spend state is unreadable *and* a Hard cap is configured,
//! new dispatch is refused. If no cap is configured at all, the run is never
//! killed — the owner's legitimate multi-day runs must not be surprise-killed.

use std::path::Path;
use std::time::{Duration, Instant};

use brehon_types::{BudgetConfig, BudgetEnforcement};

use super::event_loop::EventLoopCtx;
use super::recovery::push_dashboard_event;

/// How often [`budget_tick`] re-reads spend and re-evaluates the gate.
pub(super) const DEFAULT_BUDGET_CHECK_INTERVAL: Duration = Duration::from_secs(5);

/// A point-in-time read of the run's real spend.
///
/// `readable` is `false` when the spend state could not be determined (no
/// `brehon_root`, or an IO error reading the rollup). The gate uses this to
/// fail closed under a Hard cap rather than charging on with an assumed zero.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct SpendSnapshot {
    /// Real run-total tokens from the live per-task rollup.
    pub tokens: u64,
    /// Whether the spend state was readable at all.
    pub readable: bool,
}

impl SpendSnapshot {
    /// Spend state that could not be read (used to fail closed).
    fn unknown() -> Self {
        Self {
            tokens: 0,
            readable: false,
        }
    }
}

/// The decision produced by the pure budget evaluator.
#[derive(Debug, Clone, PartialEq, Eq)]
#[must_use = "a budget verdict must be acted on (refuse/warn) or the gate does nothing"]
pub(crate) enum BudgetVerdict {
    /// Spend is within bounds (or no cap is configured): proceed.
    Allow,
    /// Soft breach or alert threshold reached: warn but continue.
    Warn(String),
    /// Hard breach (or fail-closed): refuse new spend and tear down.
    Refuse(String),
}

/// True when the config carries at least one numeric spend cap (token or cost).
///
/// The wall-clock ceiling is intentionally excluded: a wall-clock-only config
/// still has no *spend* cap, so an unreadable rollup under such a config must
/// not fail closed (there is nothing to compare spend against).
fn has_spend_cap(cfg: &BudgetConfig) -> bool {
    cfg.max_tokens_per_agent.is_some() || cfg.max_total_cost.is_some()
}

/// True when the config configures no enforceable ceiling of any kind.
///
/// Delegates to [`BudgetConfig::has_enforceable_ceiling`] so the gate, the
/// startup warning, and the doctor checker share one definition of "armed" and
/// can never drift (e.g. on whether `max_cost_per_task` counts).
fn has_no_ceiling(cfg: &BudgetConfig) -> bool {
    !cfg.has_enforceable_ceiling()
}

/// Clamp the alert threshold to a sane percentage so a misconfigured `>100`
/// cannot silently disable the Soft warn-at-threshold behavior.
fn effective_alert_threshold(cfg: &BudgetConfig) -> u8 {
    cfg.alert_threshold_percent.min(100)
}

/// Pure, injectable budget decision. No IO, no clock, no agents.
///
/// `spend` is the real run-total token signal, `elapsed` is the wall-clock age
/// of the run, and `cfg` is the merged budget policy. This is the single seam
/// the dispatch refusal and the teardown tick both consult, which keeps the
/// enforcement semantics identical across both and trivially unit-testable.
///
/// Wave-1 scoping decision: `max_tokens_per_agent` is treated as the
/// **run-total** token cap. The only trustworthy spend signal we have is the
/// run-wide rollup; true per-pane caps need the pane/task id at the dispatch
/// site and are deferred to a later wave.
#[must_use = "the verdict must be acted on or the budget gate does nothing"]
pub(crate) fn evaluate_budget(
    spend: &SpendSnapshot,
    elapsed: Duration,
    cfg: &BudgetConfig,
) -> BudgetVerdict {
    // Never brick an unlimited run: with no ceiling configured, always allow.
    if has_no_ceiling(cfg) {
        return BudgetVerdict::Allow;
    }

    let hard = cfg.enforcement == BudgetEnforcement::Hard;

    // Fail closed: spend unknown + a spend cap configured under Hard => refuse.
    // (A wall-clock-only config has no spend cap, so an unreadable rollup is
    // not a reason to refuse there — elapsed is still known and checked below.)
    if !spend.readable && hard && has_spend_cap(cfg) {
        return BudgetVerdict::Refuse(
            "spend state is unreadable and a Hard token/cost cap is configured \
             (failing closed)"
                .to_string(),
        );
    }

    // Wall-clock ceiling (saturating; minutes -> seconds).
    if let Some(max_minutes) = cfg.max_wall_clock_minutes {
        let limit_secs = max_minutes.saturating_mul(60);
        if limit_secs > 0 && elapsed.as_secs() >= limit_secs {
            let reason = format!(
                "wall-clock limit reached: elapsed {}m >= max_wall_clock_minutes={}m",
                elapsed.as_secs() / 60,
                max_minutes
            );
            return if hard {
                BudgetVerdict::Refuse(reason)
            } else {
                BudgetVerdict::Warn(reason)
            };
        }
    }

    // Token cap (run-total). Only meaningful when spend is readable.
    if spend.readable {
        if let Some(max_tokens) = cfg.max_tokens_per_agent {
            if max_tokens > 0 {
                if spend.tokens >= max_tokens {
                    let reason = format!(
                        "token limit reached: {} >= max_tokens_per_agent={} (run total)",
                        spend.tokens, max_tokens
                    );
                    return if hard {
                        BudgetVerdict::Refuse(reason)
                    } else {
                        BudgetVerdict::Warn(reason)
                    };
                }
                if let Some(warn) = threshold_warning(
                    spend.tokens,
                    max_tokens,
                    effective_alert_threshold(cfg),
                    "tokens",
                ) {
                    return BudgetVerdict::Warn(warn);
                }
            }
        }

        // Approximate cost cap derived from the token rollup at a flat rate.
        if let Some(max_cost) = cfg.max_total_cost {
            if max_cost > 0.0 {
                let observed_cost = spend.tokens as f64 * brehon_types::APPROX_COST_PER_TOKEN_USD;
                if observed_cost >= max_cost {
                    let reason = format!(
                        "estimated cost limit reached: ~${:.2} >= max_total_cost={:.2} \
                         (approximate, from {} tokens)",
                        observed_cost, max_cost, spend.tokens
                    );
                    return if hard {
                        BudgetVerdict::Refuse(reason)
                    } else {
                        BudgetVerdict::Warn(reason)
                    };
                }
            }
        }
    }

    BudgetVerdict::Allow
}

/// Emit a Soft warn string when `observed` has reached `threshold_percent` of
/// `limit` (but is still under the limit). Returns `None` below the threshold.
fn threshold_warning(
    observed: u64,
    limit: u64,
    threshold_percent: u8,
    unit: &str,
) -> Option<String> {
    if threshold_percent == 0 || limit == 0 {
        return None;
    }
    // observed * 100 >= limit * threshold, computed saturating to avoid u64
    // overflow on very large rollups.
    let observed_scaled = observed.saturating_mul(100);
    let limit_scaled = limit.saturating_mul(u64::from(threshold_percent));
    if observed_scaled >= limit_scaled {
        Some(format!(
            "{unit} usage at {}% of cap ({observed}/{limit}); alert threshold {threshold_percent}%",
            percent_of(observed, limit)
        ))
    } else {
        None
    }
}

fn percent_of(observed: u64, limit: u64) -> u64 {
    if limit == 0 {
        return 0;
    }
    observed.saturating_mul(100) / limit
}

/// Read the live run-total spend. Kept separate from [`evaluate_budget`] so
/// tests inject a [`SpendSnapshot`] directly without touching the filesystem.
#[must_use]
pub(crate) fn read_spend_snapshot(brehon_root: Option<&Path>) -> SpendSnapshot {
    match brehon_root {
        None => SpendSnapshot::unknown(),
        Some(root) => match brehon_types::read_run_total_tokens(root) {
            Ok(tokens) => SpendSnapshot {
                tokens,
                readable: true,
            },
            Err(_) => SpendSnapshot::unknown(),
        },
    }
}

/// Load the merged budget policy for the run via the injected config loader.
///
/// Returns `None` when there is no `brehon_root` or the config cannot be
/// loaded. Callers under a Hard cap should treat `None` as "policy unknown",
/// but since the policy itself is what carries the cap, a missing policy means
/// there is no configured cap to enforce and the run proceeds (the gate cannot
/// refuse against a cap it cannot see; that is a config problem surfaced by the
/// startup warning + doctor check, not a per-prompt failure).
fn load_budget_config(ctx: &EventLoopCtx, brehon_root: Option<&Path>) -> Option<BudgetConfig> {
    let root = brehon_root?;
    let project_root = project_root_for_config(root);
    (ctx.project_config_loader)(&project_root).map(|cfg| cfg.budget)
}

/// The `project_config_loader` expects the *project* root, but the run loop
/// holds the `.brehon` root. Strip a trailing `.brehon` so the loader resolves
/// `.brehon/config.yaml` correctly.
fn project_root_for_config(brehon_root: &Path) -> std::path::PathBuf {
    if brehon_root.file_name().and_then(|name| name.to_str()) == Some(".brehon") {
        if let Some(parent) = brehon_root.parent() {
            return parent.to_path_buf();
        }
    }
    brehon_root.to_path_buf()
}

/// Synchronous gate for the new-prompt seam.
///
/// Returns `Err(reason)` when a fresh prompt must be refused (Hard breach or
/// fail-closed), `Ok(())` otherwise. To avoid a filesystem read on the hot
/// dispatch path, this prefers the cached verdict set by [`budget_tick`] and
/// only falls back to nothing (allow) when no cache exists — the periodic tick
/// is the authoritative, throttled evaluator. `evaluate_budget` itself stays
/// pure regardless.
pub(crate) fn dispatch_allowed(ctx: &EventLoopCtx) -> Result<(), String> {
    match ctx.budget_block_dispatch.as_ref() {
        Some(reason) => Err(reason.clone()),
        None => Ok(()),
    }
}

/// Periodic over-budget + wall-clock check and one-shot teardown.
///
/// Self-throttled by `ctx.last_budget_check`/`ctx.budget_check_interval` so it
/// is safe to call every loop iteration (mirrors `detect_and_handle_stalls`).
/// On a Hard breach it caches the refusal reason for the dispatch seam, emits
/// an operator signal, drains in-flight PTYs once via `mux.shutdown_all`, flips
/// the one-shot teardown latch, and sets `ctx.shutdown` so the post-loop
/// `shutdown_all` + CLI `terminate_session_processes` backstop reap anything
/// that outlives the inline drain.
pub(super) fn budget_tick(ctx: &mut EventLoopCtx) {
    if ctx.last_budget_check.elapsed() < ctx.budget_check_interval {
        return;
    }
    ctx.last_budget_check = Instant::now();

    let brehon_root = ctx.dashboard_data.lock().unwrap().brehon_root.clone();
    let Some(cfg) = load_budget_config(ctx, brehon_root.as_deref()) else {
        // No loadable policy => no configured cap to enforce here. Clear any
        // stale cached refusal so dispatch is not wedged on a vanished config.
        ctx.budget_block_dispatch = None;
        return;
    };

    if has_no_ceiling(&cfg) {
        ctx.budget_block_dispatch = None;
        return;
    }

    let spend = read_spend_snapshot(brehon_root.as_deref());
    let elapsed = ctx.started_at.elapsed();

    match evaluate_budget(&spend, elapsed, &cfg) {
        BudgetVerdict::Allow => {
            ctx.budget_block_dispatch = None;
        }
        BudgetVerdict::Warn(reason) => {
            // Cache nothing: Warn must not refuse dispatch.
            ctx.budget_block_dispatch = None;
            // Dedupe identical warnings so the dashboard isn't spammed.
            if ctx.last_budget_warn.as_deref() != Some(reason.as_str()) {
                tracing::warn!(reason = %reason, "budget soft warning");
                push_dashboard_event(&ctx.dashboard_data, format!("budget warning: {reason}"));
                ctx.last_budget_warn = Some(reason);
            }
        }
        BudgetVerdict::Refuse(reason) => {
            // Cache the refusal so the dispatch seam fails closed immediately,
            // even before the next tick.
            ctx.budget_block_dispatch = Some(reason.clone());

            if !ctx.budget_torn_down {
                tracing::warn!(reason = %reason, "budget hard limit reached; draining and stopping");
                push_dashboard_event(
                    &ctx.dashboard_data,
                    format!("budget exceeded: {reason} — draining and stopping"),
                );
                emit_budget_breach(ctx, &reason, &cfg);

                // Immediate mid-tick kill so spend stops before the next tick.
                // One-shot via the latch below so we never re-kill per tick.
                ctx.rt.block_on(ctx.mux.shutdown_all());
                ctx.budget_torn_down = true;
                ctx.shutdown
                    .store(true, std::sync::atomic::Ordering::SeqCst);
            }
        }
    }
}

/// Emit a durable budget-breach signal through the injected sink, if wired.
///
/// We avoid adding a new `EventKind` variant (it is matched exhaustively in
/// ~10 consumer crates) and instead thread a small budget-local record through
/// an injected closure. When no sink is wired the operator signal degrades to
/// the dashboard event + `tracing::warn!` already emitted by the caller.
fn emit_budget_breach(ctx: &EventLoopCtx, reason: &str, cfg: &BudgetConfig) {
    if let Some(sink) = ctx.budget_event_sink.as_ref() {
        sink(BudgetBreachEvent {
            reason: reason.to_string(),
            enforcement: cfg.enforcement,
        });
    }
}

/// A budget-breach record handed to the injected operator sink.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct BudgetBreachEvent {
    /// Human-readable cause naming the cap and the observed value.
    pub reason: String,
    /// The enforcement mode in effect when the breach fired.
    pub enforcement: BudgetEnforcement,
}

/// Injected operator sink for budget breaches.
pub(crate) type BudgetEventSink = std::sync::Arc<dyn Fn(BudgetBreachEvent) + Send + Sync>;

/// The production budget-breach sink.
///
/// Escalates the breach to `error!` — the strongest durable, greppable signal
/// available without introducing a new exhaustively-matched `EventKind` (which
/// would touch ~10 consumer crates) or threading an `EventStore` handle into the
/// run loop. It lands wherever the run loop's existing `tracing` output goes, so
/// an unattended operator has a durable record of *why* spend stopped. The
/// dashboard event is emitted separately by [`budget_tick`]. Tests inject a
/// capturing sink instead of this one.
#[must_use]
pub(crate) fn default_budget_event_sink() -> BudgetEventSink {
    std::sync::Arc::new(|event: BudgetBreachEvent| {
        tracing::error!(
            target: "brehon::budget",
            reason = %event.reason,
            enforcement = ?event.enforcement,
            "budget kill-switch fired: new spend refused and in-flight agents torn down"
        );
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cfg(
        max_tokens: Option<u64>,
        max_cost: Option<f64>,
        max_minutes: Option<u64>,
        enforcement: BudgetEnforcement,
        alert: u8,
    ) -> BudgetConfig {
        BudgetConfig {
            max_total_cost: max_cost,
            max_cost_per_task: None,
            max_tokens_per_agent: max_tokens,
            alert_threshold_percent: alert,
            enforcement,
            max_wall_clock_minutes: max_minutes,
        }
    }

    fn readable(tokens: u64) -> SpendSnapshot {
        SpendSnapshot {
            tokens,
            readable: true,
        }
    }

    #[test]
    fn over_token_cap_hard_refuses_with_reason() {
        let verdict = evaluate_budget(
            &readable(2_000),
            Duration::ZERO,
            &cfg(Some(1_000), None, None, BudgetEnforcement::Hard, 80),
        );
        match verdict {
            BudgetVerdict::Refuse(reason) => {
                assert!(reason.contains("token limit"), "reason: {reason}");
                assert!(reason.contains("2000"), "reason names observed: {reason}");
                assert!(reason.contains("1000"), "reason names cap: {reason}");
            }
            other => panic!("expected Refuse, got {other:?}"),
        }
    }

    #[test]
    fn under_token_cap_hard_allows() {
        let verdict = evaluate_budget(
            &readable(500),
            Duration::ZERO,
            &cfg(Some(1_000), None, None, BudgetEnforcement::Hard, 80),
        );
        assert_eq!(verdict, BudgetVerdict::Allow);
    }

    #[test]
    fn soft_over_cap_warns_not_refuses() {
        let verdict = evaluate_budget(
            &readable(2_000),
            Duration::ZERO,
            &cfg(Some(1_000), None, None, BudgetEnforcement::Soft, 80),
        );
        assert!(matches!(verdict, BudgetVerdict::Warn(_)), "got {verdict:?}");
    }

    #[test]
    fn soft_alert_threshold_reached_warns() {
        // 850/1000 = 85% >= 80% threshold, still under the cap.
        let verdict = evaluate_budget(
            &readable(850),
            Duration::ZERO,
            &cfg(Some(1_000), None, None, BudgetEnforcement::Soft, 80),
        );
        match verdict {
            BudgetVerdict::Warn(reason) => assert!(reason.contains("threshold"), "{reason}"),
            other => panic!("expected Warn, got {other:?}"),
        }
    }

    #[test]
    fn fail_closed_unreadable_hard_with_cap_refuses() {
        let verdict = evaluate_budget(
            &SpendSnapshot::unknown(),
            Duration::ZERO,
            &cfg(Some(1_000), None, None, BudgetEnforcement::Hard, 80),
        );
        match verdict {
            BudgetVerdict::Refuse(reason) => assert!(reason.contains("unreadable"), "{reason}"),
            other => panic!("expected fail-closed Refuse, got {other:?}"),
        }
    }

    #[test]
    fn unreadable_no_cap_allows_owner_unlimited_run() {
        let verdict = evaluate_budget(
            &SpendSnapshot::unknown(),
            Duration::ZERO,
            &cfg(None, None, None, BudgetEnforcement::Hard, 80),
        );
        assert_eq!(
            verdict,
            BudgetVerdict::Allow,
            "an unlimited run must never be bricked by an unreadable rollup"
        );
    }

    #[test]
    fn wall_clock_breach_hard_refuses_under_allows() {
        let policy = cfg(None, None, Some(10), BudgetEnforcement::Hard, 80);
        // Under the ceiling.
        assert_eq!(
            evaluate_budget(&readable(0), Duration::from_secs(9 * 60), &policy),
            BudgetVerdict::Allow
        );
        // At/over the ceiling.
        match evaluate_budget(&readable(0), Duration::from_secs(10 * 60), &policy) {
            BudgetVerdict::Refuse(reason) => assert!(reason.contains("wall-clock"), "{reason}"),
            other => panic!("expected wall-clock Refuse, got {other:?}"),
        }
    }

    #[test]
    fn wall_clock_breach_with_unreadable_spend_still_refuses() {
        // A wall-clock-only config has no spend cap, so the unreadable rollup
        // must not short-circuit; the elapsed breach is still enforced.
        let policy = cfg(None, None, Some(1), BudgetEnforcement::Hard, 80);
        match evaluate_budget(&SpendSnapshot::unknown(), Duration::from_secs(120), &policy) {
            BudgetVerdict::Refuse(reason) => assert!(reason.contains("wall-clock"), "{reason}"),
            other => panic!("expected wall-clock Refuse, got {other:?}"),
        }
    }

    #[test]
    fn alert_threshold_over_100_is_clamped_and_still_warns_at_cap() {
        // threshold 250 must clamp to 100, so warn only fires at/after 100%.
        let policy = cfg(Some(1_000), None, None, BudgetEnforcement::Soft, 250);
        // 950/1000 = 95% < clamped 100% threshold => no threshold warn, allow.
        assert_eq!(
            evaluate_budget(&readable(950), Duration::ZERO, &policy),
            BudgetVerdict::Allow
        );
        // At the cap it still warns (Soft over-cap).
        assert!(matches!(
            evaluate_budget(&readable(1_000), Duration::ZERO, &policy),
            BudgetVerdict::Warn(_)
        ));
    }

    #[test]
    fn no_ceiling_always_allows() {
        let verdict = evaluate_budget(
            &readable(u64::MAX),
            Duration::from_secs(u64::MAX / 2),
            &cfg(None, None, None, BudgetEnforcement::Hard, 80),
        );
        assert_eq!(verdict, BudgetVerdict::Allow);
    }

    #[test]
    fn cost_cap_hard_refuses_when_estimate_exceeds() {
        // 2_000_000 tokens at flat $1/Mtok => ~$2.00 >= $1.00 cap.
        let verdict = evaluate_budget(
            &readable(2_000_000),
            Duration::ZERO,
            &cfg(None, Some(1.0), None, BudgetEnforcement::Hard, 80),
        );
        match verdict {
            BudgetVerdict::Refuse(reason) => assert!(reason.contains("cost limit"), "{reason}"),
            other => panic!("expected cost Refuse, got {other:?}"),
        }
    }
}
