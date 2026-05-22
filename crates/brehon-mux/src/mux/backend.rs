//! Backend ownership/status reporting for panes.

use crate::pane::PaneBackend;

use super::Mux;
use super::types::PaneBackendOwnership;

impl Mux {
    /// Return which runtime owns a pane's terminal/process surface.
    pub fn pane_backend_ownership(&self, pane_id: &str) -> Option<PaneBackendOwnership> {
        let pane = self.get(pane_id)?;
        if self.is_panesmith_managed(pane_id) {
            return Some(PaneBackendOwnership::Panesmith);
        }
        if pane.is_gateway_backed() {
            return Some(PaneBackendOwnership::Gateway);
        }
        match &pane.backend {
            PaneBackend::Pty(_) => Some(PaneBackendOwnership::GhosttyVt),
            PaneBackend::None => Some(PaneBackendOwnership::None),
        }
    }
}
