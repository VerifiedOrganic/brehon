use std::collections::HashSet;
use std::path::Path;

struct BuiltinSkillSource {
    name: &'static str,
    raw: &'static str,
}

static BUILTIN_SKILLS: &[BuiltinSkillSource] = &[
    BuiltinSkillSource {
        name: "brehon-discovery",
        raw: include_str!("../../../../brehon-mcp/src/skills/brehon-discovery.md"),
    },
    BuiltinSkillSource {
        name: "brehon-breakdown",
        raw: include_str!("../../../../brehon-mcp/src/skills/brehon-breakdown.md"),
    },
    BuiltinSkillSource {
        name: "brehon-dispatch",
        raw: include_str!("../../../../brehon-mcp/src/skills/brehon-dispatch.md"),
    },
];

pub(crate) fn builtin_skill_names_for_role(role: &str) -> &'static [&'static str] {
    match role {
        "supervisor" => &["brehon-discovery", "brehon-breakdown", "brehon-dispatch"],
        _ => &[],
    }
}

pub(crate) fn write_builtin_skills(
    dest_root: &Path,
    role: &str,
) -> std::result::Result<(), &'static str> {
    std::fs::create_dir_all(dest_root)
        .map_err(|_| "Failed to create Brehon runtime skills directory.")?;

    let allowed: HashSet<&str> = builtin_skill_names_for_role(role).iter().copied().collect();
    remove_stale_brehon_skills(dest_root, &allowed)?;

    for skill_name in allowed {
        let content = skill_markdown(skill_name)
            .ok_or("Failed to render Brehon runtime skill from builtin source.")?;
        let skill_dir = dest_root.join(skill_name);
        if skill_dir.exists() {
            if skill_dir.is_dir() {
                std::fs::remove_dir_all(&skill_dir)
                    .map_err(|_| "Failed to refresh Brehon runtime skill.")?;
            } else {
                std::fs::remove_file(&skill_dir)
                    .map_err(|_| "Failed to refresh Brehon runtime skill.")?;
            }
        }
        std::fs::create_dir_all(&skill_dir)
            .map_err(|_| "Failed to create Brehon runtime skill directory.")?;
        std::fs::write(skill_dir.join("SKILL.md"), content)
            .map_err(|_| "Failed to write Brehon runtime skill.")?;
    }

    Ok(())
}

fn remove_stale_brehon_skills(
    dest_root: &Path,
    allowed: &HashSet<&str>,
) -> std::result::Result<(), &'static str> {
    for entry in
        std::fs::read_dir(dest_root).map_err(|_| "Failed to inspect Brehon runtime skills.")?
    {
        let entry = entry.map_err(|_| "Failed to inspect Brehon runtime skill entry.")?;
        let name = entry.file_name();
        let Some(name) = name.to_str() else {
            continue;
        };
        if !name.starts_with("brehon-") || allowed.contains(name) {
            continue;
        }

        let path = entry.path();
        if path.is_dir() {
            std::fs::remove_dir_all(&path)
                .map_err(|_| "Failed to remove stale Brehon runtime skill.")?;
        } else {
            std::fs::remove_file(&path)
                .map_err(|_| "Failed to remove stale Brehon runtime skill.")?;
        }
    }

    Ok(())
}

fn skill_markdown(name: &str) -> Option<String> {
    let source = BUILTIN_SKILLS.iter().find(|source| source.name == name)?;
    let (metadata, body) = parse_skill_source(source.raw)?;
    let source_name = metadata_value(metadata, "name")?;
    if source_name != name || !name.starts_with("brehon-") {
        return None;
    }
    let description = metadata_value(metadata, "description")?;

    Some(format!(
        "---\nname: {name}\ndescription: {}\n---\n\n{}\n",
        yaml_double_quoted(description),
        body.trim()
    ))
}

fn parse_skill_source(raw: &str) -> Option<(&str, &str)> {
    let trimmed = raw.trim_start();
    let after_open = trimmed.strip_prefix("---")?;
    let close_idx = after_open.find("\n---")?;
    let metadata = &after_open[..close_idx];
    let body = after_open[close_idx + 4..].trim_start();
    Some((metadata, body))
}

fn metadata_value<'a>(metadata: &'a str, key: &str) -> Option<&'a str> {
    metadata.lines().find_map(|line| {
        let (found, value) = line.split_once(':')?;
        if found.trim() == key {
            Some(value.trim())
        } else {
            None
        }
    })
}

fn yaml_double_quoted(value: &str) -> String {
    let mut quoted = String::from("\"");
    for ch in value.chars() {
        match ch {
            '\\' => quoted.push_str("\\\\"),
            '"' => quoted.push_str("\\\""),
            '\n' => quoted.push_str("\\n"),
            '\r' => quoted.push_str("\\r"),
            '\t' => quoted.push_str("\\t"),
            _ => quoted.push(ch),
        }
    }
    quoted.push('"');
    quoted
}
