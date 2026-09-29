// SPDX-License-Identifier: Apache-2.0
// Copyright (c) 2026 Praxis Contributors

//! Responses API proxy filter.
//!
//! Body-preparation waypoint in the Responses API filter pipeline.
//! Sits between upstream enrichment filters (`rehydrate`, `openai_tool_parse`)
//! and downstream consumption filters (`stream_events`, `tool_dispatch`).
//! Named `inference` in pipeline configs so branch chains can
//! `rejoin` here for the agentic tool loop.
//!
//! When `ResponsesState` is present in `RequestExtensions`, replaces
//! the request input with `state.messages` only after conversation
//! history has changed it. Provider-owned conversation continuations
//! send only their new message delta. It strips `previous_response_id`
//! and `conversation` only after local rehydration consumes them.
//! OpenAI-managed `prompt` template references fail closed unless the selected
//! upstream declares `application_protocol: openai_responses` and
//! `application_provider: openai`.

mod config;

#[cfg(test)]
#[expect(clippy::allow_attributes, reason = "blanket test suppressions")]
#[allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::indexing_slicing,
    clippy::panic,
    clippy::needless_raw_strings,
    clippy::needless_raw_string_hashes,
    clippy::too_many_lines,
    reason = "tests"
)]
mod tests;

use std::{borrow::Cow, collections::HashSet, fmt};

use async_trait::async_trait;
use base64::Engine as _;
use bytes::Bytes;
use praxis_filter::{
    BodyAccess, BodyMode, FilterAction, FilterError, HttpFilter, HttpFilterContext, SelectedUpstreamBodyOutcome,
    SubRequestResponseMode, body::MAX_JSON_BODY_BYTES, parse_filter_config,
};
use serde::{
    Deserialize, Deserializer,
    de::{IgnoredAny, MapAccess, Visitor},
    ser::SerializeMap as _,
};
use tracing::{debug, trace};

use self::config::{ResponsesProxyConfig, build_config};
use super::{
    body_limits::reject_rewritten_body_too_large, enforce_agentic_stream_guard, error::responses_error_rejection,
    state::ResponsesState,
};
use crate::{classifier::is_responses_create, json_body::SerializedJson};

// -----------------------------------------------------------------------------
// ResponsesProxyFilter
// -----------------------------------------------------------------------------

/// Rebuilds the request body from `ResponsesState` when present.
///
/// Reads the assembled conversation history from
/// `ResponsesState::messages` and replaces the `input` field in
/// the outbound body when it differs from the original normalized
/// input. Strips `previous_response_id` after Praxis resolves it
/// locally via the rehydrate filter.
///
/// When no `ResponsesState` exists, preserves the request body unchanged.
///
/// Non-null `prompt` template references are rejected unless the load balancer
/// selected a cluster declaring `application_protocol: openai_responses` and
/// `application_provider: openai`. The check runs in the selected-upstream body
/// phase, after routing has frozen that application metadata. Missing or
/// different application metadata fails closed.
///
/// This filter always advertises the Praxis streaming capability. When the
/// effective outbound body contains `"stream": true` it selects Praxis's
/// streaming transport; otherwise it selects the buffered transport. There is
/// no operator opt-in — the removed `terminal_streaming` flag is rejected via
/// `deny_unknown_fields` so stale configs fail to build. Classifier metadata
/// remains descriptive client intent; this final serializer owns the transport
/// decision. IRR can resume one downstream stream across response-dependent
/// transitions, but every response-body filter in a step composed with this
/// filter must use `BodyMode::Stream` (or explicitly reject streaming requests)
/// rather than silently buffering them.
///
/// # YAML
///
/// ```yaml
/// filter: openai_responses_proxy
/// ```
///
/// # Full YAML
///
/// ```yaml
/// filter: openai_responses_proxy
/// max_rewritten_body_bytes: 67108864
/// ```
///
/// # Example
///
/// ```rust
/// use praxis_ai_apis::openai::ResponsesProxyFilter;
///
/// let yaml = serde_yaml::Value::Null;
/// let filter = ResponsesProxyFilter::from_config(&yaml).unwrap();
/// assert_eq!(filter.name(), "openai_responses_proxy");
/// ```
pub struct ResponsesProxyFilter {
    /// Parsed and validated configuration.
    config: ResponsesProxyConfig,
}

impl ResponsesProxyFilter {
    /// Reject prompt templates unless the selected cluster declares OpenAI.
    fn reject_prompt_for_non_openai_upstream(
        ctx: &HttpFilterContext<'_>,
        body: &Option<Bytes>,
    ) -> Option<SelectedUpstreamBodyOutcome> {
        (is_responses_create(&ctx.request.method, ctx.request.uri.path())
            && request_has_prompt_template(ctx, body)
            && !is_openai_responses_provider(ctx))
        .then(|| {
            debug!("rejecting prompt template for non-OpenAI Responses backend");
            SelectedUpstreamBodyOutcome::Reject(responses_error_rejection(
                400,
                "invalid_request_error",
                "prompt templates are supported only when the selected upstream declares application_protocol: openai_responses and application_provider: openai",
            ))
        })
    }

    /// Create from parsed YAML config.
    ///
    /// # Errors
    ///
    /// Returns [`FilterError`] if the YAML config contains unknown fields.
    ///
    /// [`FilterError`]: praxis_filter::FilterError
    pub fn from_config(config: &serde_yaml::Value) -> Result<Box<dyn HttpFilter>, FilterError> {
        let cfg: ResponsesProxyConfig = if config.is_null() {
            ResponsesProxyConfig::default()
        } else {
            parse_filter_config("openai_responses_proxy", config)?
        };
        let validated = build_config(cfg)?;
        Ok(Box::new(Self { config: validated }))
    }

    /// Serialize the rebuilt body from conversation state.
    fn serialize_body(
        &self,
        state: &ResponsesState,
        preserve_native_compaction: bool,
    ) -> Result<Result<Vec<u8>, FilterAction>, FilterError> {
        let serialized = serialize_outbound_body(state, preserve_native_compaction)
            .map_err(|e| -> FilterError { format!("openai_responses_proxy: {e}").into() })?;
        if serialized.len() > self.config.max_rewritten_body_bytes {
            debug!(
                body_bytes = serialized.len(),
                max_bytes = self.config.max_rewritten_body_bytes,
                "rebuilt request body exceeds maximum size"
            );
            return Ok(Err(reject_rewritten_body_too_large(
                serialized.len(),
                self.config.max_rewritten_body_bytes,
            )));
        }

        debug!(
            messages = state.messages.len(),
            body_bytes = serialized.len(),
            "rebuilt request body from ResponsesState"
        );

        Ok(Ok(serialized))
    }

    /// Reconcile the selected backend's compaction projection with the body
    /// produced by the earlier request-body phase.
    ///
    /// The selected-upstream phase runs after other request-body filters. Use
    /// that live body as the source so downstream changes, such as a model
    /// rewrite, are retained instead of rebuilding from the older
    /// [`ResponsesState`] snapshot.
    fn serialize_selected_body(
        &self,
        body: &Bytes,
        state: &ResponsesState,
    ) -> Result<Result<Vec<u8>, FilterAction>, FilterError> {
        let mut outbound: serde_json::Value = serde_json::from_slice(body).map_err(|e| -> FilterError {
            format!("openai_responses_proxy: invalid selected request body: {e}").into()
        })?;

        if let Some(object) = outbound.as_object_mut() {
            if state.history_rehydrated {
                object.remove("previous_response_id");
                object.remove("conversation");
            }

            if let Some(items) = object.get("input").and_then(serde_json::Value::as_array) {
                let projected = messages_for_backend(items, false, &state.provider_compaction_ids);
                if let Cow::Owned(items) = projected {
                    object.insert("input".to_owned(), serde_json::Value::Array(items));
                }
            }
        }

        let serialized = serde_json::to_vec(&outbound)
            .map_err(|e| -> FilterError { format!("openai_responses_proxy: {e}").into() })?;
        if serialized.len() > self.config.max_rewritten_body_bytes {
            return Ok(Err(reject_rewritten_body_too_large(
                serialized.len(),
                self.config.max_rewritten_body_bytes,
            )));
        }
        Ok(Ok(serialized))
    }
}

#[expect(
    clippy::too_many_lines,
    reason = "request and selected-upstream body phases share one filter implementation"
)]
#[async_trait]
impl HttpFilter for ResponsesProxyFilter {
    fn name(&self) -> &'static str {
        "openai_responses_proxy"
    }

    fn selected_upstream_request_body_access(&self) -> BodyAccess {
        BodyAccess::ReadWrite
    }

    fn request_body_mode(&self) -> BodyMode {
        // Accept up to the absolute ceiling; the pipeline's body_limits
        // decides the real raw cap. max_rewritten_body_bytes bounds only
        // the body rebuilt from ResponsesState.
        BodyMode::StreamBuffer {
            max_bytes: Some(MAX_JSON_BODY_BYTES),
        }
    }

    fn may_select_streaming_subrequest_response(&self) -> bool {
        // Always advertise the capability: transport follows the effective
        // outbound `stream` field, chosen per-request after upstream selection.
        // There is no operator opt-in. A runtime guard in Praxis still
        // validates the actual streaming terminal action.
        true
    }

    async fn on_request(&self, _ctx: &mut HttpFilterContext<'_>) -> Result<FilterAction, FilterError> {
        Ok(FilterAction::Continue)
    }

    async fn on_request_body(
        &self,
        ctx: &mut HttpFilterContext<'_>,
        body: &mut Option<Bytes>,
        end_of_stream: bool,
    ) -> Result<FilterAction, FilterError> {
        if !end_of_stream {
            trace!("buffering request body chunk");
            return Ok(FilterAction::Continue);
        }

        let Some(state) = ctx.extensions.get::<ResponsesState>() else {
            select_terminal_response_mode(ctx, body);
            return Ok(FilterAction::Continue);
        };

        if !request_needs_rebuild(state) {
            select_terminal_response_mode(ctx, body);
            return Ok(FilterAction::Continue);
        }

        let serialized = match self.serialize_body(state, true)? {
            Ok(bytes) => bytes,
            Err(action) => return Ok(action),
        };
        SerializedJson::from_bytes(serialized).commit(body, self.name(), "body");
        select_terminal_response_mode(ctx, body);
        Ok(FilterAction::Continue)
    }

    async fn on_selected_upstream_request_body(
        &self,
        ctx: &mut HttpFilterContext<'_>,
        body: &mut Option<Bytes>,
    ) -> Result<SelectedUpstreamBodyOutcome, FilterError> {
        if let Some(action) = Self::reject_prompt_for_non_openai_upstream(ctx, body) {
            return Ok(action);
        }
        let preserve_native_compaction = is_openai_responses_provider(ctx);
        let Some(state) = ctx.extensions.get::<ResponsesState>() else {
            select_terminal_response_mode(ctx, body);
            if let Some(rejection) = enforce_agentic_stream_guard(ctx) {
                return Ok(SelectedUpstreamBodyOutcome::Reject(rejection));
            }
            debug!("no ResponsesState in extensions, passthrough");
            return Ok(SelectedUpstreamBodyOutcome::Continue);
        };

        if !request_needs_rebuild(state) {
            select_terminal_response_mode(ctx, body);
            if let Some(rejection) = enforce_agentic_stream_guard(ctx) {
                return Ok(SelectedUpstreamBodyOutcome::Reject(rejection));
            }
            debug!("ResponsesState does not require an outbound rewrite, passthrough");
            return Ok(SelectedUpstreamBodyOutcome::Continue);
        }

        // The pre-selection body already carries the provider-native
        // projection. Leave it intact for a native OpenAI Responses backend;
        // reserializing here would both allocate unnecessarily and risk
        // replacing changes made by later body filters.
        if preserve_native_compaction {
            return Ok(SelectedUpstreamBodyOutcome::Continue);
        }

        let Some(current_body) = body.as_ref() else {
            return Ok(SelectedUpstreamBodyOutcome::Continue);
        };
        let serialized = match self.serialize_selected_body(current_body, state)? {
            Ok(bytes) => bytes,
            Err(FilterAction::Reject(rejection)) => return Ok(SelectedUpstreamBodyOutcome::Reject(rejection)),
            Err(_) => return Err("openai_responses_proxy: invalid selected-upstream body outcome".into()),
        };

        SerializedJson::from_bytes(serialized).commit(body, self.name(), "body");
        select_terminal_response_mode(ctx, body);
        if let Some(rejection) = enforce_agentic_stream_guard(ctx) {
            return Ok(SelectedUpstreamBodyOutcome::Reject(rejection));
        }

        Ok(SelectedUpstreamBodyOutcome::Continue)
    }
}

/// Narrow deserialization target for the provider-visible stream bit.
///
/// Only `stream` participates in transport selection; all other request fields
/// are intentionally ignored.
#[derive(Deserialize)]
struct EffectiveResponseMode {
    /// Whether the effective outbound Responses request asks for SSE.
    #[serde(default)]
    stream: bool,
}

/// Allocation-free result of validating a request while locating `prompt`.
struct PromptTemplateProbe {
    /// Whether at least one top-level `prompt` value was non-null.
    present: bool,
}

impl<'de> Deserialize<'de> for PromptTemplateProbe {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        deserializer.deserialize_map(PromptTemplateProbeVisitor)
    }
}

/// Streaming visitor that validates the full object without retaining values.
struct PromptTemplateProbeVisitor;

impl<'de> Visitor<'de> for PromptTemplateProbeVisitor {
    type Value = PromptTemplateProbe;

    fn expecting(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("a Responses request object")
    }

    fn visit_map<A>(self, mut map: A) -> Result<Self::Value, A::Error>
    where
        A: MapAccess<'de>,
    {
        let mut present = false;
        while let Some(field) = map.next_key::<PromptTemplateField>()? {
            match field {
                PromptTemplateField::Prompt => {
                    present |= map.next_value::<Option<IgnoredAny>>()?.is_some();
                },
                PromptTemplateField::Other => {
                    map.next_value::<IgnoredAny>()?;
                },
            }
        }
        Ok(PromptTemplateProbe { present })
    }
}

/// Top-level field discriminator that never retains field names.
enum PromptTemplateField {
    /// The OpenAI-managed prompt template field.
    Prompt,
    /// Every other request field.
    Other,
}

impl<'de> Deserialize<'de> for PromptTemplateField {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        deserializer.deserialize_identifier(PromptTemplateFieldVisitor)
    }
}

/// Borrowing visitor for the top-level field discriminator.
struct PromptTemplateFieldVisitor;

impl Visitor<'_> for PromptTemplateFieldVisitor {
    type Value = PromptTemplateField;

    fn expecting(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("a JSON object field")
    }

    fn visit_str<E>(self, v: &str) -> Result<Self::Value, E>
    where
        E: serde::de::Error,
    {
        Ok(if v == "prompt" {
            PromptTemplateField::Prompt
        } else {
            PromptTemplateField::Other
        })
    }
}

// -----------------------------------------------------------------------------
// Helpers
// -----------------------------------------------------------------------------

/// Whether raw JSON carries a non-null top-level `prompt`.
///
/// The visitor validates the top-level object and discards every value via
/// [`IgnoredAny`], which `serde_json` skips iteratively — so a non-null `prompt`
/// is detected at any nesting depth without recursion, a depth limit, or
/// retained allocation. A body that is not valid JSON is not attributed to
/// prompt templates: it cannot carry a prompt that a strict OpenAI-compatible
/// backend would parse and honor, and rejecting a malformed request belongs to
/// normal request validation, not this prompt-template guard.
fn raw_request_has_prompt(body: &[u8]) -> bool {
    serde_json::from_slice::<PromptTemplateProbe>(body).is_ok_and(|probe| probe.present)
}

/// Whether the canonical or passthrough request carries a non-null `prompt`.
fn request_has_prompt_template(ctx: &HttpFilterContext<'_>, body: &Option<Bytes>) -> bool {
    if let Some(state) = ctx.extensions.get::<ResponsesState>() {
        return state.request_body.get("prompt").is_some_and(|prompt| !prompt.is_null());
    }

    body.as_deref().is_some_and(raw_request_has_prompt)
}

/// Whether the selected cluster explicitly supports OpenAI Responses behavior.
fn is_openai_responses_provider(ctx: &HttpFilterContext<'_>) -> bool {
    ctx.selected_application_protocol() == Some("openai_responses")
        && ctx.selected_application_provider() == Some("openai")
}

/// Align the typed Praxis response mode with the effective serialized request.
///
/// Classifier metadata describes client intent, but request transformations can
/// change the provider-visible body. The final serializer therefore owns this
/// transport decision and reads the bytes it actually leaves for the upstream.
fn select_terminal_response_mode(ctx: &mut HttpFilterContext<'_>, body: &Option<Bytes>) {
    let mode = if body
        .as_deref()
        .and_then(|bytes| serde_json::from_slice::<EffectiveResponseMode>(bytes).ok())
        .is_some_and(|selection| selection.stream)
    {
        SubRequestResponseMode::Streaming
    } else {
        SubRequestResponseMode::Buffered
    };
    ctx.set_subrequest_response_mode(mode);
}

/// Borrowed view of the outbound request body.
///
/// This keeps the original request and message history borrowed while
/// replacing `input` and omitting locally consumed fields during
/// serialization, avoiding full-body and message clones.
struct OutboundBody<'a> {
    /// Shared request state to project into the provider body.
    state: &'a ResponsesState,
    /// Preserve provider-native compaction items instead of translating them
    /// to Chat-style assistant messages.
    preserve_native_compaction: bool,
    /// IDs of compaction items known to have come from a provider response.
    provider_compaction_ids: &'a HashSet<String>,
}

impl serde::Serialize for OutboundBody<'_> {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: serde::Serializer,
    {
        let Some(object) = self.state.request_body.as_object() else {
            return self.state.request_body.serialize(serializer);
        };

        let messages = if provider_owns_conversation(self.state) && self.state.iteration > 0 {
            self.state
                .messages
                .get(self.state.provider_history_len..)
                .unwrap_or_default()
        } else {
            &self.state.messages
        };
        let backend_messages =
            messages_for_backend(messages, self.preserve_native_compaction, self.provider_compaction_ids);
        let mut map = serializer.serialize_map(None)?;
        let mut wrote_input = false;
        for (name, value) in object {
            match name.as_str() {
                "input" => {
                    map.serialize_entry(name, backend_messages.as_ref())?;
                    wrote_input = true;
                },
                "previous_response_id" | "conversation" if self.state.history_rehydrated => {},
                _ => map.serialize_entry(name, value)?,
            }
        }
        if !wrote_input {
            map.serialize_entry("input", backend_messages.as_ref())?;
        }
        map.end()
    }
}

/// Whether the upstream provider, rather than local rehydration, owns history.
fn provider_owns_conversation(state: &ResponsesState) -> bool {
    !state.history_rehydrated
        && state
            .conversation
            .as_ref()
            .is_some_and(|conversation| !conversation.is_null())
}

/// Serialize the outbound body without cloning request state.
fn serialize_outbound_body(
    state: &ResponsesState,
    preserve_native_compaction: bool,
) -> Result<Vec<u8>, serde_json::Error> {
    serde_json::to_vec(&OutboundBody {
        state,
        preserve_native_compaction,
        provider_compaction_ids: &state.provider_compaction_ids,
    })
}

/// Project compaction items into the selected backend's input format.
///
/// Returns `Cow::Borrowed` when no compaction items are present, avoiding
/// allocation. Native mode borrows only provider-originated compaction items;
/// locally generated Praxis summaries are still translated to assistant
/// messages because they are not opaque provider state.
fn messages_for_backend<'a>(
    messages: &'a [serde_json::Value],
    preserve_native_compaction: bool,
    provider_compaction_ids: &HashSet<String>,
) -> Cow<'a, [serde_json::Value]> {
    let mut translated: Option<Vec<serde_json::Value>> = None;

    for (i, m) in messages.iter().enumerate() {
        let is_provider_compaction = preserve_native_compaction
            && m.get("type").and_then(serde_json::Value::as_str) == Some("compaction")
            && m.get("id")
                .and_then(serde_json::Value::as_str)
                .is_some_and(|id| provider_compaction_ids.contains(id));
        if m.get("type").and_then(serde_json::Value::as_str) == Some("compaction") && !is_provider_compaction {
            let vec = translated.get_or_insert_with(|| messages.get(..i).unwrap_or(&[]).to_vec());
            vec.push(compaction_to_assistant_message(m));
        } else if let Some(vec) = &mut translated {
            vec.push(m.clone());
        }
    }

    match translated {
        Some(vec) => Cow::Owned(vec),
        None => Cow::Borrowed(messages),
    }
}

/// Translate a compaction item to a Chat Completions assistant message.
fn compaction_to_assistant_message(m: &serde_json::Value) -> serde_json::Value {
    let summary = m
        .get("encrypted_content")
        .and_then(serde_json::Value::as_str)
        .and_then(|e| base64::engine::general_purpose::STANDARD.decode(e).ok())
        .and_then(|b| String::from_utf8(b).ok())
        .unwrap_or_default();
    let prefix = m
        .get("summary_prefix")
        .and_then(serde_json::Value::as_str)
        .unwrap_or(crate::openai::translation::chat_completions::DEFAULT_SUMMARY_PREFIX);
    serde_json::json!({
        "role": "assistant",
        "content": format!("{prefix}{summary}")
    })
}

/// Count the exact serialized bytes for both outbound compaction projections.
///
/// The selected provider is not known when pre-selection rewrite filters
/// enforce their configured cap. Measuring both the native and translated
/// forms prevents the smaller translated assistant placeholder from masking a
/// larger opaque provider item that the selected-upstream hook will preserve.
pub(super) fn serialized_outbound_body_len(state: &ResponsesState) -> Result<usize, serde_json::Error> {
    let translated = serialized_outbound_body_len_for(state, false)?;
    let native = serialized_outbound_body_len_for(state, true)?;
    Ok(translated.max(native))
}

/// Count one concrete outbound projection without allocating the body.
fn serialized_outbound_body_len_for(
    state: &ResponsesState,
    preserve_native_compaction: bool,
) -> Result<usize, serde_json::Error> {
    let mut counter = ByteCounter::default();
    serde_json::to_writer(
        &mut counter,
        &OutboundBody {
            state,
            preserve_native_compaction,
            provider_compaction_ids: &state.provider_compaction_ids,
        },
    )?;
    Ok(counter.bytes)
}

/// Writer that counts serialized bytes without allocating a second body.
#[derive(Default)]
struct ByteCounter {
    /// Number of bytes written by the serializer.
    bytes: usize,
}

impl std::io::Write for ByteCounter {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        self.bytes = self.bytes.saturating_add(buf.len());
        Ok(buf.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

/// Return whether state requires parsing and rebuilding the outbound body.
fn request_needs_rebuild(state: &ResponsesState) -> bool {
    state.request_body_requires_rebuild()
        || state.messages != state.input
        || (state.history_rehydrated
            && (state.previous_response_id.is_some()
                || state.conversation.is_some()
                || state.request_body.get("previous_response_id").is_some()
                || state.request_body.get("conversation").is_some()))
}
