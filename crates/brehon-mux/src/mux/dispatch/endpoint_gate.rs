use super::super::Mux;

fn normalize_endpoint_key(base_url: &str) -> String {
    base_url.trim().trim_end_matches('/').to_string()
}

impl Mux {
    /// Per-endpoint concurrency backpressure.
    ///
    /// Returns `Some(busy_peers)` when the endpoint this pane targets is already
    /// at its configured `max_concurrency` and the prompt should be deferred.
    /// Fails open whenever the cap or endpoint is unknown.
    pub(in crate::mux) fn endpoint_capacity_backpressure(&self, pane_id: &str) -> Option<usize> {
        let spawn_config = self.panes.get(pane_id)?.gateway_spawn_config.as_ref()?;
        let cap = spawn_config.max_concurrency.filter(|cap| *cap > 0)?;
        let endpoint = normalize_endpoint_key(spawn_config.base_url.as_deref()?);
        let busy_peers = self.count_busy_endpoint_peers(&endpoint, pane_id);
        (busy_peers >= cap).then_some(busy_peers)
    }

    pub(in crate::mux) fn endpoint_has_concurrency_cap(&self, pane_id: &str) -> bool {
        self.panes
            .get(pane_id)
            .and_then(|pane| pane.gateway_spawn_config.as_ref())
            .is_some_and(|config| {
                config.base_url.is_some() && config.max_concurrency.is_some_and(|cap| cap > 0)
            })
    }

    fn count_busy_endpoint_peers(&self, endpoint: &str, exclude_pane: &str) -> usize {
        self.panes
            .iter()
            .filter(|(id, _)| id.as_str() != exclude_pane)
            .filter(|(_, pane)| {
                pane.is_processing()
                    && pane
                        .gateway_spawn_config
                        .as_ref()
                        .and_then(|config| config.base_url.as_deref())
                        .is_some_and(|base_url| normalize_endpoint_key(base_url) == endpoint)
            })
            .count()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::mux::types::AsyncGatewayPromptDispatch;
    use crate::pane::{GatewaySpawnConfig, Generation, Pane};
    use brehon_acp::GatewayProtocol;
    use brehon_types::PromptId;
    use std::time::Instant;

    fn gateway_spawn_config(
        base_url: Option<&str>,
        max_concurrency: Option<usize>,
    ) -> GatewaySpawnConfig {
        GatewaySpawnConfig {
            command: None,
            args: Vec::new(),
            env: Vec::new(),
            cwd: String::new(),
            protocol: GatewayProtocol::OpenAiCompatibleChat,
            tool_prefix: None,
            base_url: base_url.map(str::to_string),
            max_concurrency,
            api_key_env: None,
            headers: Vec::new(),
            model: None,
            sidecar_socket_path: None,
            sidecar_ready_path: None,
            sidecar_connect_timeout_ms: None,
        }
    }

    fn add_gateway_pane(
        mux: &mut Mux,
        name: &str,
        base_url: Option<&str>,
        max_concurrency: Option<usize>,
        busy: bool,
    ) {
        let mut pane = Pane::director(name, 24, 80).unwrap();
        pane.gateway_spawn_config = Some(gateway_spawn_config(base_url, max_concurrency));
        if busy {
            pane.set_pane_busy(
                PromptId::new(format!("p-{name}")),
                Generation::default(),
                Instant::now(),
            );
        }
        mux.add_pane(pane);
    }

    #[test]
    fn no_cap_never_defers_even_with_busy_peer() {
        let mut mux = Mux::new(24, 80);
        add_gateway_pane(&mut mux, "a", Some("http://x/v1"), None, true);
        add_gateway_pane(&mut mux, "b", Some("http://x/v1"), None, false);
        assert_eq!(mux.endpoint_capacity_backpressure("b"), None);
    }

    #[test]
    fn cap_one_defers_when_a_peer_is_busy() {
        let mut mux = Mux::new(24, 80);
        add_gateway_pane(&mut mux, "a", Some("http://x/v1"), Some(1), true);
        add_gateway_pane(&mut mux, "b", Some("http://x/v1"), Some(1), false);
        assert_eq!(mux.endpoint_capacity_backpressure("b"), Some(1));
    }

    #[test]
    fn cap_one_allows_when_all_peers_idle() {
        let mut mux = Mux::new(24, 80);
        add_gateway_pane(&mut mux, "a", Some("http://x/v1"), Some(1), false);
        add_gateway_pane(&mut mux, "b", Some("http://x/v1"), Some(1), false);
        assert_eq!(mux.endpoint_capacity_backpressure("b"), None);
    }

    #[test]
    fn busy_peers_on_other_endpoints_do_not_count() {
        let mut mux = Mux::new(24, 80);
        add_gateway_pane(&mut mux, "a", Some("http://A/v1"), Some(1), true);
        add_gateway_pane(&mut mux, "b", Some("http://B/v1"), Some(1), false);
        assert_eq!(mux.endpoint_capacity_backpressure("b"), None);
    }

    #[test]
    fn cap_two_defers_only_once_two_peers_are_busy() {
        let mut mux = Mux::new(24, 80);
        add_gateway_pane(&mut mux, "a", Some("http://x/v1"), Some(2), true);
        add_gateway_pane(&mut mux, "b", Some("http://x/v1"), Some(2), false);
        assert_eq!(mux.endpoint_capacity_backpressure("b"), None);
        add_gateway_pane(&mut mux, "c", Some("http://x/v1"), Some(2), true);
        assert_eq!(mux.endpoint_capacity_backpressure("b"), Some(2));
    }

    #[test]
    fn missing_base_url_fails_open() {
        let mut mux = Mux::new(24, 80);
        add_gateway_pane(&mut mux, "a", None, Some(1), true);
        add_gateway_pane(&mut mux, "b", None, Some(1), false);
        assert_eq!(mux.endpoint_capacity_backpressure("b"), None);
    }

    #[test]
    fn zero_cap_is_treated_as_unlimited() {
        let mut mux = Mux::new(24, 80);
        add_gateway_pane(&mut mux, "a", Some("http://x/v1"), Some(0), true);
        add_gateway_pane(&mut mux, "b", Some("http://x/v1"), Some(0), false);
        assert_eq!(mux.endpoint_capacity_backpressure("b"), None);
    }

    #[test]
    fn trailing_slash_and_whitespace_share_one_endpoint_pool() {
        let mut mux = Mux::new(24, 80);
        add_gateway_pane(&mut mux, "a", Some("http://x:8080/v1/"), Some(1), true);
        add_gateway_pane(&mut mux, "b", Some(" http://x:8080/v1 "), Some(1), false);
        assert_eq!(mux.endpoint_capacity_backpressure("b"), Some(1));
    }

    #[test]
    fn endpoint_has_concurrency_cap_requires_base_url_and_positive_cap() {
        let mut mux = Mux::new(24, 80);
        add_gateway_pane(&mut mux, "capped", Some("http://x/v1"), Some(1), false);
        add_gateway_pane(&mut mux, "no-cap", Some("http://x/v1"), None, false);
        add_gateway_pane(&mut mux, "zero-cap", Some("http://x/v1"), Some(0), false);
        add_gateway_pane(&mut mux, "no-url", None, Some(1), false);
        assert!(mux.endpoint_has_concurrency_cap("capped"));
        assert!(!mux.endpoint_has_concurrency_cap("no-cap"));
        assert!(!mux.endpoint_has_concurrency_cap("zero-cap"));
        assert!(!mux.endpoint_has_concurrency_cap("no-url"));
    }

    #[test]
    fn begin_async_gateway_delivery_defers_when_endpoint_at_capacity() {
        let mut mux = Mux::new(24, 80);
        add_gateway_pane(
            &mut mux,
            "worker-a",
            Some("http://127.0.0.1:8080/v1"),
            Some(1),
            true,
        );
        add_gateway_pane(
            &mut mux,
            "worker-b",
            Some("http://127.0.0.1:8080/v1"),
            Some(1),
            false,
        );

        let rt = tokio::runtime::Runtime::new().expect("runtime");
        let dispatch = rt
            .block_on(mux.begin_async_gateway_prompt_delivery(rt.handle(), "worker-b", "do work"))
            .expect("dispatch should succeed");
        assert!(
            matches!(dispatch, AsyncGatewayPromptDispatch::Queued { .. }),
            "worker-b must be deferred while worker-a holds the only endpoint slot"
        );
    }
}
