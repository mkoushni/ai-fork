// SPDX-License-Identifier: MIT
// Copyright (c) 2026 Praxis Contributors

//! Shared sub-request client construction from runtime configuration.

use std::time::Duration;

use praxis_core::{
    circuit::CircuitBreakerConfig,
    config::Config,
    subrequest::{SubRequestClient, SubRequestConnector, SubRequestConnectorOptions},
};
use tracing::debug;

// -----------------------------------------------------------------------------
// Constants
// -----------------------------------------------------------------------------

/// How often the sub-request circuit breaker eviction loop runs.
const CIRCUIT_EVICTION_INTERVAL: Duration = Duration::from_secs(300); // 5 min

/// How long a healthy breaker must sit idle before eviction.
const CIRCUIT_IDLE_THRESHOLD: Duration = Duration::from_secs(600); // 10 min

// -----------------------------------------------------------------------------
// Construction
// -----------------------------------------------------------------------------

/// Build the process-wide [`SubRequestClient`] from runtime and body-limit config.
///
/// Maps `subrequest_pool_size`, `subrequest_max_connections`, and
/// `subrequest_circuit_breaker` through one [`SubRequestConnectorOptions`]
/// path so startup and `--validate`/`--dump` cannot drift. Absent circuit-breaker
/// configuration leaves isolation disabled, matching [`SubRequestConnector::new`].
///
/// [`SubRequestClient`]: praxis_core::subrequest::SubRequestClient
/// [`SubRequestConnectorOptions`]: praxis_core::subrequest::SubRequestConnectorOptions
/// [`SubRequestConnector::new`]: praxis_core::subrequest::SubRequestConnector::new
#[must_use]
pub fn create_subrequest_client(config: &Config) -> SubRequestClient {
    let pool_size = config
        .runtime
        .subrequest_pool_size
        .unwrap_or(praxis_core::config::DEFAULT_SUBREQUEST_POOL_SIZE);
    let connector = SubRequestConnector::with_options(SubRequestConnectorOptions {
        keepalive_pool_size: pool_size,
        max_connections: config.runtime.subrequest_max_connections,
        circuit_breaker: config
            .runtime
            .subrequest_circuit_breaker
            .as_ref()
            .map(|cb| CircuitBreakerConfig {
                threshold: cb.consecutive_failures,
                recovery_window: Duration::from_secs(cb.recovery_window_secs),
                half_open_timeout: Duration::from_secs(cb.half_open_timeout_secs),
            }),
    });
    let response_ceiling = config.body_limits.max_response_bytes.unwrap_or(usize::MAX);
    SubRequestClient::with_max_response_bytes(connector, response_ceiling)
}

/// Spawn idle-circuit eviction when `runtime.subrequest_circuit_breaker` is set.
///
/// Interval and idle threshold match the Praxis server so AI and core
/// bound circuit state the same way. No-op when the breaker is absent.
#[expect(clippy::expect_used, reason = "fatal if the eviction runtime cannot start")]
pub(crate) fn spawn_circuit_eviction_if_configured(config: &Config, client: &SubRequestClient) {
    if config.runtime.subrequest_circuit_breaker.is_none() {
        return;
    }
    let client = client.clone();
    std::thread::spawn(move || {
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("circuit breaker eviction runtime");
        rt.block_on(async move {
            let mut interval = tokio::time::interval(CIRCUIT_EVICTION_INTERVAL);
            interval.tick().await;
            loop {
                interval.tick().await;
                let evicted = client.evict_idle_circuits(CIRCUIT_IDLE_THRESHOLD);
                if evicted > 0 {
                    debug!(evicted, "circuit breaker: evicted idle entries");
                }
            }
        });
    });
}

// -----------------------------------------------------------------------------
// Tests
// -----------------------------------------------------------------------------

#[cfg(test)]
#[expect(clippy::allow_attributes, reason = "blanket test suppressions")]
#[allow(clippy::unwrap_used, clippy::expect_used, reason = "tests")]
mod tests {
    use bytes::Bytes;
    use http::HeaderMap;
    use pingora_core::upstreams::peer::HttpPeer;
    use praxis_core::subrequest::{SubRequest, SubRequestError, SubResponse};

    use super::*;

    #[test]
    fn absent_circuit_breaker_leaves_isolation_disabled() {
        let client = create_subrequest_client(&minimal_config(""));
        let debug = format!("{client:?}");
        assert!(
            debug.contains("circuit_breakers: false"),
            "absent runtime.subrequest_circuit_breaker should not install a registry; got {debug}"
        );
        assert_eq!(
            client.evict_idle_circuits(Duration::from_secs(1)),
            0,
            "idle eviction is a no-op without a circuit registry"
        );
    }

    #[test]
    fn configured_circuit_breaker_enables_shared_connector() {
        let client = create_subrequest_client(&minimal_config(
            "
runtime:
  subrequest_pool_size: 16
  subrequest_max_connections: 32
  subrequest_circuit_breaker:
    consecutive_failures: 3
    recovery_window_secs: 30
    half_open_timeout_secs: 15
",
        ));
        let debug = format!("{client:?}");
        assert!(
            debug.contains("circuit_breakers: true"),
            "configured breaker should install the connector registry; got {debug}"
        );
        assert!(
            debug.contains("max_connections: Some(32)"),
            "max_connections should still be applied alongside the breaker; got {debug}"
        );
    }

    #[test]
    fn client_aware_factories_accept_configured_client() {
        let config = minimal_config(
            "
runtime:
  subrequest_circuit_breaker:
    consecutive_failures: 3
    recovery_window_secs: 30
",
        );
        let client = create_subrequest_client(&config);
        let registry = crate::build_full_registry(&client);
        let filter_config = serde_yaml::from_str(
            "
vector_store_url: http://127.0.0.1:9
allow_private_url: true
callout_failure_mode: closed
",
        )
        .unwrap();
        registry
            .create("openai_file_search_callout", &filter_config)
            .expect("file-search factory should build against the shared client");
    }

    #[tokio::test]
    async fn configured_breaker_trips_on_connect_failure() {
        let client = create_subrequest_client(&minimal_config(
            "
runtime:
  subrequest_circuit_breaker:
    consecutive_failures: 1
    recovery_window_secs: 30
    half_open_timeout_secs: 30
",
        ));
        let peer_addr = closed_loopback_addr();
        let first = execute_against(&client, &peer_addr).await;
        assert!(
            matches!(first, Err(SubRequestError::Connect(_))),
            "first refusal should be a connect error that records the failure; got {first:?}"
        );
        let second = execute_against(&client, &peer_addr).await;
        assert!(
            matches!(second, Err(SubRequestError::CircuitOpen { .. })),
            "threshold 1 must fail-fast as CircuitOpen before recovery; got {second:?}"
        );
    }

    #[tokio::test]
    async fn absent_breaker_does_not_fail_fast_as_circuit_open() {
        let client = create_subrequest_client(&minimal_config(""));
        let peer_addr = closed_loopback_addr();
        let first = execute_against(&client, &peer_addr).await;
        assert!(
            matches!(first, Err(SubRequestError::Connect(_))),
            "absent breaker should still surface the connect error; got {first:?}"
        );
        let second = execute_against(&client, &peer_addr).await;
        assert!(
            matches!(second, Err(SubRequestError::Connect(_))),
            "absent breaker must not fail-fast as CircuitOpen; got {second:?}"
        );
    }

    // -------------------------------------------------------------------------
    // Test Utilities
    // -------------------------------------------------------------------------

    fn minimal_config(runtime: &str) -> Config {
        Config::from_yaml(&format!(
            r#"
listeners:
  - name: web
    address: "127.0.0.1:8080"
    filter_chains: [main]
filter_chains:
  - name: main
    filters:
      - filter: static_response
        status: 200
{runtime}
"#
        ))
        .unwrap()
    }

    fn closed_loopback_addr() -> String {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        drop(listener);
        addr.to_string()
    }

    async fn execute_against(client: &SubRequestClient, peer_addr: &str) -> Result<SubResponse, SubRequestError> {
        let peer = HttpPeer::new(peer_addr.to_owned(), false, String::new());
        let request = SubRequest {
            method: http::Method::GET,
            uri: "/".parse().unwrap(),
            headers: HeaderMap::new(),
            body: Bytes::new(),
        };
        Box::pin(client.execute(&peer, &request, 1024, Duration::from_secs(2), None)).await
    }
}
