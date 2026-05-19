//! Config-level worker routing helpers.

use serde_json::{Map, Value};

use brehon_types::config::{RoutingRuleConfig, RoutingRuleMatchConfig};

/// Effective execution policy for a task after applying config routing.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct ResolvedExecutionPolicy {
    pub policy: Option<Map<String, Value>>,
    pub source: ExecutionPolicySource,
    pub rule_id: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ExecutionPolicySource {
    None,
    Task,
    RoutingRule,
    RoutingDefault,
}

impl ExecutionPolicySource {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::None => "none",
            Self::Task => "task",
            Self::RoutingRule => "routing_rule",
            Self::RoutingDefault => "routing_default",
        }
    }
}

/// Resolve the policy used for assignment. Explicit task policy always wins.
pub(crate) fn resolve_execution_policy(
    task: &Map<String, Value>,
    config: Option<&brehon_types::BrehonConfig>,
) -> ResolvedExecutionPolicy {
    if let Some(policy) = task
        .get("execution_policy")
        .and_then(|value| value.as_object())
    {
        return ResolvedExecutionPolicy {
            policy: Some(policy.clone()),
            source: ExecutionPolicySource::Task,
            rule_id: None,
        };
    }

    let Some(config) = config else {
        return ResolvedExecutionPolicy {
            policy: None,
            source: ExecutionPolicySource::None,
            rule_id: None,
        };
    };

    let mut default_policy = Map::new();
    if let Some(lane) = config.routing.default_worker_lane.as_deref() {
        default_policy.insert(
            "preferred_lane".to_string(),
            Value::String(lane.to_string()),
        );
        default_policy.insert("strict".to_string(), Value::Bool(false));
    }

    for rule in &config.routing.rules {
        if !routing_rule_matches(task, rule) {
            continue;
        }
        let mut policy = default_policy.clone();
        for (key, value) in &rule.policy {
            policy.insert(key.clone(), value.clone());
        }
        return ResolvedExecutionPolicy {
            policy: (!policy.is_empty()).then_some(policy),
            source: ExecutionPolicySource::RoutingRule,
            rule_id: (!rule.id.trim().is_empty()).then(|| rule.id.clone()),
        };
    }

    ResolvedExecutionPolicy {
        policy: (!default_policy.is_empty()).then_some(default_policy),
        source: if config.routing.default_worker_lane.is_some() {
            ExecutionPolicySource::RoutingDefault
        } else {
            ExecutionPolicySource::None
        },
        rule_id: None,
    }
}

pub(crate) fn routing_summary(
    task: &Map<String, Value>,
    config: Option<&brehon_types::BrehonConfig>,
) -> Option<Value> {
    let resolved = resolve_execution_policy(task, config);
    if resolved.source == ExecutionPolicySource::None && resolved.policy.is_none() {
        return None;
    }

    Some(serde_json::json!({
        "source": resolved.source.as_str(),
        "rule_id": resolved.rule_id,
        "effective_execution_policy": resolved.policy,
    }))
}

fn routing_rule_matches(task: &Map<String, Value>, rule: &RoutingRuleConfig) -> bool {
    criteria_matches(task, &rule.criteria)
}

fn criteria_matches(task: &Map<String, Value>, criteria: &RoutingRuleMatchConfig) -> bool {
    if !criteria_has_any_matcher(criteria) {
        return false;
    }

    if criteria.default {
        return true;
    }

    if let Some(expected) = criteria.task_type.as_deref() {
        if !task_string_equals(task, "task_type", expected) {
            return false;
        }
    }

    if let Some(expected) = criteria.priority.as_deref() {
        if !task_string_equals(task, "priority", expected) {
            return false;
        }
    }

    let expected_work_classes = expected_work_classes(criteria);
    if !expected_work_classes.is_empty() {
        let task_classes = task_work_classes(task);
        if !expected_work_classes
            .iter()
            .any(|expected| task_classes.iter().any(|actual| actual == expected))
        {
            return false;
        }
    }

    if !criteria.title_any.is_empty() {
        let title = task
            .get("title")
            .and_then(|value| value.as_str())
            .unwrap_or("")
            .to_ascii_lowercase();
        if !criteria
            .title_any
            .iter()
            .any(|needle| contains_case_insensitive(&title, needle))
        {
            return false;
        }
    }

    if !criteria.text_any.is_empty() {
        let text = task_search_text(task);
        if !criteria
            .text_any
            .iter()
            .any(|needle| contains_case_insensitive(&text, needle))
        {
            return false;
        }
    }

    true
}

fn criteria_has_any_matcher(criteria: &RoutingRuleMatchConfig) -> bool {
    criteria.default
        || criteria.task_type.is_some()
        || criteria.priority.is_some()
        || criteria.work_class.is_some()
        || !criteria.work_classes.is_empty()
        || !criteria.title_any.is_empty()
        || !criteria.text_any.is_empty()
}

fn task_string_equals(task: &Map<String, Value>, key: &str, expected: &str) -> bool {
    task.get(key)
        .and_then(|value| value.as_str())
        .is_some_and(|actual| actual.eq_ignore_ascii_case(expected))
}

fn expected_work_classes(criteria: &RoutingRuleMatchConfig) -> Vec<String> {
    criteria
        .work_class
        .iter()
        .chain(criteria.work_classes.iter())
        .map(|value| value.trim().to_ascii_lowercase())
        .filter(|value| !value.is_empty())
        .collect()
}

fn task_work_classes(task: &Map<String, Value>) -> Vec<String> {
    let mut classes = Vec::new();
    push_string_field(task, "work_class", &mut classes);
    push_string_array_field(task, "work_classes", &mut classes);
    push_string_array_field(task, "tags", &mut classes);
    if let Some(policy) = task
        .get("execution_policy")
        .and_then(|value| value.as_object())
    {
        push_string_field(policy, "work_class", &mut classes);
        push_string_array_field(policy, "work_classes", &mut classes);
        push_string_array_field(policy, "tags", &mut classes);
    }
    classes.sort();
    classes.dedup();
    classes
}

fn push_string_field(map: &Map<String, Value>, key: &str, out: &mut Vec<String>) {
    if let Some(value) = map
        .get(key)
        .and_then(|value| value.as_str())
        .map(|value| value.trim().to_ascii_lowercase())
        .filter(|value| !value.is_empty())
    {
        out.push(value);
    }
}

fn push_string_array_field(map: &Map<String, Value>, key: &str, out: &mut Vec<String>) {
    if let Some(values) = map.get(key).and_then(|value| value.as_array()) {
        out.extend(
            values
                .iter()
                .filter_map(|value| value.as_str())
                .map(|value| value.trim().to_ascii_lowercase())
                .filter(|value| !value.is_empty()),
        );
    }
}

fn contains_case_insensitive(haystack_lowercase: &str, needle: &str) -> bool {
    let needle = needle.trim().to_ascii_lowercase();
    !needle.is_empty() && haystack_lowercase.contains(&needle)
}

fn task_search_text(task: &Map<String, Value>) -> String {
    let mut parts = Vec::new();
    for key in [
        "task_id",
        "id",
        "title",
        "description",
        "notes",
        "implementation_notes",
        "blockers",
        "priority",
    ] {
        if let Some(value) = task.get(key).and_then(|value| value.as_str()) {
            parts.push(value.to_string());
        }
    }
    for key in [
        "acceptance_criteria",
        "test_requirements",
        "constraints",
        "file_hints",
        "plan_steps",
    ] {
        if let Some(values) = task.get(key).and_then(|value| value.as_array()) {
            parts.extend(
                values
                    .iter()
                    .filter_map(|value| value.as_str())
                    .map(str::to_string),
            );
        }
    }
    if let Some(plan_import) = task.get("plan_import").and_then(|value| value.as_object()) {
        for key in [
            "source_gate",
            "source_task_id",
            "source_epic_id",
            "source_epic_title",
            "phase_title",
        ] {
            if let Some(value) = plan_import.get(key).and_then(|value| value.as_str()) {
                parts.push(value.to_string());
            }
        }
    }
    parts.join("\n").to_ascii_lowercase()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rule_text_match_produces_policy() {
        let mut task = Map::new();
        task.insert(
            "title".to_string(),
            Value::String("Release supply-chain gate".to_string()),
        );
        task.insert("task_type".to_string(), Value::String("task".to_string()));
        let mut config = brehon_config::parse_defaults().expect("defaults");
        config.routing.default_worker_lane = Some("kimi-worker".to_string());
        let mut policy = serde_json::Map::new();
        policy.insert(
            "preferred_lane".to_string(),
            Value::String("gpt53-worker".to_string()),
        );
        policy.insert(
            "preferred_model".to_string(),
            Value::String("gpt-5.3".to_string()),
        );
        policy.insert("strict".to_string(), Value::Bool(true));
        config
            .routing
            .rules
            .push(brehon_types::config::RoutingRuleConfig {
                id: "high-risk-release".to_string(),
                criteria: brehon_types::config::RoutingRuleMatchConfig {
                    text_any: vec!["release".to_string(), "supply-chain".to_string()],
                    ..Default::default()
                },
                policy,
            });

        let resolved = resolve_execution_policy(&task, Some(&config));
        assert_eq!(resolved.source, ExecutionPolicySource::RoutingRule);
        assert_eq!(resolved.rule_id.as_deref(), Some("high-risk-release"));
        assert_eq!(
            resolved
                .policy
                .as_ref()
                .and_then(|policy| policy.get("preferred_lane"))
                .and_then(|value| value.as_str()),
            Some("gpt53-worker")
        );
    }
}
