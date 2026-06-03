use crate::tools::agent::{
    prompt_delivery_ack_dir, prompt_enqueue_ack_dir, sanitize_prompt_id_for_path,
};
use brehon_types::{pane_assignment_context_path, PaneAssignmentContext};
use std::path::Path;

pub(crate) fn write_prompt_delivery_fixture(
    root: &Path,
    prompt_id: &str,
    target: &str,
    injected: bool,
) {
    let enqueue_dir = prompt_enqueue_ack_dir(root);
    std::fs::create_dir_all(&enqueue_dir).unwrap();
    std::fs::write(
        enqueue_dir.join(format!("{}.json", sanitize_prompt_id_for_path(prompt_id))),
        serde_json::to_string_pretty(&serde_json::json!({
            "prompt_id": prompt_id,
            "target": target,
            "queued_at": "2026-05-24T01:00:00Z"
        }))
        .unwrap(),
    )
    .unwrap();
    if injected {
        let delivery_dir = prompt_delivery_ack_dir(root);
        std::fs::create_dir_all(&delivery_dir).unwrap();
        std::fs::write(
            delivery_dir.join(format!("{}.json", sanitize_prompt_id_for_path(prompt_id))),
            serde_json::to_string_pretty(&serde_json::json!({
                "prompt_id": prompt_id,
                "target": target,
                "method": "prompt_queue",
                "injected_at": "2026-05-24T01:00:05Z"
            }))
            .unwrap(),
        )
        .unwrap();
    }
}

pub(crate) fn write_pane_assignment_context_fixture(
    root: &Path,
    pane_id: &str,
    assignment_kind: &str,
    task_id: &str,
    review_id: Option<&str>,
    round: Option<u32>,
) {
    let path = pane_assignment_context_path(root, pane_id);
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    let context = PaneAssignmentContext {
        pane_id: pane_id.to_string(),
        assignment_kind: assignment_kind.to_string(),
        task_id: task_id.to_string(),
        review_id: review_id.map(str::to_string),
        round,
        status: if assignment_kind == "review" {
            "collecting".to_string()
        } else {
            "assigned".to_string()
        },
        updated_at: "2026-05-24T01:00:10Z".to_string(),
    };
    std::fs::write(path, serde_json::to_string_pretty(&context).unwrap()).unwrap();
}
