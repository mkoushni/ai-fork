---
issue: https://github.com/praxis-proxy/ai/issues/778
discussion: >-
  Sub-task of praxis-proxy/ai#762 (itself a sub-task of praxis-proxy/ai#114,
  under Epic praxis-proxy/ai#363). The What?/Why? for provider translation in
  general — including AWS Bedrock as one of the four in-scope providers — was
  already discussed and accepted in
  docs/proposals/00762_api-translation.md. This proposal narrows that accepted
  scope to Bedrock specifically and adds the How?, per praxis-proxy/ai#762
  being split into one per-provider sub-task (praxis-proxy/ai#778-#781).
status: proposed
authors:
  - mkoushni
graduation_criteria:
  - "Bedrock request/response/error/stream fixture manifest reviewed by stakeholders"
  - "InvokeModel model-family scope (Anthropic-shaped body reuse vs. new adapters) reviewed by stakeholders"
  - "Bedrock API key rotation/hot-reload strategy for credential_inject reviewed by stakeholders"
stakeholders:
  - # TODO: add relevant maintainer/domain-expert handles before merge
---

# API Translation: AWS Bedrock (Converse + InvokeModel)

## What?

Add a Bedrock-specific translation stage to the Praxis filter pipeline that
bidirectionally rewrites OpenAI Chat Completions-shaped traffic to and from
both AWS Bedrock inference APIs — **Converse** and **InvokeModel** — including
their streaming variants, which use Bedrock's binary
`application/vnd.amazon.eventstream` framing rather than SSE. This is the
Bedrock-scoped instance of the general translation stage already accepted in
[docs/proposals/00762_api-translation.md](00762_api-translation.md); this
proposal adds the **How?** for
[praxis-proxy/ai#778](https://github.com/praxis-proxy/ai/issues/778) only.

### Goals

- Translate OpenAI Chat Completions requests to Bedrock Converse and
  InvokeModel request bodies, and translate Bedrock responses (including
  errors) back to OpenAI Chat Completions shape.
- Decode Bedrock's binary event-stream framing (`converse-stream`,
  `invoke-with-response-stream`) incrementally, across arbitrary chunk
  boundaries, and re-emit it to the consumer as OpenAI-shaped SSE.
- Derive the Bedrock model path segment from a locally validated,
  operator-owned allowlist rather than an unchecked consumer string, closing
  the path-injection risk that coincides with SigV4 removal.
- Strip 100% of consumer-supplied SigV4 material — headers and query
  parameters — before the request leaves Praxis, whether or not the consumer
  sent any.
- Reuse existing, tested building blocks (`aws-smithy-eventstream`,
  `percent-encoding`, `credential_inject`, `apis/src/anthropic/*` mapping
  code) instead of re-implementing binary framing, path encoding, credential
  injection, or content-block mapping that this codebase (or the Rust
  ecosystem) already solved.

## Why?

### Motivation

Bedrock is the most structurally different of the four providers in
praxis-proxy/ai#762's scope, for three concrete reasons this proposal has to
address:

1. **Two inference APIs, not one.** Converse is Bedrock's own
   cross-model-family unification (closest in spirit to what Praxis is
   building); InvokeModel is the older, per-model-family API where the
   request/response body schema is defined by the model provider, not by
   Bedrock itself.
2. **Model identity is a URL path segment, not a body field.** Every other
   provider in scope (Vertex, Azure, Cohere) keeps the model in the request
   body or, for Azure, in a deployment name already bound to the config.
   Bedrock's Converse/InvokeModel paths are `/model/{modelId}/...`, so
   translating a request means the model name has to become part of the
   *route*, not just the payload — while still honoring 00762's rule that
   provider/route identity may never be taken uncritically from
   consumer-controlled input.
3. **The streaming transport is binary, not text.** `application/vnd.amazon.eventstream`
   is a length-prefixed, CRC32-checked binary framing — not `text/event-stream`.
   Reusing the SSE line-parser used for Azure/Cohere/Vertex is not an option;
   this needs its own decoder, and it needs to not be a hand-rolled one given
   how easy it is to get CRC/length framing subtly wrong.

### User Stories

(Inherits the general user stories from 00762 — consumer stability, operator
config-only provider swaps, transparent streaming, provider-identity
integrity, fail-closed translation. Adding the Bedrock-specific one:)

- As a **platform operator**, I want to route to Bedrock Converse for new
  integrations and InvokeModel for existing model-specific integrations
  without running two separate gateways, so that both API generations are
  available behind the same OpenAI-shaped consumer contract.

## How?

### Requirements

Restating praxis-proxy/ai#778's acceptance criteria as requirements this
design must satisfy:

1. Request transform OpenAI → Bedrock Converse, fixture-backed.
2. Request transform OpenAI → Bedrock InvokeModel, fixture-backed.
3. Response transform Bedrock → OpenAI (both APIs), fixture-backed.
4. Error-response transform, fixture-backed.
5. Streaming transform across arbitrary chunk boundaries using the
   `application/vnd.amazon.eventstream` binary decoder; event framing and
   ordering preserved.
6. Provider identity derived exclusively from the trusted route result;
   unrecognized/missing provider rejected before translation.
7. Consumer-supplied SigV4 credentials stripped from headers **and** query
   params, verified by a fixture.
8. Deterministic, documented header and body mutation ordering.
9. Translation runs only after the caller/model authorization stage.
10. Fixtures carry provenance and are scanned for real credential material.
11. End-to-end "capital of France" test against a real Bedrock endpoint or a
    strict protocol simulator.
12. Example config in `examples/configs/` plus a functional integration test.

### Design

#### Filter chain

Two new filters, following the exact structural precedent of
`apis/src/anthropic/to_openai/` and `apis/src/anthropic/stream_events/`
(the only other provider translation this codebase already ships and tests):

```yaml
filter_chains:
  - name: bedrock-transform
    filters:
      - filter: bedrock_translate
        api: converse                 # or: invoke_model
        models:
          - anthropic.claude-3-5-sonnet-20241022-v2:0
          - amazon.nova-pro-v1:0
        max_body_bytes: 1048576
        credential:                   # optional; see "Credential handling"
          strategy: bearer_token
          secret_ref:
            name: bedrock-api-key
            namespace: bedrock-gateway
            key: token

      - filter: bedrock_stream_events
        max_partial_frame_bytes: 10485760
        response_conditions:
          - when:
              headers:
                content-type: "application/vnd.amazon.eventstream"

      - filter: credential_inject
        credentials:
          - name: bedrock-api-key
            namespace: bedrock-gateway
            key: token
            strategy: bearer_token
            file: /run/secrets/bedrock-api-key/token

      - filter: router
        routes:
          - path_prefix: "/"
            cluster: "bedrock-runtime"

      - filter: load_balancer
        clusters:
          - name: "bedrock-runtime"
            endpoints:
              - "bedrock-runtime.us-east-1.amazonaws.com:443"
```

New crate module layout, mirroring `apis/src/anthropic/`:

```
apis/src/bedrock/
  mod.rs                 # registers bedrock_translate + bedrock_stream_events
  request.rs             # OpenAI -> Converse / InvokeModel body mapping
  response.rs            # Converse / InvokeModel -> OpenAI body mapping
  error.rs                # Bedrock error -> OpenAI error envelope mapping
  path.rs                 # model allowlist validation + path construction
  stream_events/
    mod.rs               # bedrock_stream_events filter (binary decode)
```

#### Provider identity and the model allowlist (requirement 6)

00762 requires "provider identity ... read exclusively from the trusted route
result ... never inferred or overridden from consumer-controlled inputs." In
a single-provider filter chain like the one above, *provider* identity is
already structural: this filter chain only ever talks to Bedrock because the
operator wired it that way in `filter_chains`/`load_balancer` — a consumer
cannot make this chain call Vertex AI no matter what it sends. `bedrock_translate`
never chooses between providers; it only serves the one its chain was
configured for. This is the same trust boundary the accepted Anthropic
example config already relies on (`router`'s `path_prefix: "/"` sends
everything to one static `cluster`).

What *is* still consumer-influenced, and must be independently policed, is
*which Bedrock model* a request targets, because that model ID lands in the
URL path. `bedrock_translate` validates the request's `model` field against
its own `models:` allowlist (config, above) before it is allowed anywhere
near path construction — the same "exact match against operator-owned
policy" pattern `filters/src/routing/provider_route.rs` already uses for its
`route.model` check, just scoped locally to this filter instead of a second
routing hop. An unrecognized model is rejected with `404` (matching
`provider_route`'s existing convention for "wrong model" — see
`filters/src/routing/provider_route.rs:214`); a missing `model` field is
rejected `400`. Only an allowlisted model string is percent-encoded
(`percent-encoding`, already a workspace dependency — see
`apis/Cargo.toml:30`) into the path segment, so no consumer-controlled byte
sequence reaches the URL unchecked.

#### Reading the request during the header phase

Bedrock needs the model (for the path) and the `stream` flag (for which of
the four path variants to use) before the request leaves the header phase —
i.e. before `router`/`load_balancer` run. `anthropic_to_openai` does not need
this because Anthropic's Messages API keeps the model in the body only.

This is solvable without a second routing hop because of a mechanism this
codebase already exercises: `HttpFilterContext::buffered_request_body` is
populated by the protocol layer's **pre-read** whenever any filter in the
chain declares `BodyMode::StreamBuffer`, and — per the framework's own
documentation — *this pre-read completes before `on_request` runs for the
whole chain*:

`praxis-proxy-filter` crate, `src/context.rs`:

```rust
    /// This remains available during `on_request` even when a filter's body
    /// hook was skipped because body-derived header mutations changed its
    /// request conditions between the pre-read and header phases.
    pub buffered_request_body: Option<bytes::Bytes>,
```

and, stated even more explicitly by an existing filter that depends on the
same ordering (`praxis-proxy-filter` crate, `src/builtins/http/security/policy/filter.rs`):

```rust
    async fn identity_gate(&self, ctx: &HttpFilterContext<'_>) -> Result<FilterAction, FilterError> {
        // When a downstream body-buffering filter (e.g. the protocol
        // classifier) forces a pre-read, praxis runs `on_request_body` BEFORE
        // this header phase. ...
```

So `bedrock_translate` declares `request_body_mode() = BodyMode::StreamBuffer
{ max_bytes: Some(config.max_body_bytes) }` (identical to
`anthropic_to_openai`'s existing request mode), which forces the pre-read.
Its own `on_request` then reads `ctx.buffered_request_body` directly to pull
`model` and `stream`, run the allowlist check above, build the four possible
paths (`/model/{model}/converse`, `/converse-stream`, `/invoke`,
`/invoke-with-response-stream`), and set `ctx.rewritten_path` — all before
`router` runs later in the same header phase. This is not a new mechanism;
it is the exact mechanism `model_to_header` + `filters/src/inference/model_to_header.rs`
already rely on to get a body-derived value in front of routing, applied
directly instead of via an intermediate header hop (which Bedrock doesn't
need, since nothing downstream needs the model as a header — only this
filter needs it, to build the path).

`on_request` also strips all consumer-supplied SigV4 material at this point
(requirement 7) — headers `Authorization`, `X-Amz-Date`, `X-Amz-Security-Token`,
`X-Amz-Content-Sha256`, and any other header prefixed `x-amz-`; and query
parameters `X-Amz-Algorithm`, `X-Amz-Credential`, `X-Amz-Date`, `X-Amz-Expires`,
`X-Amz-SignedHeaders`, `X-Amz-Signature`, `X-Amz-Security-Token`. This mirrors
`anthropic_to_openai`'s existing self-contained stripping of `x-api-key`/
`anthropic-version`, and `provider_route::strip_edge_headers`'s pattern of
removing `Authorization` unconditionally before any credential-injection
filter runs — done inside the translation filter itself rather than left to
an operator to remember to configure, because it is a security requirement,
not an optional cleanliness step.

#### Request body translation (requirements 1–2)

`on_request_body` runs after the pre-read with the full body and
`end_of_stream = true` (the same one-shot pattern `anthropic_to_openai`
already uses for its `StreamBuffer` body). It dispatches on the configured
`api`:

- **Converse** (`request::to_converse_body`): OpenAI `messages[]` become
  Converse `system` (system-role messages hoisted out, matching the same
  hoisting `anthropic_to_openai` already does for Anthropic's `system`
  field) plus `messages[]` with `content: [{text: ...}]` blocks; OpenAI
  `tools[]` map to Converse `toolConfig.tools[].toolSpec`; `temperature`/
  `top_p`/`max_tokens`/`stop` map to `inferenceConfig`. Malformed or
  untranslatable input (no `messages`, unsupported field shape) is rejected
  `400` before ever reaching Bedrock — the same fail-closed rule 00762
  requires for every provider.
- **InvokeModel** (`request::to_invoke_model_body`): InvokeModel's body
  schema is defined per model *family*, not by Bedrock — 00762 explicitly
  scopes this proposal to pinned release fixtures, not full provider
  coverage. This release targets the Anthropic model family on Bedrock,
  whose InvokeModel body (`anthropic_version`, `max_tokens`, `messages`,
  `system`) is — by AWS's own design — nearly identical to Anthropic's
  native Messages API body. Rather than re-deriving the same content-block
  and tool-use mapping a second time, `to_invoke_model_body` calls the
  existing, already-fixture-tested mapping functions in
  `apis/src/anthropic/to_openai/request.rs` and layers on the two
  Bedrock-specific differences: inject `anthropic_version` and drop the
  top-level `model`/`stream` fields (which are already consumed into the
  path by this point). Additional InvokeModel model families are additive
  follow-up work, not a blocker for this proposal.

#### Response and error translation (requirements 3–4)

`on_response` buffers non-streaming bodies (`BodyMode::StreamBuffer`, same
as `anthropic_to_openai`'s response handling) and dispatches by status and
`api`:

- **2xx, Converse**: `response::from_converse_body` maps
  `output.message.content[]` (text and `toolUse` blocks) to OpenAI
  `choices[0].message.content` / `tool_calls[]`; `stopReason` (`end_turn`,
  `tool_use`, `max_tokens`, `stop_sequence`, `content_filtered`) to OpenAI
  `finish_reason` (`stop`, `tool_calls`, `length`, `stop`,
  `content_filter`); `usage.{input,output,total}Tokens` to OpenAI
  `usage.{prompt,completion,total}_tokens`.
- **2xx, InvokeModel** (Anthropic family): `response::from_invoke_model_body`
  again delegates to the existing `apis/src/anthropic/to_openai/response.rs`
  mapping, since the InvokeModel response body is Anthropic's native
  Messages response shape.
- **non-2xx**: `error::to_openai_error` maps Bedrock's `{"message": "..."}`
  body plus its `x-amzn-errortype` header into the OpenAI error envelope
  (`{"error": {"message", "type", "code"}}`), the same shape
  `apis/src/anthropic/wire.rs`'s `error_body` helper already produces on the
  Anthropic side, applied here to Bedrock's error fields instead.

#### Streaming translation (requirement 5)

`bedrock_stream_events` self-arms exactly like `anthropic_stream_events`
does today: via metadata set by `bedrock_translate` (`bedrock_translate.streaming
== "true"`) plus a `Content-Type: application/vnd.amazon.eventstream` check,
with `response_conditions` in the example config as a cheap, optional
pre-filter (the same belt-and-suspenders pattern the Anthropic example
config already uses — see `examples/configs/anthropic/messages-to-openai.yaml:34-37`).
It declares `response_body_mode() = BodyMode::Stream` — never buffering the
full stream, per 00762's "all decoders must operate incrementally" goal.

For the binary framing itself, this proposal adds
[`aws-smithy-eventstream`](https://docs.rs/aws-smithy-eventstream) (v0.61.2)
as a new dependency instead of hand-rolling length-prefix/CRC32 parsing. It
is the same low-level crate the official `aws-sdk-bedrockruntime` Rust SDK
uses internally for this exact wire format; using it directly (its `frame`
module — `MessageFrameDecoder`, `DecodedFrame`, `Message` — is public and
usable standalone) gets byte-for-byte-correct, already-fuzz-tested framing
without pulling in a full generated SDK client. `MessageFrameDecoder::decode_frame`
is exactly the incremental shape this filter needs: feed it bytes, and it
reports either `DecodedFrame::Incomplete` (buffer more) or
`DecodedFrame::Complete(Message)` (one fully-framed, CRC-checked event) —
so the filter's job per chunk is: append to a scratch buffer, loop
`decode_frame` while it returns `Complete`, translate each `Message` to an
OpenAI SSE chunk, and stop looping (not erroring) on `Incomplete`.

The partial-frame scratch buffer is bounded (`max_partial_frame_bytes`,
config above) and held as **typed per-request filter state** — a
`BedrockStreamState { scratch: Vec<u8>, decoder_bytes_seen: usize, ... }` —
rather than a filter-metadata string. This is not a new idiom: it is the
same mechanism the newer `apis/src/openai/responses/stream_events/` filter
already uses for its own per-chunk parser state, via
`HttpFilterContext::insert_filter_state`/`get_filter_state_mut` (keyed by the
currently-executing filter's own id, so no string-key collisions across
filters or filter instances are possible):

`apis/src/openai/responses/stream_events/mod.rs`:

```rust
/// Per-request parser and accumulation state.
pub(super) struct StreamEventsState {
    /// Byte-level SSE frame parser.
    frame_parser: SseFrameParser,
    ...
}
...
ctx.insert_filter_state(StreamEventsState {
    frame_parser: SseFrameParser::new(self.parser_config.max_buffer_bytes),
    ...
});
...
fn is_armed(ctx: &HttpFilterContext<'_>) -> bool {
    ctx.get_filter_state::<StreamEventsState>().is_some()
}
```

`bedrock_stream_events` follows this same, more current convention (rather
than the older filter-metadata hex-encoding `anthropic_stream_events` used
before this mechanism existed): `is_armed` becomes "has `BedrockStreamState`
been inserted", and the scratch buffer is a plain `Vec<u8>` field on that
state — no string encode/decode round-trip per chunk. On overflow, it
discards the buffer and resyncs at the next frame boundary rather than
corrupting state silently — the same recovery philosophy just adopted for
the token-accounting overflow fix (praxis-proxy/ai#674), applied here to
transport framing instead of JSON/SSE scanning.

Each decoded `Message` is translated per API:

- **`converse-stream`**: event-type header selects `messageStart`,
  `contentBlockStart`/`Delta`/`Stop` (text or `toolUse` input-delta),
  `messageStop` (carries `stopReason`), and `metadata` (carries `usage`) —
  each maps to one OpenAI `chat.completion.chunk` SSE frame, plus a final
  `data: [DONE]\n\n`. In-stream exception messages (distinguished by an
  `:exception-type` header instead of `:event-type` — Bedrock's
  binary-framing equivalent of an SSE `error` event) are mapped to a
  terminal OpenAI-shaped error chunk and the stream is closed rather than
  silently dropped, satisfying 00762's "in-stream exception events ... must
  be mapped to the consumer contract."
- **`invoke-with-response-stream`** (Anthropic family): each frame's
  `chunk.bytes` field is a base64-encoded JSON payload in Anthropic's own
  streaming event shape (`content_block_delta`, etc.) — so after
  base64-decoding, this reuses the existing per-event translation functions
  in `apis/src/anthropic/stream_events/mod.rs` instead of a third
  reimplementation of the same delta-accumulation logic.

On a frame that fails CRC/length validation (transport corruption, not a
schema mismatch), the filter cannot reject the request — headers are already
sent — so it emits one terminal OpenAI-shaped error chunk, logs at `warn`,
and stops translating further bytes for that response, rather than passing
corrupted bytes downstream or panicking.

#### Credential handling

Translation itself stays a pure protocol concern, per 00762 — no signing
logic lives in `bedrock_translate`. Upstream authentication to Bedrock is
delegated entirely to the already-shipped `credential_inject` filter using
its `bearer_token` strategy, now that
[AWS Bedrock supports API keys via Bearer tokens](https://docs.aws.amazon.com/bedrock/latest/userguide/api-keys.html) —
no SigV4 signing needs to be implemented in Praxis at all for the upstream
hop, which removes what would otherwise be the largest and riskiest part of
this proposal.

`credential_inject` only activates when it finds its documented metadata
contract (`intelligent_route.credential.{strategy,name,namespace,key}`) in
`ctx.filter_metadata` — normally written by `intelligent_route` or
`provider_route`. Neither is appropriate here: they exist for multi-hop
gateway topologies with edge-selected candidates and (for `provider_route`)
downstream mTLS peer trust, which this single-provider chain does not have
and should not fake just to satisfy an unrelated filter's activation
condition. Instead, when the optional `credential:` block is present in its
own config (example above), `bedrock_translate` writes that same, already
zero-secret-bytes metadata contract itself — exactly the role
`provider_route` plays for its own scope (see
`filters/src/routing/provider_route.rs:219-227`,
`set_credential_metadata`), just scoped locally to the one provider this
chain serves, with no mTLS precondition. `credential_inject` runs unmodified
after it in the chain and does not need to know or care which filter wrote
the metadata.

**Known limitation, flagged as a graduation criterion, not hidden:**
`credential_inject`'s `file` source is read once at filter construction.
Bedrock's short-term API keys expire in 12 hours; keeping one live in
production requires either a long-term key (AWS's own docs mark these
"not recommended for production") or an operator-side rotation process that
also triggers a Praxis config/secret reload. This proposal does not solve
credential rotation — it reuses the existing injection seam as-is and
surfaces the rotation gap explicitly for stakeholder review, rather than
silently assuming it away.

#### Mutation ordering (requirement 8)

Within `bedrock_translate::on_request`: (1) parse the pre-read body, (2)
validate `model` against the allowlist — reject before any mutation if
invalid, (3) strip SigV4 headers/query params, (4) build and set
`rewritten_path`, (5) set credential metadata if configured, (6) set
`bedrock_translate.{model,streaming}` metadata for `bedrock_stream_events`.
This fixed order is documented in the filter's module-level rustdoc, the
same way `provider_route`'s docstring documents its own ordering guarantees.

#### Authorization ordering (requirement 9)

`bedrock_translate` is placed after any caller/model authorization filters
in the example config's `filters:` list, and — like `anthropic_to_openai` —
performs no authorization decisions itself; it only runs once the pipeline
has already reached it, which the framework guarantees by filter order
within `execute_http_request`.

#### Fixtures and testing (requirements 10–12)

- **Unit + fixture tests** in `apis/src/bedrock/{request,response,error}.rs`
  and `stream_events/mod.rs`, following `apis/src/anthropic/to_openai/`'s
  existing structure: golden request/response/error JSON fixtures with
  provenance headers (Bedrock API version, capture/spec source), plus
  negative fixtures (missing `messages`, oversized/truncated event-stream
  frames, malformed CRC, in-stream exception events) — each fixture scanned
  for secret material before merge, per 00762.
- **Streaming fixtures**: raw event-stream binary fixtures, generated with
  `aws-smithy-eventstream`'s own `write_message_to` in a test-only helper —
  reusing the same crate symmetrically for both directions in the test
  harness rather than hand-encoding binary frames by hand.
- **Integration test**: `examples/configs/bedrock/converse.yaml` (and a
  `converse-streaming.yaml` variant) plus a functional integration test
  using `praxis_test_utils::Backend::fixed` returning a canned Converse
  response, following the exact pattern of
  `tests/integration/tests/suite/examples/anthropic_messages.rs`.
- **End-to-end "capital of France" test**: an OpenAI-shaped
  `POST /v1/chat/completions` with `{"model": "...", "messages": [{"role":
  "user", "content": "What is the capital of France?"}]}` through the proxy,
  asserting `choices[0].message.content` contains "Paris" — run against a
  strict Bedrock protocol simulator (a fixed backend serving the pinned
  Converse fixture) for CI, with the same request runnable against a real
  Bedrock endpoint when credentials are available.

### Why this satisfies praxis-proxy/ai#778

| Acceptance criterion | Satisfied by |
|---|---|
| OpenAI → Converse request transform | `request::to_converse_body`, fixture-backed |
| OpenAI → InvokeModel request transform | `request::to_invoke_model_body`, reusing `apis/src/anthropic/to_openai/request.rs` for the Anthropic family, fixture-backed |
| Bedrock → OpenAI response transform (both APIs) | `response::from_converse_body` / `from_invoke_model_body`, fixture-backed |
| Error-response transform | `error::to_openai_error`, fixture-backed |
| Streaming transform, arbitrary chunk boundaries | `bedrock_stream_events` + `aws-smithy-eventstream::MessageFrameDecoder`, incremental, bounded buffer |
| Provider identity from trusted route result only | Provider fixed by filter-chain/cluster config, not consumer input; model independently allowlisted before path construction |
| Consumer SigV4 stripped, headers + query | `bedrock_translate::on_request`, enumerated header/query strip list |
| Deterministic mutation ordering | Fixed, documented 6-step order in `on_request` |
| Translation after authorization | Filter chain position; framework-enforced execution order |
| Fixture provenance + secret scan | Provenance headers per fixture; pre-merge secret scan, per 00762 |
| End-to-end "capital of France" | Integration test against `Backend::fixed` simulator + real-endpoint-capable |
| Example config + functional integration test | `examples/configs/bedrock/*.yaml` + `tests/integration/tests/suite/examples/bedrock.rs` |

## Related

- [praxis-proxy/ai#778](https://github.com/praxis-proxy/ai/issues/778) —
  tracking issue this proposal implements.
- [docs/proposals/00762_api-translation.md](00762_api-translation.md) —
  parent proposal; accepted What?/Why? for provider translation in general.
- [praxis-proxy/ai#674](https://github.com/praxis-proxy/ai/issues/674) —
  overflow-recovery philosophy reused for the partial-frame buffer.
- [`aws-smithy-eventstream`](https://docs.rs/aws-smithy-eventstream) —
  binary event-stream framing crate reused for decoding.
