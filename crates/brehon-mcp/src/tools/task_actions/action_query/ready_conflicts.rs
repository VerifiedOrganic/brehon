use super::*;

pub(super) fn supervisor_integration_conflicts_from_tasks(
    tasks: &[serde_json::Map<String, Value>],
) -> Vec<Value> {
    let mut conflicts: Vec<Value> = tasks
        .iter()
        .filter(|task| {
            let status = task
                .get("status")
                .and_then(|value| value.as_str())
                .unwrap_or("");
            !is_terminal_task_status(status) && task_has_supervisor_integration_conflict(task)
        })
        .cloned()
        .map(Value::Object)
        .collect();

    conflicts.sort_by(|a, b| {
        let a_time = a.get("updated_at").and_then(|v| v.as_str()).unwrap_or("");
        let b_time = b.get("updated_at").and_then(|v| v.as_str()).unwrap_or("");
        b_time.cmp(a_time)
    });
    conflicts
}
