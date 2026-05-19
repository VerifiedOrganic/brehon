//! Permission mediation.
//!
//! Handles permission requests from agents and policy-based approval/denial.

use std::collections::{HashMap, VecDeque};
use std::sync::Arc;

use tokio::sync::{oneshot, Mutex};

use brehon_types::SessionId;

#[derive(Debug, Clone)]
pub struct PermissionRequest {
    #[allow(dead_code)]
    pub session_id: SessionId,
    pub permission_id: String,
    pub action: String,
    #[allow(dead_code)]
    pub details: Option<serde_json::Value>,
    #[allow(dead_code)]
    pub timestamp: chrono::DateTime<chrono::Utc>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PermissionDecision {
    Approved,
    Denied,
    Ask,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PermissionResolution {
    Resolved,
    Expired,
    NotFound,
    InvalidDecision,
}

#[derive(Debug, Clone)]
pub struct PermissionPolicy {
    #[allow(dead_code)]
    pub(crate) allow_read: bool,
    #[allow(dead_code)]
    pub(crate) allow_write: Vec<String>,
    #[allow(dead_code)]
    pub(crate) allow_execute: Vec<String>,
    #[allow(dead_code)]
    pub(crate) allow_network: bool,
    #[allow(dead_code)]
    pub(crate) allow_system: bool,
    #[allow(dead_code)]
    pub(crate) custom_rules: HashMap<String, PermissionDecision>,
    pub(crate) default_decision: PermissionDecision,
    pub(crate) timeout_decision: PermissionDecision,
}

impl Default for PermissionPolicy {
    fn default() -> Self {
        Self {
            allow_read: true,
            allow_write: vec!["**".to_string()],
            allow_execute: vec![],
            allow_network: false,
            allow_system: false,
            custom_rules: HashMap::new(),
            default_decision: PermissionDecision::Ask,
            timeout_decision: PermissionDecision::Denied,
        }
    }
}

#[derive(Debug)]
pub struct PendingPermission {
    #[allow(dead_code)]
    pub request: PermissionRequest,
    pub responded: bool,
    response_tx: Option<oneshot::Sender<PermissionDecision>>,
}

#[derive(Debug, Default)]
struct PermissionState {
    pending: HashMap<String, PendingPermission>,
    expired: VecDeque<String>,
}

pub struct PermissionManager {
    policy: PermissionPolicy,
    state: Arc<Mutex<PermissionState>>,
}

impl PermissionManager {
    pub fn new(policy: PermissionPolicy) -> Self {
        Self {
            policy,
            state: Arc::new(Mutex::new(PermissionState::default())),
        }
    }

    fn remember_expired(expired: &mut VecDeque<String>, permission_id: &str) {
        const MAX_TRACKED_EXPIRED: usize = 256;

        if let Some(index) = expired.iter().position(|id| id == permission_id) {
            expired.remove(index);
        }
        expired.push_back(permission_id.to_string());
        if expired.len() > MAX_TRACKED_EXPIRED {
            expired.pop_front();
        }
    }

    /// Returns the effective policy decision for a request.
    ///
    /// Note: the coarse-grained `allow_*` fields are reserved for future policy
    /// wiring. Today only per-action `custom_rules` and `default_decision`
    /// influence the mediation result.
    pub fn decision_for_request(&self, request: &PermissionRequest) -> PermissionDecision {
        self.policy
            .custom_rules
            .get(&request.action)
            .copied()
            .unwrap_or(self.policy.default_decision)
    }

    pub fn timeout_decision(&self) -> PermissionDecision {
        self.policy.timeout_decision
    }

    pub async fn register_request(
        &self,
        request: PermissionRequest,
    ) -> oneshot::Receiver<PermissionDecision> {
        let (response_tx, response_rx) = oneshot::channel();
        let permission_id = request.permission_id.clone();
        let mut state = self.state.lock().await;
        if let Some(index) = state.expired.iter().position(|id| id == &permission_id) {
            state.expired.remove(index);
        }
        let pending = PendingPermission {
            request,
            responded: false,
            response_tx: Some(response_tx),
        };
        state.pending.insert(permission_id, pending);
        response_rx
    }

    pub async fn resolve(
        &self,
        permission_id: &str,
        decision: PermissionDecision,
    ) -> PermissionResolution {
        if matches!(decision, PermissionDecision::Ask) {
            return PermissionResolution::InvalidDecision;
        }

        let mut state = self.state.lock().await;
        let Some(mut entry) = state.pending.remove(permission_id) else {
            return if state.expired.iter().any(|id| id == permission_id) {
                PermissionResolution::Expired
            } else {
                PermissionResolution::NotFound
            };
        };
        entry.responded = true;
        if let Some(response_tx) = entry.response_tx.take() {
            if response_tx.send(decision).is_ok() {
                // Release the lock before returning; the entry is fully consumed now.
                drop(state);
                PermissionResolution::Resolved
            } else {
                Self::remember_expired(&mut state.expired, permission_id);
                PermissionResolution::Expired
            }
        } else {
            Self::remember_expired(&mut state.expired, permission_id);
            PermissionResolution::Expired
        }
    }

    pub async fn expire(&self, permission_id: &str) {
        let mut state = self.state.lock().await;
        if state.pending.remove(permission_id).is_some() {
            Self::remember_expired(&mut state.expired, permission_id);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;
    use tokio::sync::Barrier;

    fn test_request(permission_id: &str) -> PermissionRequest {
        PermissionRequest {
            session_id: SessionId::new("session-1"),
            permission_id: permission_id.to_string(),
            action: "bash".to_string(),
            details: None,
            timestamp: chrono::Utc::now(),
        }
    }

    #[test]
    fn test_permission_policy_default() {
        let policy = PermissionPolicy::default();
        assert!(policy.allow_read);
        assert_eq!(policy.default_decision, PermissionDecision::Ask);
        assert_eq!(policy.timeout_decision, PermissionDecision::Denied);
    }

    #[tokio::test]
    async fn test_permission_manager_resolves_registered_request() {
        let manager = PermissionManager::new(PermissionPolicy::default());
        let request = test_request("perm-1");

        let response_rx = manager.register_request(request).await;
        assert_eq!(
            manager
                .resolve("perm-1", PermissionDecision::Approved)
                .await,
            PermissionResolution::Resolved
        );
        assert_eq!(
            response_rx
                .await
                .expect("permission response should resolve"),
            PermissionDecision::Approved
        );
    }

    #[tokio::test]
    async fn test_permission_manager_rejects_non_terminal_resolution() {
        let manager = PermissionManager::new(PermissionPolicy::default());
        let request = test_request("perm-1");

        let _response_rx = manager.register_request(request).await;
        assert_eq!(
            manager.resolve("perm-1", PermissionDecision::Ask).await,
            PermissionResolution::InvalidDecision
        );
    }

    #[tokio::test]
    async fn test_permission_manager_reports_expired_when_receiver_is_gone() {
        let manager = PermissionManager::new(PermissionPolicy::default());
        let request = test_request("perm-1");

        let response_rx = manager.register_request(request).await;
        drop(response_rx);

        assert_eq!(
            manager
                .resolve("perm-1", PermissionDecision::Approved)
                .await,
            PermissionResolution::Expired
        );
    }

    #[tokio::test]
    async fn test_permission_manager_reports_expired_after_expire() {
        let manager = PermissionManager::new(PermissionPolicy::default());
        let _response_rx = manager.register_request(test_request("perm-1")).await;

        manager.expire("perm-1").await;

        assert_eq!(
            manager
                .resolve("perm-1", PermissionDecision::Approved)
                .await,
            PermissionResolution::Expired
        );
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn test_permission_manager_concurrent_expire_and_resolve_never_report_not_found() {
        for attempt in 0..256 {
            let manager = Arc::new(PermissionManager::new(PermissionPolicy::default()));
            let permission_id = format!("perm-{attempt}");
            // Hold receiver to keep the oneshot channel open for resolve().
            let _response_rx = manager.register_request(test_request(&permission_id)).await;
            let barrier = Arc::new(Barrier::new(2));

            let expire_manager = Arc::clone(&manager);
            let expire_permission_id = permission_id.clone();
            let expire_barrier = Arc::clone(&barrier);
            let expire_task = tokio::spawn(async move {
                expire_barrier.wait().await;
                expire_manager.expire(&expire_permission_id).await;
            });

            let resolve_manager = Arc::clone(&manager);
            let resolve_permission_id = permission_id.clone();
            let resolve_barrier = Arc::clone(&barrier);
            let resolve_task = tokio::spawn(async move {
                resolve_barrier.wait().await;
                tokio::task::yield_now().await;
                resolve_manager
                    .resolve(&resolve_permission_id, PermissionDecision::Approved)
                    .await
            });

            expire_task.await.expect("expire task should complete");
            let resolution = resolve_task.await.expect("resolve task should complete");
            assert!(
                matches!(
                    resolution,
                    PermissionResolution::Resolved | PermissionResolution::Expired
                ),
                "late resolve should never observe NotFound, got {resolution:?}"
            );
        }
    }
}
