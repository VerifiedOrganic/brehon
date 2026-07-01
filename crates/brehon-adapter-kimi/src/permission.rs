use serde_json::Value;

pub(crate) fn permission_response_for_approved(params: Option<&Value>) -> Value {
    let selected = params
        .and_then(|params| params.get("options"))
        .and_then(Value::as_array)
        .and_then(|options| select_approval_option(options));

    match selected {
        Some(option_id) => serde_json::json!({
            "outcome": {
                "outcome": "selected",
                "optionId": option_id,
            }
        }),
        None => serde_json::json!({
            "outcome": {
                "outcome": "cancelled",
            }
        }),
    }
}

pub(crate) fn permission_response_for_denied(params: Option<&Value>, reason: &str) -> Value {
    let selected = params
        .and_then(|params| params.get("options"))
        .and_then(Value::as_array)
        .and_then(|options| select_rejection_option(options));

    match selected {
        Some(option_id) => serde_json::json!({
            "outcome": {
                "outcome": "selected",
                "optionId": option_id,
                "message": reason,
            }
        }),
        None => serde_json::json!({
            "outcome": {
                "outcome": "cancelled",
                "message": reason,
            }
        }),
    }
}

fn select_approval_option(options: &[Value]) -> Option<String> {
    options
        .iter()
        .find_map(|option| {
            let kind = option.get("kind").and_then(Value::as_str);
            let name = option.get("name").and_then(Value::as_str);
            let is_approval = kind
                .map(|kind| kind.starts_with("allow_") || kind == "allow" || kind == "approve")
                .unwrap_or(false)
                || name
                    .map(|name| {
                        let name = name.to_ascii_lowercase();
                        name.contains("approve") || name.contains("allow")
                    })
                    .unwrap_or(false);
            if is_approval {
                option
                    .get("optionId")
                    .or_else(|| option.get("id"))
                    .and_then(Value::as_str)
                    .map(ToOwned::to_owned)
            } else {
                None
            }
        })
        .or_else(|| {
            options.iter().find_map(|option| {
                option
                    .get("optionId")
                    .or_else(|| option.get("id"))
                    .and_then(Value::as_str)
                    .map(ToOwned::to_owned)
            })
        })
}

fn select_rejection_option(options: &[Value]) -> Option<String> {
    options.iter().find_map(|option| {
        let kind = option.get("kind").and_then(Value::as_str);
        let name = option.get("name").and_then(Value::as_str);
        let is_rejection = kind
            .map(|kind| {
                kind.starts_with("reject_")
                    || kind == "reject"
                    || kind == "deny"
                    || kind == "cancel"
            })
            .unwrap_or(false)
            || name
                .map(|name| {
                    let name = name.to_ascii_lowercase();
                    name.contains("reject") || name.contains("deny") || name.contains("cancel")
                })
                .unwrap_or(false);
        if is_rejection {
            option
                .get("optionId")
                .or_else(|| option.get("id"))
                .and_then(Value::as_str)
                .map(ToOwned::to_owned)
        } else {
            None
        }
    })
}

pub(crate) fn permission_denial_reason(params: Option<&Value>) -> Option<String> {
    let mut texts = Vec::new();
    if let Some(params) = params {
        collect_permission_text(params, &mut texts);
    }
    for text in texts {
        if text.contains("BREHON_PROJECT_ROOT") {
            return Some(
                "Brehon blocks shell commands that reference BREHON_PROJECT_ROOT during isolated runs; use relative paths inside BREHON_WORKSPACE_ROOT."
                    .to_string(),
            );
        }
        if text_contains_bare_go_build(&text) {
            return Some(
                "Brehon blocks bare `go build <package>` because it can write an executable into the current directory. Use `go test`, `go build ./...`, or `go build -o /tmp/<name> <package>` for validation."
                    .to_string(),
            );
        }
    }
    None
}

fn collect_permission_text(value: &Value, out: &mut Vec<String>) {
    match value {
        Value::String(text) => out.push(text.clone()),
        Value::Array(items) => {
            for item in items {
                collect_permission_text(item, out);
            }
        }
        Value::Object(map) => {
            for value in map.values() {
                collect_permission_text(value, out);
            }
        }
        _ => {}
    }
}

fn text_contains_bare_go_build(text: &str) -> bool {
    text.split(&[';', '|'][..])
        .flat_map(|segment| segment.split("&&"))
        .flat_map(|segment| segment.split("||"))
        .any(segment_contains_bare_go_build)
}

fn segment_contains_bare_go_build(segment: &str) -> bool {
    let tokens: Vec<&str> = segment.split_whitespace().collect();
    let Some(go_index) = tokens.iter().position(|token| *token == "go") else {
        return false;
    };
    let Some(build_index) = tokens
        .iter()
        .enumerate()
        .skip(go_index + 1)
        .find_map(|(idx, token)| (*token == "build").then_some(idx))
    else {
        return false;
    };
    let args = &tokens[build_index + 1..];
    if args
        .iter()
        .any(|arg| *arg == "-o" || arg.starts_with("-o="))
    {
        return false;
    }
    let package_args = args
        .iter()
        .copied()
        .filter(|arg| !arg.starts_with('-'))
        .filter(|arg| *arg != "2>/dev/null" && *arg != ">/dev/null")
        .collect::<Vec<_>>();
    if package_args.is_empty() {
        return true;
    }
    package_args.iter().any(|arg| *arg != "./...")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn approval_response_prefers_allow_option() {
        let response = permission_response_for_approved(Some(&serde_json::json!({
            "options": [
                {"kind": "reject_once", "name": "Reject", "optionId": "reject"},
                {"kind": "allow_once", "name": "Approve once", "optionId": "approve"},
                {"kind": "allow_always", "name": "Approve for this session", "optionId": "approve_for_session"}
            ]
        })));

        assert_eq!(response["outcome"]["optionId"], "approve");
    }

    #[test]
    fn approval_response_falls_back_to_first_option_id() {
        let response = permission_response_for_approved(Some(&serde_json::json!({
            "options": [
                {"kind": "custom", "optionId": "custom-first"},
                {"kind": "custom", "optionId": "custom-second"}
            ]
        })));

        assert_eq!(response["outcome"]["optionId"], "custom-first");
    }

    #[test]
    fn rejects_bare_go_build_package() {
        let params = serde_json::json!({
            "action": "Running: go vet ./tools/epdg-dev && go build ./tools/epdg-dev && gofmt -l tools/epdg-dev/main.go",
            "options": [
                {"kind": "reject_once", "name": "Reject", "optionId": "reject"},
                {"kind": "allow_once", "name": "Approve once", "optionId": "approve"}
            ]
        });

        let reason = permission_denial_reason(Some(&params)).unwrap();
        let response = permission_response_for_denied(Some(&params), &reason);

        assert_eq!(response["outcome"]["optionId"], "reject");
        assert!(response["outcome"]["message"]
            .as_str()
            .unwrap()
            .contains("bare `go build <package>`"));
    }

    #[test]
    fn allows_go_build_with_output_path() {
        let params = serde_json::json!({
            "action": "Running: go vet ./tools/epdg-dev && go build -o /tmp/epdg-dev ./tools/epdg-dev",
            "options": [
                {"kind": "reject_once", "name": "Reject", "optionId": "reject"},
                {"kind": "allow_once", "name": "Approve once", "optionId": "approve"}
            ]
        });

        assert!(permission_denial_reason(Some(&params)).is_none());
    }

    #[test]
    fn rejects_project_root_reference() {
        let params = serde_json::json!({
            "action": "Running: cd \"$BREHON_PROJECT_ROOT\" && sed -i 's/a/b/' src/lib.rs",
            "options": [
                {"kind": "reject_once", "name": "Reject", "optionId": "reject"},
                {"kind": "allow_once", "name": "Approve once", "optionId": "approve"}
            ]
        });

        let reason = permission_denial_reason(Some(&params)).unwrap();
        let response = permission_response_for_denied(Some(&params), &reason);

        assert_eq!(response["outcome"]["optionId"], "reject");
        assert!(response["outcome"]["message"]
            .as_str()
            .unwrap()
            .contains("BREHON_PROJECT_ROOT"));
    }
}
