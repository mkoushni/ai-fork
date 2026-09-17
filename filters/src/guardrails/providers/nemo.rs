// SPDX-License-Identifier: Apache-2.0
// Copyright (c) 2026 Praxis Contributors

//! `NeMo` Guardrails provider: calls `/v1/guardrail/checks` and maps
//! the response to [`GuardResult`].

use std::time::Duration;

use async_trait::async_trait;
use bytes::Bytes;
use http::{HeaderMap, HeaderValue, Method};
use praxis_ai_apis::{
    callout_target::{AddressPolicy, validate_configured_http_target, validate_resolved_addrs},
    subrequest::{SubRequest, SubRequestClient, SubResponse},
};
use praxis_core::connectivity::{UrlTargetError, prepare_url_target};
use praxis_filter::{
    CalloutOutcome, CalloutResponse, FilterError, FilteredSubrequestExecutor, RequestExtensions, StagedUpstream,
    StagedUpstreamFallback,
};
use serde::{Deserialize, Serialize};

use super::{GuardCalloutRuntime, GuardPhase, GuardProvider, GuardResult};

/// Default timeout for `NeMo` HTTP calls (10 seconds).
const DEFAULT_TIMEOUT_MS: u64 = 10_000;

/// Maximum response body size accepted from `NeMo` (1 MiB).
const MAX_RESPONSE_SIZE: usize = 1024 * 1024;

/// `NeMo`-specific configuration fields.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct NemoConfig {
    /// `NeMo` endpoint URL.
    endpoint: String,

    /// Model name sent in each request. Defaults to `""` when omitted.
    #[serde(default)]
    model: String,

    /// Per-request timeout in milliseconds.
    #[serde(default = "default_timeout_ms")]
    timeout_ms: u64,
}

/// Returns the default timeout value for serde deserialization.
fn default_timeout_ms() -> u64 {
    DEFAULT_TIMEOUT_MS
}

/// Outgoing request payload for `NeMo`
#[derive(Serialize)]
struct NemoRequest {
    /// Model name
    model: String,
    /// List of messages to evaluate.
    messages: Vec<serde_json::Value>,
}

/// Incoming response payload for `NeMo`
#[derive(Deserialize)]
struct NemoResponse {
    /// Overall verdict: `"success"`, `"blocked"`, or `"error"`.
    status: String,

    /// Per-rail evaluation results. The names of rails whose `status` is
    /// `"blocked"` are joined to form the [`GuardResult::Block::reason`]
    /// string.
    rails_status: Option<serde_json::Value>,

    /// Error details returned when `status` is `"error"`.
    /// Contains `"error"` and optionally `"details"` keys.
    guardrails_data: Option<serde_json::Value>,
}

/// `NeMo` Guardrails provider.
pub(in crate::guardrails) struct NemoProvider {
    /// Shared HTTP client with admission control and circuit breaking.
    client: SubRequestClient,

    /// `NeMo` endpoint URL.
    endpoint: String,

    /// Model name included in every request. Empty string when not configured.
    model: String,

    /// Per-request deadline covering target preparation and the outbound exchange.
    timeout: Duration,
}

impl NemoProvider {
    /// Parse and validate `NeMo`-specific config from the provider settings.
    ///
    /// Uses the provided [`SubRequestClient`] and bound outbound chain so
    /// callouts inherit the runtime's admission control, circuit breaking,
    /// and outbound security filters.
    ///
    /// # Errors
    ///
    /// Returns `FilterError` if the configuration is invalid.
    pub fn from_config(config: &serde_yaml::Value, client: SubRequestClient) -> Result<Self, FilterError> {
        let cfg: NemoConfig = serde_yaml::from_value(config.clone())
            .map_err(|e| -> FilterError { format!("ai_guardrails (nemo): {e}").into() })?;
        if cfg.endpoint.is_empty() {
            return Err("ai_guardrails (nemo): 'endpoint' must not be empty".into());
        }
        validate_configured_http_target(
            "ai_guardrails (nemo)",
            &cfg.endpoint,
            // Construction validates URL shape. The bound outbound pipeline's
            // runtime policy is applied after nested pipelines are built and
            // enforced again against the pinned resolution for every callout.
            AddressPolicy::from_allow_private(true),
        )?;
        if cfg.timeout_ms == 0 {
            return Err("ai_guardrails (nemo): 'timeout_ms' must be greater than zero".into());
        }

        Ok(Self {
            client,
            endpoint: cfg.endpoint,
            model: cfg.model,
            timeout: Duration::from_millis(cfg.timeout_ms),
        })
    }

    /// Return the validated timeout used for each provider callout.
    pub(in crate::guardrails) fn callout_timeout(&self) -> Duration {
        self.timeout
    }
}

#[async_trait]
impl GuardProvider for NemoProvider {
    async fn evaluate(
        &self,
        messages: Vec<serde_json::Value>,
        _phase: GuardPhase,
        runtime: &GuardCalloutRuntime<'_>,
    ) -> Result<GuardResult, FilterError> {
        let request = build_request(&self.model, messages)?;
        let response = Box::pin(self.execute_callout(request, runtime)).await?;
        ensure_success_status(&response)?;
        let nemo_response: NemoResponse = serde_json::from_slice(&response.body)
            .map_err(|e| -> FilterError { format!("ai_guardrails (nemo): failed to parse response: {e}").into() })?;
        map_nemo_response(&nemo_response)
    }
}

#[expect(
    clippy::multiple_inherent_impl,
    reason = "separates construction from the async callout path"
)]
impl NemoProvider {
    /// Execute one `NeMo` request through the bound outbound filter chain.
    #[expect(
        clippy::too_many_lines,
        reason = "target pinning and classified response mapping form one operation"
    )]
    #[expect(
        clippy::large_stack_frames,
        reason = "filtered subrequest executor owns the callout state machine"
    )]
    async fn execute_callout(
        &self,
        request: SubRequest,
        runtime: &GuardCalloutRuntime<'_>,
    ) -> Result<SubResponse, FilterError> {
        let address_policy = AddressPolicy::from_allow_private(runtime.outbound.allow_private_upstreams());
        let target = prepare_url_target(&self.endpoint, runtime.deadline, |addrs| {
            validate_resolved_addrs("ai_guardrails (nemo)", addrs, address_policy)
                .map(|_| ())
                .map_err(|error| -> Box<dyn std::error::Error + Send + Sync> { error.to_string().into() })
        })
        .await
        .map_err(map_url_target_error)?;

        let mut extensions = RequestExtensions::default();
        extensions.insert(StagedUpstream::from_prepared_target(&target)?);
        if target.addresses().len() > 1 {
            extensions.insert(StagedUpstreamFallback::from_prepared_target(&target));
        }
        let prepared = target.bind(request);

        let executor = FilteredSubrequestExecutor::for_callout(
            self.client.clone(),
            runtime.downstream.clone(),
            runtime.depth,
            MAX_RESPONSE_SIZE,
            self.timeout,
        );

        match Box::pin(executor.run_classified(runtime.outbound, prepared.request(), extensions, runtime.deadline))
            .await?
        {
            CalloutOutcome::Response(CalloutResponse::Buffered(response)) => Ok(response),
            CalloutOutcome::Response(CalloutResponse::Streaming { .. }) => {
                Err("ai_guardrails (nemo): streaming NeMo responses are not supported".into())
            },
            CalloutOutcome::ResponseTooLarge { actual, limit } => Err(format!(
                "ai_guardrails (nemo): provider response exceeded size limit ({} bytes, limit {limit} bytes)",
                actual.map_or_else(|| "unknown".to_owned(), |size| size.to_string())
            )
            .into()),
            _ => Err("ai_guardrails (nemo): unsupported provider response mode".into()),
        }
    }
}

// -----------------------------------------------------------------------------
// Private Utilities
// -----------------------------------------------------------------------------

/// Build the outbound `NeMo` JSON callout.
fn build_request(model: &str, messages: Vec<serde_json::Value>) -> Result<SubRequest, FilterError> {
    let payload = NemoRequest {
        model: model.to_owned(),
        messages,
    };
    let body =
        Bytes::from(serde_json::to_vec(&payload).map_err(|e| -> FilterError {
            format!("ai_guardrails (nemo): failed to serialize request: {e}").into()
        })?);

    let mut headers = HeaderMap::new();
    headers.insert(http::header::CONTENT_TYPE, HeaderValue::from_static("application/json"));
    headers.insert(http::header::ACCEPT, HeaderValue::from_static("application/json"));

    Ok(SubRequest {
        method: Method::POST,
        uri: http::Uri::default(),
        headers,
        body,
    })
}

/// Map URL target preparation failures to filter errors.
fn map_url_target_error(error: UrlTargetError) -> FilterError {
    match error {
        UrlTargetError::InvalidTarget(invalid) => {
            format!("ai_guardrails (nemo): invalid endpoint URL: {invalid}").into()
        },
        UrlTargetError::Resolve(resolve) => {
            format!("ai_guardrails (nemo): failed to resolve endpoint: {resolve}").into()
        },
        UrlTargetError::PolicyRejected(source) => {
            format!("ai_guardrails (nemo): endpoint address policy rejected target: {source}").into()
        },
        UrlTargetError::DeadlineExceeded => "ai_guardrails (nemo): endpoint preparation deadline exceeded".into(),
        _ => "ai_guardrails (nemo): endpoint preparation failed".into(),
    }
}

/// Reject non-2xx HTTP responses from the provider.
fn ensure_success_status(response: &SubResponse) -> Result<(), FilterError> {
    if !(200..300).contains(&response.status) {
        return Err(format!(
            "ai_guardrails (nemo): provider returned HTTP status code {}",
            response.status
        )
        .into());
    }
    Ok(())
}

/// Map a deserialized [`NemoResponse`] to a [`GuardResult`].
///
/// The `/v1/guardrail/checks` endpoint returns three statuses:
/// - `"success"` - all rails passed
/// - `"blocked"` - at least one rail blocked the content
/// - `"error"` - `NeMo` internal processing error (fail-closed)
fn map_nemo_response(nemo: &NemoResponse) -> Result<GuardResult, FilterError> {
    match nemo.status.as_str() {
        "success" => Ok(GuardResult::Pass),
        "blocked" => {
            let reason = blocked_rail_names(nemo.rails_status.as_ref());
            Ok(GuardResult::Block { reason })
        },
        "error" => {
            let detail = extract_error_detail(nemo.guardrails_data.as_ref());
            Err(format!("ai_guardrails (nemo): NeMo returned error status: {detail}").into())
        },
        other => Err(format!("ai_guardrails (nemo): unknown status '{other}'").into()),
    }
}

/// Collect the names of all rails whose `status` is `"blocked"` from the
/// `rails_status` map and join them with `", "` in sorted order.
///
/// Returns an empty string if `rails_status` is absent or no rails are blocked.
fn blocked_rail_names(rails_status: Option<&serde_json::Value>) -> String {
    let Some(map) = rails_status.and_then(|v| v.as_object()) else {
        return String::new();
    };
    let mut names: Vec<&str> = map
        .iter()
        .filter(|(_, v)| v.get("status").and_then(|s| s.as_str()) == Some("blocked"))
        .map(|(name, _)| name.as_str())
        .collect();
    names.sort_unstable();
    names.join(", ")
}

/// Extract a human-readable error string from the `guardrails_data` object
/// returned by `NeMo` when `status` is `"error"`.
///
/// Expected shape: `{"error": "...", "details": "..."}`.
fn extract_error_detail(data: Option<&serde_json::Value>) -> String {
    let Some(obj) = data.and_then(|v| v.as_object()) else {
        return "no details available".to_owned();
    };
    let error = obj.get("error").and_then(|v| v.as_str()).unwrap_or("unknown error");
    match obj.get("details").and_then(|v| v.as_str()) {
        Some(details) => format!("{error} ({details})"),
        None => error.to_owned(),
    }
}

// -----------------------------------------------------------------------------
// Tests
// -----------------------------------------------------------------------------

#[cfg(test)]
#[expect(clippy::allow_attributes, reason = "blanket test suppressions")]
#[allow(clippy::unwrap_used, clippy::str_to_string, reason = "tests")]
mod tests {
    use super::*;

    #[test]
    fn blocked_rail_names_sorts_alphabetically() {
        let rails = serde_json::json!({
            "toxicity": {"status": "blocked"},
            "jailbreak": {"status": "blocked"},
            "pii masking": {"status": "blocked"},
        });
        assert_eq!(blocked_rail_names(Some(&rails)), "jailbreak, pii masking, toxicity");
    }

    #[test]
    fn blocked_rail_names_filters_out_non_blocked_rails() {
        let rails = serde_json::json!({
            "toxicity": {"status": "blocked"},
            "jailbreak": {"status": "success"},
        });
        assert_eq!(
            blocked_rail_names(Some(&rails)),
            "toxicity",
            "only rails with status 'blocked' should be included in the reason string"
        );
    }

    #[test]
    fn blocked_rail_names_empty_rails_status_returns_empty_string() {
        let rails = serde_json::json!({});
        assert_eq!(blocked_rail_names(Some(&rails)), "");
    }

    #[test]
    fn blocked_rail_names_absent_map_returns_empty_string() {
        assert_eq!(
            blocked_rail_names(None),
            "",
            "missing rails_status should not panic or error"
        );
    }

    #[test]
    fn blocked_rail_names_non_object_rails_status_returns_empty_string() {
        let rails = serde_json::json!("not an object");
        assert_eq!(blocked_rail_names(Some(&rails)), "");
    }

    #[test]
    fn map_nemo_response_success_returns_pass() {
        let resp = NemoResponse {
            status: "success".to_string(),
            rails_status: None,
            guardrails_data: None,
        };
        let result = map_nemo_response(&resp).unwrap();
        assert!(matches!(result, GuardResult::Pass));
    }

    #[test]
    fn map_nemo_response_blocked_returns_block_with_reason() {
        let resp = NemoResponse {
            status: "blocked".to_string(),
            rails_status: Some(serde_json::json!({
                "toxicity": {"status": "blocked"},
            })),
            guardrails_data: None,
        };
        let result = map_nemo_response(&resp).unwrap();
        assert!(
            matches!(result, GuardResult::Block { reason } if reason == "toxicity"),
            "blocked response should produce GuardResult::Block with rail name as reason"
        );
    }

    #[test]
    fn map_nemo_response_error_extracts_guardrails_data() {
        let resp = NemoResponse {
            status: "error".to_string(),
            rails_status: None,
            guardrails_data: Some(serde_json::json!({
                "error": "Could not load guardrails configuration.",
                "details": "Invalid config path /app/config/nonexistent-config."
            })),
        };
        let err_msg = format!("{}", map_nemo_response(&resp).unwrap_err());
        assert!(
            err_msg.contains("Could not load guardrails configuration."),
            "{err_msg}"
        );
        assert!(err_msg.contains("Invalid config path"), "{err_msg}");
    }

    #[test]
    fn map_nemo_response_error_without_guardrails_data() {
        let resp = NemoResponse {
            status: "error".to_string(),
            rails_status: None,
            guardrails_data: None,
        };
        let err_msg = format!("{}", map_nemo_response(&resp).unwrap_err());
        assert!(err_msg.contains("no details available"), "{err_msg}");
    }

    #[test]
    fn map_nemo_response_error_with_error_key_only() {
        let resp = NemoResponse {
            status: "error".to_string(),
            rails_status: None,
            guardrails_data: Some(serde_json::json!({"error": "Internal failure"})),
        };
        let err_msg = format!("{}", map_nemo_response(&resp).unwrap_err());
        assert!(err_msg.contains("Internal failure"), "{err_msg}");
        assert!(
            !err_msg.contains("Internal failure ("),
            "should not append details in parens when details key is absent: {err_msg}"
        );
    }

    #[test]
    fn map_nemo_response_error_with_missing_error_key() {
        let resp = NemoResponse {
            status: "error".to_string(),
            rails_status: None,
            guardrails_data: Some(serde_json::json!({"unexpected": "shape"})),
        };
        let err_msg = format!("{}", map_nemo_response(&resp).unwrap_err());
        assert!(err_msg.contains("unknown error"), "{err_msg}");
    }

    #[test]
    fn map_nemo_response_unknown_status_returns_error() {
        let resp = NemoResponse {
            status: "garbage".to_string(),
            rails_status: None,
            guardrails_data: None,
        };
        let err_msg = format!("{}", map_nemo_response(&resp).unwrap_err());
        assert!(
            err_msg.contains("unknown status 'garbage'"),
            "unknown status should produce a descriptive error: {err_msg}"
        );
    }
}
