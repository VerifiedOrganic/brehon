use super::action_update::detect_hallucinated_handoff_phrase;

#[test]
fn detects_exact_hallucination_from_production_incident() {
    assert_eq!(
        detect_hallucinated_handoff_phrase(
            "All 9 follow-up items have been implemented, tests pass (74/74), and the task is now in review."
        ),
        Some("task is now in review")
    );
}

#[test]
fn detects_review_ready_phrasing_variants() {
    for notes in [
        "Fixes done; ready for review.",
        "All tests pass, ready for re-review",
        "rebased and ready for rereview",
        "Task is complete and all tests green",
        "work is now complete",
    ] {
        assert!(
            detect_hallucinated_handoff_phrase(notes).is_some(),
            "expected to reject: {notes}"
        );
    }
}

#[test]
fn does_not_flag_aspirational_language() {
    for notes in [
        "Working on fixes; will be ready for review once tests pass",
        "75% done, pushing toward completion",
        "Investigating root cause before declaring anything complete",
        "",
    ] {
        assert!(
            detect_hallucinated_handoff_phrase(notes).is_none(),
            "should not reject: {notes}"
        );
    }
}

#[test]
fn is_case_insensitive() {
    assert!(detect_hallucinated_handoff_phrase("TASK IS NOW IN REVIEW").is_some());
    assert!(detect_hallucinated_handoff_phrase("Ready For Review").is_some());
}
