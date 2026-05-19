//! Structured task specification: parsing, composition, and validation.

use serde_json::Value;

use brehon_types::{infer_task_completion_mode, TaskCompletionMode};

use super::dependencies::{parse_optional_text_arg, parse_string_list_arg};
use super::lifecycle::{is_container_task, is_epic, is_initiative};

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(super) struct StructuredTaskSpec {
    pub(super) acceptance_criteria: Vec<String>,
    pub(super) file_hints: Vec<String>,
    pub(super) constraints: Vec<String>,
    pub(super) test_requirements: Vec<String>,
    pub(super) plan_steps: Vec<String>,
    pub(super) implementation_notes: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum StructuredDescriptionSection {
    AcceptanceCriteria,
    FileHints,
    Constraints,
    TestRequirements,
    PlanSteps,
    ImplementationNotes,
}

pub(super) fn read_structured_task_spec(args: &Value) -> Result<StructuredTaskSpec, String> {
    Ok(StructuredTaskSpec {
        acceptance_criteria: parse_string_list_arg(args, "acceptance_criteria")?,
        file_hints: parse_string_list_arg(args, "file_hints")?,
        constraints: parse_string_list_arg(args, "constraints")?,
        test_requirements: parse_string_list_arg(args, "test_requirements")?,
        plan_steps: parse_string_list_arg(args, "plan_steps")?,
        implementation_notes: parse_optional_text_arg(args, "implementation_notes")?,
    })
}

fn trim_multiline_block(text: &str) -> String {
    let lines: Vec<&str> = text.lines().collect();
    let first = lines
        .iter()
        .position(|line| !line.trim().is_empty())
        .unwrap_or(lines.len());
    let last = lines
        .iter()
        .rposition(|line| !line.trim().is_empty())
        .map(|index| index + 1)
        .unwrap_or(first);

    lines[first..last].join("\n")
}

fn normalize_structured_heading(line: &str) -> Option<StructuredDescriptionSection> {
    let trimmed = line.trim();
    if trimmed.is_empty() {
        return None;
    }

    let without_hashes = trimmed.trim_start_matches('#').trim();
    let normalized = without_hashes
        .trim_end_matches(':')
        .trim()
        .to_ascii_lowercase();

    match normalized.as_str() {
        "acceptance criteria" => Some(StructuredDescriptionSection::AcceptanceCriteria),
        "file hints" | "area hints" => Some(StructuredDescriptionSection::FileHints),
        "constraints" => Some(StructuredDescriptionSection::Constraints),
        "test requirements" | "test plan" => Some(StructuredDescriptionSection::TestRequirements),
        "plan" | "implementation plan" => Some(StructuredDescriptionSection::PlanSteps),
        "implementation notes" | "notes" => Some(StructuredDescriptionSection::ImplementationNotes),
        _ => None,
    }
}

fn strip_list_marker(line: &str) -> Option<&str> {
    let trimmed = line.trim_start();
    for marker in ["- ", "* ", "+ "] {
        if let Some(rest) = trimmed.strip_prefix(marker) {
            return Some(rest.trim());
        }
    }

    let digit_count = trimmed.chars().take_while(|ch| ch.is_ascii_digit()).count();
    if digit_count == 0 {
        return None;
    }
    let rest = &trimmed[digit_count..];
    let rest = rest.strip_prefix('.').or_else(|| rest.strip_prefix(')'))?;
    Some(rest.trim())
}

fn parse_description_list(lines: &[String]) -> Vec<String> {
    let mut items = Vec::new();
    let mut current: Option<String> = None;

    for raw_line in lines {
        let trimmed = raw_line.trim();
        if trimmed.is_empty() {
            continue;
        }

        if let Some(item) = strip_list_marker(trimmed) {
            if let Some(previous) = current.take() {
                items.push(previous);
            }
            current = Some(item.to_string());
            continue;
        }

        if let Some(existing) = current.as_mut() {
            existing.push(' ');
            existing.push_str(trimmed);
        } else {
            current = Some(trimmed.to_string());
        }
    }

    if let Some(previous) = current {
        items.push(previous);
    }

    items
}

fn parse_description_notes(lines: &[String]) -> Option<String> {
    let rendered = trim_multiline_block(&lines.join("\n"));
    if rendered.is_empty() {
        None
    } else {
        Some(rendered)
    }
}

pub(super) fn parse_structured_task_spec_from_description(
    description: &str,
) -> (String, StructuredTaskSpec) {
    let mut summary_lines = Vec::new();
    let mut acceptance_criteria_lines = Vec::new();
    let mut file_hints_lines = Vec::new();
    let mut constraints_lines = Vec::new();
    let mut test_requirements_lines = Vec::new();
    let mut plan_steps_lines = Vec::new();
    let mut implementation_notes_lines = Vec::new();
    let mut current_section = None;

    for line in description.lines() {
        if let Some(section) = normalize_structured_heading(line) {
            current_section = Some(section);
            continue;
        }

        match current_section {
            Some(StructuredDescriptionSection::AcceptanceCriteria) => {
                acceptance_criteria_lines.push(line.to_string());
            }
            Some(StructuredDescriptionSection::FileHints) => {
                file_hints_lines.push(line.to_string());
            }
            Some(StructuredDescriptionSection::Constraints) => {
                constraints_lines.push(line.to_string());
            }
            Some(StructuredDescriptionSection::TestRequirements) => {
                test_requirements_lines.push(line.to_string());
            }
            Some(StructuredDescriptionSection::PlanSteps) => {
                plan_steps_lines.push(line.to_string());
            }
            Some(StructuredDescriptionSection::ImplementationNotes) => {
                implementation_notes_lines.push(line.to_string());
            }
            None => summary_lines.push(line.to_string()),
        }
    }

    (
        trim_multiline_block(&summary_lines.join("\n")),
        StructuredTaskSpec {
            acceptance_criteria: parse_description_list(&acceptance_criteria_lines),
            file_hints: parse_description_list(&file_hints_lines),
            constraints: parse_description_list(&constraints_lines),
            test_requirements: parse_description_list(&test_requirements_lines),
            plan_steps: parse_description_list(&plan_steps_lines),
            implementation_notes: parse_description_notes(&implementation_notes_lines),
        },
    )
}

fn merge_structured_task_specs(
    explicit: StructuredTaskSpec,
    from_description: StructuredTaskSpec,
) -> StructuredTaskSpec {
    StructuredTaskSpec {
        acceptance_criteria: if explicit.acceptance_criteria.is_empty() {
            from_description.acceptance_criteria
        } else {
            explicit.acceptance_criteria
        },
        file_hints: if explicit.file_hints.is_empty() {
            from_description.file_hints
        } else {
            explicit.file_hints
        },
        constraints: if explicit.constraints.is_empty() {
            from_description.constraints
        } else {
            explicit.constraints
        },
        test_requirements: if explicit.test_requirements.is_empty() {
            from_description.test_requirements
        } else {
            explicit.test_requirements
        },
        plan_steps: if explicit.plan_steps.is_empty() {
            from_description.plan_steps
        } else {
            explicit.plan_steps
        },
        implementation_notes: explicit
            .implementation_notes
            .or(from_description.implementation_notes),
    }
}

pub(super) fn resolve_task_brief(
    description: &str,
    spec: StructuredTaskSpec,
) -> (String, StructuredTaskSpec) {
    let (summary, from_description) = parse_structured_task_spec_from_description(description);
    (summary, merge_structured_task_specs(spec, from_description))
}

pub(super) fn bullet_section(title: &str, items: &[String]) -> Option<String> {
    if items.is_empty() {
        return None;
    }
    let bullets = items
        .iter()
        .map(|item| format!("- {item}"))
        .collect::<Vec<_>>()
        .join("\n");
    Some(format!("{title}:\n{bullets}"))
}

pub(super) fn compose_task_description(summary: &str, spec: &StructuredTaskSpec) -> String {
    let mut sections = Vec::new();

    let summary = summary.trim();
    if !summary.is_empty() {
        sections.push(summary.to_string());
    }
    if let Some(section) = bullet_section("Acceptance Criteria", &spec.acceptance_criteria) {
        sections.push(section);
    }
    if let Some(section) = bullet_section("File Hints", &spec.file_hints) {
        sections.push(section);
    }
    if let Some(section) = bullet_section("Plan", &spec.plan_steps) {
        sections.push(section);
    }
    if let Some(section) = bullet_section("Constraints", &spec.constraints) {
        sections.push(section);
    }
    if let Some(section) = bullet_section("Test Requirements", &spec.test_requirements) {
        sections.push(section);
    }
    if let Some(notes) = spec.implementation_notes.as_deref() {
        sections.push(format!("Implementation Notes:\n{notes}"));
    }

    sections.join("\n\n")
}

pub(super) fn store_structured_task_spec(
    task: &mut serde_json::Map<String, Value>,
    spec: &StructuredTaskSpec,
) {
    let array = |items: &[String]| Value::Array(items.iter().cloned().map(Value::String).collect());

    if !spec.acceptance_criteria.is_empty() {
        task.insert(
            "acceptance_criteria".into(),
            array(&spec.acceptance_criteria),
        );
    }
    if !spec.file_hints.is_empty() {
        task.insert("file_hints".into(), array(&spec.file_hints));
    }
    if !spec.constraints.is_empty() {
        task.insert("constraints".into(), array(&spec.constraints));
    }
    if !spec.test_requirements.is_empty() {
        task.insert("test_requirements".into(), array(&spec.test_requirements));
    }
    if !spec.plan_steps.is_empty() {
        task.insert("plan_steps".into(), array(&spec.plan_steps));
    }
    if let Some(notes) = spec.implementation_notes.as_deref() {
        task.insert(
            "implementation_notes".into(),
            Value::String(notes.to_string()),
        );
    }
}

pub(super) fn validate_task_brief(
    task_type: &str,
    completion_mode: TaskCompletionMode,
    summary: &str,
    spec: &StructuredTaskSpec,
) -> Result<(), String> {
    let has_acceptance = !spec.acceptance_criteria.is_empty();
    let has_file_hints = !spec.file_hints.is_empty();
    let has_test_requirements = !spec.test_requirements.is_empty();
    let has_plan = !spec.plan_steps.is_empty() || spec.implementation_notes.is_some();

    let mut missing = Vec::new();
    if summary.trim().is_empty() {
        missing.push("description summary");
    }

    if is_container_task(task_type) {
        if !has_acceptance {
            missing.push("acceptance_criteria");
        }
        if !has_plan {
            missing.push("plan_steps or implementation_notes");
        }
    } else if completion_mode == TaskCompletionMode::Merge {
        if !has_acceptance {
            missing.push("acceptance_criteria");
        }
        if !has_file_hints {
            missing.push("file_hints");
        }
        if !has_test_requirements {
            missing.push("test_requirements");
        }
        if !has_plan {
            missing.push("plan_steps or implementation_notes");
        }
    }

    if missing.is_empty() {
        Ok(())
    } else if is_initiative(task_type) {
        Err(format!(
            "Initiatives must include a clear summary plus structured planning detail. Missing: {}. \
             Provide them as top-level fields or structured sections in description: \
             acceptance_criteria and plan_steps/implementation_notes.",
            missing.join(", ")
        ))
    } else if is_epic(task_type) {
        Err(format!(
            "Epic tasks must include a clear summary plus structured planning detail. Missing: {}. \
             Provide them as top-level fields or structured sections in description: \
             acceptance_criteria and plan_steps/implementation_notes.",
            missing.join(", ")
        ))
    } else {
        Err(format!(
            "Implementation merge tasks must include a clear summary plus structured detail. Missing: {}. \
             Provide them as top-level fields or structured sections in description: \
             acceptance_criteria, file_hints, test_requirements, and plan_steps/implementation_notes.",
            missing.join(", ")
        ))
    }
}

pub(super) const CONTROL_PLANE_MARKERS: &[&str] = &[
    ".brehon/",
    ".brehon\\",
    "/.brehon/",
    "\\.brehon\\",
    "runtime/reviews/",
    "runtime/sessions/",
    "runtime/prompt-queue/",
    "runtime/review-panels/",
    "runtime/review-panel-seats/",
    "runtime/tasks/",
    "runtime/events/",
    "worktrees/",
];

pub(super) fn control_plane_marker(text: &str) -> Option<&'static str> {
    let normalized = text.trim().to_ascii_lowercase();
    CONTROL_PLANE_MARKERS
        .iter()
        .copied()
        .find(|marker| normalized.contains(marker))
}

pub(super) fn control_plane_scope_issue_for_brief(
    summary: &str,
    spec: &StructuredTaskSpec,
) -> Option<String> {
    if let Some(marker) = control_plane_marker(summary) {
        return Some(format!("description summary references '{marker}'"));
    }
    for (label, items) in [
        ("file_hints", &spec.file_hints),
        ("constraints", &spec.constraints),
        ("plan_steps", &spec.plan_steps),
        ("test_requirements", &spec.test_requirements),
        ("acceptance_criteria", &spec.acceptance_criteria),
    ] {
        for item in items {
            if let Some(marker) = control_plane_marker(item) {
                return Some(format!("{label} references '{marker}' via '{item}'"));
            }
        }
    }
    if let Some(notes) = spec.implementation_notes.as_deref() {
        if let Some(marker) = control_plane_marker(notes) {
            return Some(format!("implementation_notes references '{marker}'"));
        }
    }
    None
}

pub(crate) fn control_plane_scope_issue_for_task(
    task: &serde_json::Map<String, Value>,
) -> Option<String> {
    let title = task
        .get("title")
        .and_then(|value| value.as_str())
        .unwrap_or("?");
    let description = task
        .get("description")
        .and_then(|value| value.as_str())
        .unwrap_or("");
    if let Some(marker) = control_plane_marker(description) {
        return Some(format!(
            "Task {title} references live Brehon control-plane path '{marker}' in description."
        ));
    }
    for field in [
        "file_hints",
        "constraints",
        "plan_steps",
        "test_requirements",
        "acceptance_criteria",
    ] {
        if let Some(values) = task.get(field).and_then(|value| value.as_array()) {
            for value in values {
                if let Some(item) = value.as_str() {
                    if let Some(marker) = control_plane_marker(item) {
                        return Some(format!(
                            "Task {title} references live Brehon control-plane path '{marker}' in {field}."
                        ));
                    }
                }
            }
        }
    }
    if let Some(notes) = task
        .get("implementation_notes")
        .and_then(|value| value.as_str())
    {
        if let Some(marker) = control_plane_marker(notes) {
            return Some(format!(
                "Task {title} references live Brehon control-plane path '{marker}' in implementation_notes."
            ));
        }
    }
    None
}

pub(super) fn epic_needs_integration_branch(
    title: &str,
    description: &str,
    spec: &StructuredTaskSpec,
) -> bool {
    if !spec.file_hints.is_empty() || !spec.test_requirements.is_empty() {
        return true;
    }

    infer_task_completion_mode(title, &compose_task_description(description, spec))
        == TaskCompletionMode::Merge
}
