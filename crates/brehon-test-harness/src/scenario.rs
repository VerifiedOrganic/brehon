//! YAML scenario parser and executor.
//!
//! Supports declarative test scenarios in YAML format.

use std::collections::HashMap;
use std::path::Path;

use serde::Deserialize;

use crate::{InMemoryEventStore, MockBehavior, MockDecisionEngine, MockGateway};

/// Parsed scenario definition.
#[derive(Debug, Clone, Deserialize)]
pub struct Scenario {
    pub scenario: String,
    #[serde(default)]
    pub description: Option<String>,
    #[serde(default)]
    pub config: ScenarioConfig,
    pub agents: HashMap<String, AgentSpec>,
    #[serde(default)]
    pub tasks: Vec<TaskSpec>,
    #[serde(default)]
    pub assertions: Vec<AssertionSpec>,
}

/// Scenario configuration.
#[derive(Debug, Clone, Deserialize, Default)]
pub struct ScenarioConfig {
    #[serde(default)]
    pub supervisor: SupervisorConfig,
    #[serde(default)]
    pub review: ReviewConfig,
}

#[derive(Debug, Clone, Deserialize, Default)]
pub struct SupervisorConfig {
    #[serde(default)]
    pub autonomy: Option<String>,
}

#[derive(Debug, Clone, Deserialize, Default)]
pub struct ReviewConfig {
    #[serde(default)]
    pub policy: Option<ReviewPolicySpec>,
}

#[derive(Debug, Clone, Deserialize, Default)]
pub struct ReviewPolicySpec {
    #[serde(default = "default_min_average")]
    pub min_average_score: u8,
    #[serde(default = "default_min_individual")]
    pub min_individual_score: u8,
    #[serde(default = "default_blocking")]
    pub blocking_score: u8,
    #[serde(default = "default_approvals")]
    pub min_approvals: u8,
}

fn default_min_average() -> u8 {
    7
}
fn default_min_individual() -> u8 {
    6
}
fn default_blocking() -> u8 {
    5
}
fn default_approvals() -> u8 {
    2
}

/// Agent specification in a scenario.
#[derive(Debug, Clone, Deserialize)]
pub struct AgentSpec {
    #[serde(rename = "type")]
    pub agent_type: String,
    #[serde(default)]
    pub behavior: BehaviorSpec,
}

/// Behavior specification - can be a fixture reference or inline config.
#[derive(Debug, Clone)]
pub enum BehaviorSpec {
    Fixture(String),
    Inline(AgentBehavior),
}

impl Default for BehaviorSpec {
    fn default() -> Self {
        BehaviorSpec::Inline(AgentBehavior::default())
    }
}

impl<'de> Deserialize<'de> for BehaviorSpec {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        use serde::de::{MapAccess, Visitor};

        struct BehaviorSpecVisitor;

        impl<'de> Visitor<'de> for BehaviorSpecVisitor {
            type Value = BehaviorSpec;

            fn expecting(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
                formatter.write_str("a string fixture name or a behavior configuration object")
            }

            fn visit_str<E>(self, value: &str) -> Result<Self::Value, E>
            where
                E: serde::de::Error,
            {
                Ok(BehaviorSpec::Fixture(value.to_string()))
            }

            fn visit_map<M>(self, map: M) -> Result<Self::Value, M::Error>
            where
                M: MapAccess<'de>,
            {
                let behavior: AgentBehavior =
                    Deserialize::deserialize(serde::de::value::MapAccessDeserializer::new(map))?;
                Ok(BehaviorSpec::Inline(behavior))
            }
        }

        deserializer.deserialize_any(BehaviorSpecVisitor)
    }
}

/// Agent behavior configuration.
#[derive(Debug, Clone, Deserialize, Default)]
pub struct AgentBehavior {
    #[serde(default)]
    pub complete_after_messages: Option<usize>,
    #[serde(default)]
    pub stuck_after: Option<usize>,
    #[serde(default)]
    pub crash_after: Option<usize>,
    #[serde(default)]
    pub review_scores: Vec<u8>,
    #[serde(default)]
    pub progress_events: Vec<String>,
    #[serde(default)]
    pub response_delay_ms: Option<u64>,
}

impl BehaviorSpec {
    pub fn to_mock_behavior(&self) -> MockBehavior {
        match self {
            BehaviorSpec::Fixture(name) => crate::fixtures::all_fixtures()
                .get(name.as_str())
                .cloned()
                .unwrap_or_default(),
            BehaviorSpec::Inline(behavior) => behavior.to_mock_behavior(),
        }
    }
}

impl AgentBehavior {
    pub fn to_mock_behavior(&self) -> MockBehavior {
        MockBehavior {
            stuck_after_message: self.stuck_after,
            crash_after_message: self.crash_after,
            review_scores: self.review_scores.clone(),
            progress_events: self.progress_events.clone(),
            response_delay: self
                .response_delay_ms
                .map(std::time::Duration::from_millis)
                .unwrap_or_default(),
            ..Default::default()
        }
    }
}

/// Task specification in a scenario.
#[derive(Debug, Clone, Deserialize)]
pub struct TaskSpec {
    pub id: String,
    #[serde(default)]
    pub title: Option<String>,
    #[serde(default)]
    pub description: Option<String>,
}

/// Assertion specification.
#[derive(Debug, Clone, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum AssertionSpec {
    #[serde(rename = "task_status")]
    TaskStatus { task_id: String, status: String },
    #[serde(rename = "events_contain")]
    EventsContain { events: Vec<String> },
    #[serde(rename = "review_rounds")]
    ReviewRounds {
        max: Option<usize>,
        exact: Option<usize>,
    },
    #[serde(rename = "total_nudges")]
    TotalNudges { count: usize },
}

/// Scenario execution result.
#[derive(Debug, Clone)]
pub struct ScenarioResult {
    pub name: String,
    pub passed: bool,
    pub failures: Vec<String>,
}

/// Scenario runner.
pub struct ScenarioRunner {
    store: InMemoryEventStore,
    gateway: MockGateway,
    decision_engine: MockDecisionEngine,
    behaviors: HashMap<String, MockBehavior>,
}

impl ScenarioRunner {
    pub fn new() -> Self {
        Self {
            store: InMemoryEventStore::new(),
            gateway: MockGateway::new(),
            decision_engine: MockDecisionEngine::new(),
            behaviors: HashMap::new(),
        }
    }

    pub fn with_fixtures(mut self, fixtures: HashMap<String, MockBehavior>) -> Self {
        self.behaviors = fixtures;
        self
    }

    pub fn parse_yaml(content: &str) -> Result<Scenario, serde_yaml::Error> {
        serde_yaml::from_str(content)
    }

    pub fn load_yaml(path: &Path) -> Result<Scenario, Box<dyn std::error::Error>> {
        let content = std::fs::read_to_string(path)?;
        Self::parse_yaml(&content).map_err(Into::into)
    }

    pub fn store(&self) -> &InMemoryEventStore {
        &self.store
    }

    pub fn gateway(&self) -> &MockGateway {
        &self.gateway
    }

    pub fn decision_engine(&self) -> &MockDecisionEngine {
        &self.decision_engine
    }

    pub fn apply_behavior(&self, behavior: &AgentBehavior) -> MockBehavior {
        behavior.to_mock_behavior()
    }

    pub fn get_fixture(&self, name: &str) -> Option<&MockBehavior> {
        self.behaviors.get(name)
    }
}

impl Default for ScenarioRunner {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_basic_scenario() {
        let yaml = r#"
scenario: "basic_task_completion"
description: "Single worker completes a task"
agents:
  worker-1:
    type: mock
    behavior:
      complete_after_messages: 3
tasks:
  - id: T001
    title: "Test task"
assertions:
  - type: task_status
    task_id: T001
    status: merged
"#;

        let scenario = ScenarioRunner::parse_yaml(yaml).unwrap();
        assert_eq!(scenario.scenario, "basic_task_completion");
        assert!(scenario.agents.contains_key("worker-1"));
        assert_eq!(scenario.tasks.len(), 1);
        assert_eq!(scenario.tasks[0].id, "T001");
    }

    #[test]
    fn parse_agent_behavior() {
        let yaml = r#"
scenario: "test"
agents:
  worker-1:
    type: mock
    behavior:
      stuck_after: 5
      crash_after: 10
      review_scores: [8, 7, 9]
      response_delay_ms: 100
tasks: []
assertions: []
"#;

        let scenario = ScenarioRunner::parse_yaml(yaml).unwrap();
        let behavior = &scenario.agents.get("worker-1").unwrap().behavior;
        match behavior {
            BehaviorSpec::Inline(b) => {
                assert_eq!(b.stuck_after, Some(5));
                assert_eq!(b.crash_after, Some(10));
                assert_eq!(b.review_scores, vec![8, 7, 9]);
                assert_eq!(b.response_delay_ms, Some(100));
            }
            BehaviorSpec::Fixture(name) => {
                panic!("Expected inline behavior, got fixture: {}", name);
            }
        }
    }

    #[test]
    fn parse_review_config() {
        let yaml = r#"
scenario: "test"
config:
  review:
    policy:
      min_average_score: 8
      min_individual_score: 5
      blocking_score: 4
      min_approvals: 3
agents: {}
tasks: []
assertions: []
"#;

        let scenario = ScenarioRunner::parse_yaml(yaml).unwrap();
        let policy = scenario.config.review.policy.unwrap();
        assert_eq!(policy.min_average_score, 8);
        assert_eq!(policy.min_individual_score, 5);
        assert_eq!(policy.blocking_score, 4);
        assert_eq!(policy.min_approvals, 3);
    }

    #[test]
    fn behavior_to_mock() {
        let behavior = AgentBehavior {
            stuck_after: Some(3),
            crash_after: None,
            review_scores: vec![7, 8],
            progress_events: vec!["Starting".into(), "Working".into()],
            response_delay_ms: Some(50),
            complete_after_messages: None,
        };

        let mock = behavior.to_mock_behavior();
        assert_eq!(mock.stuck_after_message, Some(3));
        assert_eq!(mock.review_scores, vec![7, 8]);
        assert_eq!(mock.progress_events, vec!["Starting", "Working"]);
        assert_eq!(mock.response_delay, std::time::Duration::from_millis(50));
    }
}
