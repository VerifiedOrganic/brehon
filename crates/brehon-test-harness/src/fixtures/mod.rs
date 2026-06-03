//! Pre-built scenarios for testing.
//!
//! This module contains YAML scenario definitions that cover common testing scenarios.

use std::collections::HashMap;
use std::time::Duration;

use crate::mock_agent::MockBehavior;

/// Basic task completion scenario.
pub const BASIC_TASK_COMPLETION: &str = r#"
scenario: "basic_task_completion"
description: "Single worker completes a task, passes review, merges"

config:
  supervisor:
    autonomy: minimal
  review:
    policy:
      min_average_score: 7
      min_individual_score: 6
      blocking_score: 5
      min_approvals: 2

agents:
  supervisor:
    type: mock
    behavior:
      complete_after_messages: 1
  worker-1:
    type: mock
    behavior:
      complete_after_messages: 3
      progress_events:
        - "Reading codebase"
        - "Implementing feature"
        - "Running tests"
  reviewer-1:
    type: mock
    behavior:
      review_scores: [8]
  reviewer-2:
    type: mock
    behavior:
      review_scores: [7]

tasks:
  - id: T001
    title: "Implement auth middleware"
    description: "Add JWT validation"

assertions:
  - type: task_status
    task_id: T001
    status: merged
  - type: events_contain
    events: [task_created, task_assigned, task_completed]
  - type: review_rounds
    exact: 1
  - type: total_nudges
    count: 0
"#;

/// Stuck worker scenario.
pub const STUCK_WORKER: &str = r#"
scenario: "stuck_worker_nudge"
description: "Worker gets stuck, time-based detection fires, nudge sent, worker resumes"

config:
  supervisor:
    autonomy: minimal
    stuck_threshold_minutes: 5

agents:
  supervisor:
    type: mock
    behavior:
      complete_after_messages: 1
  worker-1:
    type: mock
    behavior:
      stuck_after: 3
      progress_events:
        - "Starting work"
        - "Got stuck"
        - "Resuming after nudge"

tasks:
  - id: T001
    title: "Fix bug in API"
    description: "Fix null pointer exception"

assertions:
  - type: task_status
    task_id: T001
    status: merged
  - type: events_contain
    events: [stuck_detected, nudge_sent]
  - type: total_nudges
    count: 1
"#;

/// Review failure and iteration scenario.
pub const REVIEW_ITERATION: &str = r#"
scenario: "review_iteration"
description: "Review round 1 fails, worker iterates, round 2 passes"

config:
  supervisor:
    autonomy: minimal
  review:
    policy:
      min_average_score: 7
      min_individual_score: 5
      blocking_score: 4
      min_approvals: 2
      max_review_rounds: 3

agents:
  supervisor:
    type: mock
    behavior:
      complete_after_messages: 1
  worker-1:
    type: mock
    behavior:
      complete_after_messages: 3
  reviewer-1:
    type: mock
    behavior:
      review_scores: [5, 8]
  reviewer-2:
    type: mock
    behavior:
      review_scores: [6, 7]

tasks:
  - id: T001
    title: "Add unit tests"
    description: "Add tests for auth module"

assertions:
  - type: task_status
    task_id: T001
    status: merged
  - type: events_contain
    events: [review_requested, review_changes_requested, task_completed]
  - type: review_rounds
    exact: 2
"#;

/// Review deadlock scenario.
pub const REVIEW_DEADLOCK: &str = r#"
scenario: "review_deadlock"
description: "Three rounds never reach threshold, supervisor judgment triggered"

config:
  supervisor:
    autonomy: full
  review:
    policy:
      min_average_score: 8
      min_individual_score: 6
      blocking_score: 5
      min_approvals: 3
      max_review_rounds: 3

agents:
  supervisor:
    type: mock
    behavior:
      complete_after_messages: 1
  worker-1:
    type: mock
    behavior:
      complete_after_messages: 2
  reviewer-1:
    type: mock
    behavior:
      review_scores: [6, 6, 6]
  reviewer-2:
    type: mock
    behavior:
      review_scores: [7, 7, 7]
  reviewer-3:
    type: mock
    behavior:
      review_scores: [6, 6, 6]

tasks:
  - id: T001
    title: "Refactor module"
    description: "Refactor for better maintainability"

assertions:
  - type: events_contain
    events: [escalation_triggered]
  - type: review_rounds
    exact: 3
"#;

/// Agent crash and respawn scenario.
pub const AGENT_CRASH_RESPAWN: &str = r#"
scenario: "worker_crash_respawn"
description: "Worker dies mid-task, respawned, task reassigned"

config:
  supervisor:
    autonomy: minimal

agents:
  supervisor:
    type: mock
    behavior:
      complete_after_messages: 1
  worker-1:
    type: mock
    behavior:
      crash_after: 2
  worker-2:
    type: mock
    behavior:
      complete_after_messages: 2

tasks:
  - id: T001
    title: "Implement feature"
    description: "Add new endpoint"

assertions:
  - type: events_contain
    events: [agent_died, task_assigned]
  - type: task_status
    task_id: T001
    status: merged
"#;

/// Harsh reviewer scenario.
pub const HARSH_REVIEWER: &str = r#"
scenario: "harsh_reviewer"
description: "Reviewer with low scores, requires multiple iterations"

config:
  supervisor:
    autonomy: minimal
  review:
    policy:
      min_average_score: 7
      min_individual_score: 5
      blocking_score: 4
      min_approvals: 2

agents:
  supervisor:
    type: mock
    behavior:
      complete_after_messages: 1
  worker-1:
    type: mock
    behavior:
      complete_after_messages: 2
  reviewer-1:
    type: mock
    behavior:
      review_scores: [4, 5, 7]
  reviewer-2:
    type: mock
    behavior:
      review_scores: [8, 8, 8]

tasks:
  - id: T001
    title: "Fix critical bug"
    description: "Fix security vulnerability"

assertions:
  - type: task_status
    task_id: T001
    status: merged
  - type: review_rounds
    min: 2
"#;

/// Chaos testing scenario.
pub const CHAOS_TIMING: &str = r#"
scenario: "chaos_timing"
description: "Random delays test system resilience"

config:
  supervisor:
    autonomy: minimal
  chaos:
    delay_range_ms: [0, 100]

agents:
  supervisor:
    type: mock
    behavior:
      complete_after_messages: 1
  worker-1:
    type: mock
    behavior:
      response_delay_ms: 50
      complete_after_messages: 2

tasks:
  - id: T001
    title: "Test task"
    description: "Tests chaos resilience"

assertions:
  - type: task_status
    task_id: T001
    status: merged
"#;

/// All pre-built scenarios.
pub fn all_scenarios() -> Vec<(&'static str, &'static str)> {
    vec![
        ("basic_task_completion", BASIC_TASK_COMPLETION),
        ("stuck_worker", STUCK_WORKER),
        ("review_iteration", REVIEW_ITERATION),
        ("review_deadlock", REVIEW_DEADLOCK),
        ("agent_crash_respawn", AGENT_CRASH_RESPAWN),
        ("harsh_reviewer", HARSH_REVIEWER),
        ("chaos_timing", CHAOS_TIMING),
    ]
}

/// Normal worker that completes tasks without issues.
pub fn normal_worker() -> MockBehavior {
    MockBehavior::normal()
}

/// Worker that gets stuck after N messages.
pub fn stuck_worker(after_messages: usize) -> MockBehavior {
    MockBehavior::stuck_after(after_messages)
}

/// Worker that crashes after N messages.
pub fn crashing_worker(after_messages: usize) -> MockBehavior {
    MockBehavior::crashing_after(after_messages)
}

/// Harsh reviewer that gives low scores.
pub fn harsh_reviewer() -> MockBehavior {
    MockBehavior::reviewer(vec![3, 4, 5])
}

/// Lenient reviewer that gives high scores.
pub fn lenient_reviewer() -> MockBehavior {
    MockBehavior::reviewer(vec![9, 10])
}

/// Strict reviewer with high standards.
pub fn strict_reviewer() -> MockBehavior {
    MockBehavior::reviewer(vec![6, 7, 8])
}

/// Average reviewer with moderate scores.
pub fn average_reviewer() -> MockBehavior {
    MockBehavior::reviewer(vec![6, 7, 7, 8])
}

/// Slow worker with response delays.
pub fn slow_worker(delay_ms: u64) -> MockBehavior {
    MockBehavior::with_delay(Duration::from_millis(delay_ms))
}

/// Worker with progress reporting.
pub fn verbose_worker() -> MockBehavior {
    MockBehavior::with_progress(vec![
        "Starting task".into(),
        "Reading codebase".into(),
        "Making changes".into(),
        "Running tests".into(),
        "Completing task".into(),
    ])
}

/// Worker that eventually completes after iterations.
pub fn iterative_worker(n_iterations: usize) -> MockBehavior {
    MockBehavior {
        max_responses: Some(n_iterations),
        response_content: Some("continuing work".into()),
        ..Default::default()
    }
}

/// Supervisor with default behavior.
pub fn default_supervisor() -> MockBehavior {
    MockBehavior::normal()
}

/// Supervisor that escalates quickly.
pub fn escalation_supervisor() -> MockBehavior {
    MockBehavior {
        response_content: Some("escalating".into()),
        ..Default::default()
    }
}

/// Reviewer panel fixture with varied behaviors.
pub fn reviewer_panel() -> Vec<MockBehavior> {
    vec![average_reviewer(), strict_reviewer(), lenient_reviewer()]
}

/// Chaotic worker with random delays and crashes.
pub fn chaotic_worker() -> MockBehavior {
    MockBehavior {
        response_delay: Duration::from_millis(100),
        crash_after_message: Some(10),
        ..Default::default()
    }
}

/// Worker that provides good feedback.
pub fn feedback_worker() -> MockBehavior {
    MockBehavior {
        progress_events: vec![
            "Analyzing requirements".into(),
            "Implementing solution".into(),
            "Writing tests".into(),
            "Refactoring for clarity".into(),
            "Final verification".into(),
        ],
        ..Default::default()
    }
}

/// All fixture behaviors as a map.
pub fn all_fixtures() -> HashMap<&'static str, MockBehavior> {
    let mut fixtures = HashMap::new();

    fixtures.insert("normal_worker", normal_worker());
    fixtures.insert("stuck_worker", stuck_worker(3));
    fixtures.insert("stuck_worker_slow", stuck_worker(5));
    fixtures.insert("crashing_worker", crashing_worker(2));
    fixtures.insert("crashing_worker_late", crashing_worker(10));
    fixtures.insert("harsh_reviewer", harsh_reviewer());
    fixtures.insert("lenient_reviewer", lenient_reviewer());
    fixtures.insert("strict_reviewer", strict_reviewer());
    fixtures.insert("average_reviewer", average_reviewer());
    fixtures.insert("slow_worker", slow_worker(200));
    fixtures.insert("very_slow_worker", slow_worker(1000));
    fixtures.insert("verbose_worker", verbose_worker());
    fixtures.insert("iterative_worker", iterative_worker(3));
    fixtures.insert("default_supervisor", default_supervisor());
    fixtures.insert("escalation_supervisor", escalation_supervisor());
    fixtures.insert("chaotic_worker", chaotic_worker());
    fixtures.insert("feedback_worker", feedback_worker());

    fixtures
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn all_scenarios_parseable() {
        for (name, yaml) in all_scenarios() {
            let result: Result<crate::scenario::Scenario, _> =
                crate::scenario::ScenarioRunner::parse_yaml(yaml);
            assert!(
                result.is_ok(),
                "Scenario '{}' failed to parse: {:?}",
                name,
                result
            );
        }
    }

    #[test]
    fn scenario_count() {
        assert_eq!(all_scenarios().len(), 7);
    }
}
