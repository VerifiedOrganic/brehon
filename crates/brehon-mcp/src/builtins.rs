//! Builtin skills embedded at compile time.
//!
//! Each skill is a markdown file with YAML frontmatter containing metadata
//! (name, description, roles, tags). Skills are role-gated: supervisors see
//! planning/orchestration skills, workers see execution skills.

/// Parsed skill metadata from YAML frontmatter.
#[derive(Debug, Clone)]
pub struct BuiltinSkill {
    pub name: String,
    pub description: String,
    pub roles: Vec<String>,
    pub tags: Vec<String>,
    pub content: String,
}

struct BuiltinSkillSource {
    path: &'static str,
    raw: &'static str,
}

/// Parse a markdown file with YAML frontmatter delimited by `---`.
///
/// Handles the simple subset of YAML used in skill files:
/// - `key: value` for scalar fields
/// - `key:\n  - item` for list fields
fn parse_skill(raw: &str) -> Option<BuiltinSkill> {
    let trimmed = raw.trim_start();
    if !trimmed.starts_with("---") {
        return None;
    }
    // Find closing `---`
    let after_open = &trimmed[3..];
    let close_idx = after_open.find("\n---")?;
    let yaml_block = &after_open[..close_idx];
    let body = after_open[close_idx + 4..].trim().to_string();

    let mut name = String::new();
    let mut description = String::new();
    let mut roles = Vec::new();
    let mut tags = Vec::new();

    // Track which list we're currently parsing
    let mut current_list: Option<&str> = None;

    for line in yaml_block.lines() {
        let trimmed_line = line.trim();

        // List item under current key
        if let Some(rest) = trimmed_line.strip_prefix("- ") {
            let val = rest.trim().to_string();
            match current_list {
                Some("roles") => roles.push(val),
                Some("tags") => tags.push(val),
                _ => {}
            }
            continue;
        }

        // New key: value pair
        current_list = None;
        if let Some((key, value)) = trimmed_line.split_once(':') {
            let key = key.trim();
            let value = value.trim();
            if value.is_empty() {
                // Key with no inline value — start of a list
                current_list = Some(if key == "roles" {
                    "roles"
                } else if key == "tags" {
                    "tags"
                } else {
                    ""
                });
            } else {
                match key {
                    "name" => name = value.to_string(),
                    "description" => description = value.to_string(),
                    _ => {}
                }
            }
        }
    }

    if name.is_empty() {
        return None;
    }

    Some(BuiltinSkill {
        name,
        description,
        roles,
        tags,
        content: body,
    })
}

// Embed all builtin skills at compile time.
static SKILL_SOURCES: &[BuiltinSkillSource] = &[
    BuiltinSkillSource {
        path: "skills/brehon-supervisor.md",
        raw: include_str!("skills/brehon-supervisor.md"),
    },
    BuiltinSkillSource {
        path: "skills/brehon-discovery.md",
        raw: include_str!("skills/brehon-discovery.md"),
    },
    BuiltinSkillSource {
        path: "skills/brehon-breakdown.md",
        raw: include_str!("skills/brehon-breakdown.md"),
    },
    BuiltinSkillSource {
        path: "skills/brehon-dispatch.md",
        raw: include_str!("skills/brehon-dispatch.md"),
    },
    BuiltinSkillSource {
        path: "skills/brehon-supervisor-checklist.md",
        raw: include_str!("skills/brehon-supervisor-checklist.md"),
    },
    BuiltinSkillSource {
        path: "skills/brehon-worker.md",
        raw: include_str!("skills/brehon-worker.md"),
    },
];

/// Return all builtin skills, parsed from embedded sources.
pub fn all_skills() -> Vec<BuiltinSkill> {
    SKILL_SOURCES
        .iter()
        .filter_map(|src| {
            let _path = src.path;
            parse_skill(src.raw)
        })
        .collect()
}

/// Return builtin skills filtered by role. If role is empty, returns all.
pub fn skills_for_role(role: &str) -> Vec<BuiltinSkill> {
    all_skills()
        .into_iter()
        .filter(|s| {
            if role.is_empty() {
                return true;
            }
            s.roles.is_empty() || s.roles.iter().any(|r| r == role || r == "any")
        })
        .collect()
}

/// Search builtin skills by query string. Matches against name, description,
/// tags, and content. Results are filtered by role.
pub fn search_skills(query: &str, role: &str, tags: Option<&[String]>) -> Vec<BuiltinSkill> {
    let query_lower = query.to_lowercase();
    let terms: Vec<&str> = query_lower.split_whitespace().collect();

    skills_for_role(role)
        .into_iter()
        .filter(|skill| {
            // Tag filter: if specified, skill must have at least one matching tag
            if let Some(required_tags) = tags {
                if !required_tags.is_empty()
                    && !required_tags
                        .iter()
                        .any(|t| skill.tags.iter().any(|st| st == t))
                {
                    return false;
                }
            }

            // Empty query matches everything (list all)
            if terms.is_empty() {
                return true;
            }

            // Text search: all query terms must appear somewhere
            let haystack = format!(
                "{} {} {} {}",
                skill.name,
                skill.description,
                skill.tags.join(" "),
                skill.content
            )
            .to_lowercase();

            terms.iter().all(|term| haystack.contains(term))
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_all_skills_parse() {
        let skills = all_skills();
        assert_eq!(skills.len(), 6, "Expected 6 builtin skills");
        for s in &skills {
            assert!(!s.name.is_empty(), "Every skill must have a name");
            assert!(
                !s.description.is_empty(),
                "Every skill must have a description"
            );
            assert!(!s.content.is_empty(), "Every skill must have content");
        }
    }

    #[test]
    fn test_all_builtin_skills_are_brehon_namespaced() {
        let skills = all_skills();
        let mut names = std::collections::HashSet::new();
        for s in &skills {
            assert!(
                s.name.starts_with("brehon-"),
                "Builtin skill '{}' must use the brehon-* namespace",
                s.name
            );
            assert!(
                names.insert(s.name.clone()),
                "Builtin skill '{}' is duplicated",
                s.name
            );
        }
    }

    #[test]
    fn test_builtin_skill_names_match_file_stems() {
        for source in SKILL_SOURCES {
            let skill = parse_skill(source.raw)
                .unwrap_or_else(|| panic!("Builtin skill source '{}' must parse", source.path));
            let stem = std::path::Path::new(source.path)
                .file_stem()
                .and_then(|value| value.to_str())
                .unwrap();
            assert_eq!(
                skill.name, stem,
                "Builtin skill '{}' frontmatter name must match file stem '{}'",
                skill.name, stem
            );
        }
    }

    #[test]
    fn test_supervisor_skills_filtered() {
        let skills = skills_for_role("supervisor");
        assert!(
            skills.len() >= 4,
            "Supervisor should see at least 4 skills, got {}",
            skills.len()
        );
        for s in &skills {
            assert!(
                s.roles.contains(&"supervisor".to_string())
                    || s.roles.is_empty()
                    || s.roles.contains(&"any".to_string()),
                "Skill {} should be visible to supervisor",
                s.name
            );
        }
    }

    #[test]
    fn test_worker_skills_filtered() {
        let skills = skills_for_role("worker");
        assert!(
            skills.iter().any(|s| s.name == "brehon-worker"),
            "Worker should see brehon-worker skill"
        );
        assert!(
            !skills.iter().any(|s| s.name == "brehon-discovery"),
            "Worker should NOT see brehon-discovery skill"
        );
    }

    #[test]
    fn test_search_by_query() {
        let results = search_skills("planning", "", None);
        assert!(
            !results.is_empty(),
            "Should find skills matching 'planning'"
        );
    }

    #[test]
    fn test_search_by_tag() {
        let tags = vec!["brainstorming".to_string()];
        let results = search_skills("", "", Some(&tags));
        assert!(
            results.iter().any(|s| s.name == "brehon-discovery"),
            "Tag search for 'brainstorming' should find brehon-discovery"
        );
    }

    #[test]
    fn test_empty_query_returns_all() {
        let results = search_skills("", "", None);
        assert_eq!(
            results.len(),
            6,
            "Empty query with no role filter should return all 6 skills"
        );
    }
}
