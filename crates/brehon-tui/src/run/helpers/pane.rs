//! Pane utility predicates.

use brehon_mux::Mux;

/// Check whether a pane's CLI type requires a post-spawn prompt.
pub(crate) fn pane_needs_post_spawn_prompt(mux: &Mux, pane_id: &str) -> bool {
    mux.panes()
        .find(|pane| pane.id() == pane_id)
        .map(|pane| pane.cli_type().needs_post_spawn_prompt())
        .unwrap_or(false)
}
