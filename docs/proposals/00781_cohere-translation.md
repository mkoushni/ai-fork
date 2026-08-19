---
issue: https://github.com/praxis-proxy/ai/issues/781
discussion: >-
  Sub-task of praxis-proxy/ai#762 (itself a sub-task of praxis-proxy/ai#114,
  under Epic praxis-proxy/ai#363). The What?/Why? for provider translation in
  general — including Cohere as one of the four in-scope providers — was
  already discussed and accepted in
  docs/proposals/00762_api-translation.md. This proposal narrows that
  accepted scope to Cohere specifically and adds the How?, per
  praxis-proxy/ai#762 being split into one per-provider sub-task
  (praxis-proxy/ai#778-#781).
status: proposed
authors:
  - mkoushni
graduation_criteria:
  - "Cohere v2 Chat request/response/error/stream fixture manifest reviewed by stakeholders"
stakeholders:
  - # TODO: add relevant maintainer/domain-expert handles before merge
---

# API Translation: Cohere

## What?

Add a Cohere-specific translation stage to the Praxis filter pipeline that
bidirectionally rewrites OpenAI Chat Completions-shaped traffic to and from
Cohere's v2 Chat API (`/v2/chat`), including its SSE streaming variant. This
is the Cohere-scoped instance of the general translation stage already
accepted in [docs/proposals/00762_api-translation.md](00762_api-translation.md);
this proposal adds the **How?** for
[praxis-proxy/ai#781](https://github.com/praxis-proxy/ai/issues/781) only.

### Goals

- Translate OpenAI Chat Completions requests to Cohere v2 Chat request
  bodies, and translate Cohere responses (including errors) back to OpenAI
  Chat Completions shape.
- Decode Cohere's SSE event stream incrementally, across arbitrary chunk
  boundaries, reusing the codebase's existing byte-level SSE reassembly
  utility rather than a new one.
- Recognize, and take advantage of, the fact that Cohere is structurally the
  *simplest* of the four providers in scope: a single fixed endpoint, and a
  request/response schema Cohere itself designed to track OpenAI's
  tool-calling conventions closely — so this design should be sized to
  match, not padded out to look like Bedrock's or Vertex's.
- Strip consumer-supplied `Authorization` bearer credentials unconditionally.

## Why?

### Motivation

Cohere is the structural baseline of the four providers in
praxis-proxy/ai#762's scope — worth stating plainly, because getting the
*simplest* provider's design artificially complicated would say something
is wrong with the design, not that Cohere is hard:

1. **No model-in-path, no deployment map, no project/location.** Cohere's
   Chat endpoint is a single fixed path, `/v2/chat`; `model` stays a JSON
   body field on the outbound Cohere request too, exactly like on the
   inbound OpenAI request. None of the path-injection concerns that drive
   `bedrock_translate`'s and `vertex_translate`'s model-allowlist-before-path-
   construction logic (praxis-proxy/ai#778, praxis-proxy/ai#779) apply here,
   because Cohere gives the model nowhere infrastructure-sensitive to land.
2. **The schemas are close, but not identical — genuine field-level mapping
   is still required.** Unlike Azure (praxis-proxy/ai#780), Cohere is not
   wire-compatible with OpenAI: field names differ (`p`/`k` instead of
   `top_p`/`top_k`, `stop_sequences` instead of `stop`), and the response
   `usage` shape is nested differently. But Cohere v2's `tool_calls` shape
   on assistant messages, and its streaming event lifecycle
   (`message-start`/`content-start`/`content-delta`/`content-end`/
   `message-end`), are deliberately close in *structure* to conventions this
   codebase already has real code for (OpenAI tool-calling shape;
   Anthropic's start/delta/stop streaming lifecycle) — real, useful
   precedent to design against, short of literal shared functions.

### User Stories

(Inherits the general user stories from 00762 — consumer stability, operator
config-only provider swaps, transparent streaming, provider-identity
integrity, fail-closed translation. No Cohere-specific user story beyond
those; Cohere does not introduce a new operator concern the way Bedrock's
credential rotation or Azure's deployment-name mapping do.)

## How?

### Requirements

Restating praxis-proxy/ai#781's acceptance criteria as requirements this
design must satisfy:

1. Request transform OpenAI → Cohere Chat, fixture-backed.
2. Response transform Cohere Chat → OpenAI, fixture-backed.
3. Error-response transform, fixture-backed.
4. Streaming transform across arbitrary chunk boundaries using the SSE
   decoder; event framing and ordering preserved.
5. Provider identity derived exclusively from the trusted route result;
   unrecognized/missing provider rejected before translation.
6. Consumer-supplied credentials stripped (`Authorization` bearer), verified
   by a fixture.
7. Deterministic, documented header and body mutation ordering.
8. Translation runs only after the caller/model authorization stage.
9. Fixtures carry provenance and are scanned for real credential material.
10. End-to-end "capital of France" test against a real Cohere endpoint or a
    strict protocol simulator.
11. Example config in `examples/configs/` plus a functional integration test.

### Design

#### Filter chain

```yaml
filter_chains:
  - name: cohere-transform
    filters:
      - filter: cohere_translate
        max_body_bytes: 1048576
        models: []                    # optional allowlist; empty = accept any
        credential:                   # optional; see "Credential handling"
          strategy: bearer_token
          secret_ref:
            name: cohere-api-key
            namespace: cohere-gateway
            key: token

      - filter: cohere_stream_events
        max_buffer_bytes: 10485760
        response_conditions:
          - when:
              headers:
                content-type: "text/event-stream"

      - filter: credential_inject
        credentials:
          - name: cohere-api-key
            namespace: cohere-gateway
            key: token
            strategy: bearer_token
            file: /run/secrets/cohere-api-key/token

      - filter: path_rewrite
        replace:
          pattern: "^/v1/chat/completions$"
          replacement: "/v2/chat"
        conditions:
          - when:
              path_prefix: "/v1/chat/completions"

      - filter: router
        routes:
          - path_prefix: "/"
            cluster: "cohere-runtime"

      - filter: load_balancer
        clusters:
          - name: "cohere-runtime"
            endpoints:
              - "api.cohere.com:443"
```

This chain is shorter than Bedrock's/Vertex's/Azure's, and deliberately so:
the path rewrite is a static regex substitution handled entirely by the
existing `path_rewrite` builtin filter (already used exactly this way in
the accepted `examples/configs/anthropic/messages-to-openai.yaml`) — Cohere
needs no filter-authored path construction logic at all, unlike the other
three proposals, because nothing about the Cohere path is dynamic.

New crate module layout:

```
apis/src/cohere/
  mod.rs                 # registers cohere_translate + cohere_stream_events
  request.rs             # OpenAI -> Cohere v2 Chat body mapping
  response.rs            # Cohere v2 Chat -> OpenAI body mapping
  error.rs                # Cohere error -> OpenAI error envelope mapping
  stream_events/
    mod.rs               # cohere_stream_events filter (SSE decode)
```

#### Provider identity (requirement 5)

Same structural argument as the other three proposals: provider identity is
fixed by the filter chain and `load_balancer` cluster the operator
configured, not by anything in the request. Cohere has no
routing-sensitive request field at all — `model` never leaves the JSON
body — so there is no path-injection surface to police the way
`bedrock_translate`/`vertex_translate`/`azure_translate` must. The optional
`models:` allowlist exists purely for operator policy (reject an
unsupported model with a clear `404` before forwarding, rather than
surfacing Cohere's own error), not for injection safety; an empty list
accepts any `model` value, and is the honest default given there is nothing
unsafe about passing an unvalidated model string to `/v2/chat`'s fixed
path.

#### No pre-read needed — request body translation runs at the normal time (requirements 1, 7)

This is the one meaningful structural difference from the other three
proposals. `bedrock_translate`/`vertex_translate`/`azure_translate` all
need `model` (or `stream`) **before** the header phase completes, because
routing depends on it (the URL path itself encodes the model/deployment).
Cohere's routing does not depend on the request body at all — the
destination is the one endpoint the operator's `load_balancer` cluster
names — so `cohere_translate` has no need to force an early pre-read via
`ctx.buffered_request_body` the way the other three do.

`cohere_translate::on_request` is therefore small: strip the consumer's
`Authorization` header unconditionally, and set credential metadata if
configured (see "Credential handling"). Its `request_body_mode()` is still
`BodyMode::StreamBuffer { max_bytes: Some(config.max_body_bytes) }` — the
translation still needs the complete JSON body to remap fields correctly —
but it only needs it during the normal `on_request_body` phase, not pulled
forward into `on_request`. `request::to_cohere_chat_body` then performs the
mapping:

- `messages[]`: Cohere v2 uses the same `role` values (`system`, `user`,
  `assistant`, `tool`) and, since a recent Cohere v2 redesign, the same
  `tool_calls: [{id, type: "function", function: {name, arguments}}]`
  shape on assistant messages and `tool_call_id` on tool-result messages
  that OpenAI itself uses — the field-by-field mapping here is closer to an
  identity copy with light reshaping than to `bedrock_translate`'s or
  `vertex_translate`'s full content-block reconstruction.
- `temperature` copies directly; `top_p` → Cohere's `p`; `top_k` → Cohere's
  `k`; `stop` → Cohere's `stop_sequences`; `max_tokens` copies directly.
- `tools[]` (OpenAI JSON-schema function defs) map to Cohere's own
  `tools[].function` shape, which — again, by Cohere's own v2 design intent
  — is close enough to OpenAI's that this is a light reshape, not a new
  representation.

Malformed or untranslatable input (no `messages`, unsupported field shape)
is rejected `400` before ever reaching Cohere — the same fail-closed rule
00762 requires for every provider.

#### Response and error translation (requirements 2–3)

`on_response` buffers non-streaming bodies (`BodyMode::StreamBuffer`) and
dispatches by status:

- **2xx**: `response::from_cohere_chat_body` maps `message.content[]`
  (Cohere v2's response content is an array of typed blocks, primarily
  `{"type": "text", "text": ...}`) to OpenAI `choices[0].message.content`;
  `message.tool_calls[]` copies through with only the same light reshape
  used on the request side, since the shapes already match; Cohere's
  `finish_reason` (`COMPLETE`, `MAX_TOKENS`, `TOOL_CALL`, `ERROR`) maps to
  OpenAI's `finish_reason` (`stop`, `length`, `tool_calls`, `stop`); and
  Cohere's nested `usage.tokens.{input_tokens,output_tokens}` maps to
  OpenAI's flat `usage.{prompt,completion,total}_tokens` (with `total`
  computed as their sum, since Cohere does not return one directly).
- **non-2xx**: `error::to_openai_error` maps Cohere's flat error body
  (`{"message": "..."}`, the same shape Bedrock returns — see
  docs/proposals/00778_bedrock-translation.md's `error.rs`) into OpenAI's
  `{"error": {"message", "type", "code"}}` envelope, using the same mapping
  approach as `bedrock_translate`'s error module, adapted to Cohere's
  status-to-type conventions rather than Bedrock's `x-amzn-errortype`
  header.

#### Streaming translation (requirement 4)

`cohere_stream_events` self-arms via metadata set by `cohere_translate`
(`cohere_translate.streaming == "true"`) plus a
`Content-Type: text/event-stream` check, declares
`response_body_mode() = BodyMode::Stream`, and — because Cohere's transport
genuinely is SSE — reuses the exact same byte-level reassembly utility and
ownership pattern as `vertex_stream_events` (praxis-proxy/ai#779,
docs/proposals/00779_vertex-translation.md): `apis/src/openai/sse::SseFrameParser`,
owned per-request via `HttpFilterContext::insert_filter_state`/
`get_filter_state_mut` in a `CohereStreamState { frame_parser: SseFrameParser,
... }`, following the same precedent established by
`apis/src/openai/responses/stream_events/`.

What is Cohere-specific is only what happens *after* frame reassembly. Each
Cohere v2 stream event carries a `type` field in its JSON payload
(`message-start`, `content-start`, `content-delta`, `content-end`,
`message-end`, and the tool-call equivalents) describing a start/delta/end
lifecycle for the growing response — structurally the same *shape* of
lifecycle `apis/src/anthropic/stream_events/mod.rs` already accumulates
for Anthropic's `content_block_start`/`_delta`/`_stop`/`message_start`/
`message_stop` events, even though the two providers' field names and enum
values are unrelated and must be mapped independently. `cohere_stream_events`
translates each lifecycle event into one OpenAI `chat.completion.chunk` SSE
frame using the same field mapping as the non-streaming response path
(`response::from_cohere_chat_body`, applied incrementally), followed by a
final `data: [DONE]\n\n`. A structurally malformed event (unparseable JSON,
unrecognized `type`) does not abort the response — it emits one terminal
OpenAI-shaped error chunk and stops, matching the fail-safe-degrade pattern
used by every other stream filter in this set of four proposals.

#### Credential handling (requirement 6)

Cohere's only credential surface is a Bearer token in `Authorization` —
the simplest of the four providers here, and the existing
`credential_inject` filter's `bearer_token` strategy supports it **exactly
as it exists today, with no changes needed** (unlike Azure,
praxis-proxy/ai#780, which needs the small `header`/`value_prefix`
generalization proposed there). `cohere_translate` writes the same
`intelligent_route.credential.{strategy,name,namespace,key}` metadata
contract when its own optional `credential:` config block is present — the
same reasoning as the other three proposals for why `credential_inject`'s
own activation condition (normally satisfied by `intelligent_route`/
`provider_route`) isn't the right fit for a single-provider chain.
`on_request` also strips the consumer's `Authorization` header
unconditionally, regardless of whether `credential:` is configured, so a
consumer cannot pass a credential through to Cohere even in a
misconfigured or credential-less chain.

#### Mutation ordering (requirement 7)

Within `cohere_translate::on_request`: (1) strip the consumer
`Authorization` header, (2) set credential metadata if configured, (3) set
`cohere_translate.streaming` metadata for `cohere_stream_events`. (No path
construction step — see above.) Within `on_request_body`: (4) validate
`model` against the optional allowlist, (5) apply the field mapping in
`request::to_cohere_chat_body`. Documented in the filter's module-level
rustdoc.

#### Authorization ordering (requirement 8)

`cohere_translate` is placed after any caller/model authorization filters
in the example config's `filters:` list and performs no authorization
decisions itself — identical positioning rule to the other three
proposals.

#### Fixtures and testing (requirements 9–11)

- **Unit + fixture tests** in `apis/src/cohere/{request,response,error}.rs`
  and `stream_events/mod.rs`: golden request/response/error JSON fixtures
  with provenance (Cohere v2 API version, capture/spec source), plus
  negative fixtures (missing `messages`, malformed SSE event, unrecognized
  event `type`, mid-stream Cohere error) — each scanned for secret material
  before merge.
- **Integration test**: `examples/configs/cohere/chat.yaml` (and a
  streaming variant) plus a functional integration test using
  `praxis_test_utils::Backend::fixed`, following the pattern of
  `tests/integration/tests/suite/examples/anthropic_messages.rs`.
- **End-to-end "capital of France" test**: an OpenAI-shaped
  `POST /v1/chat/completions` with `{"model": "command-r-plus", "messages":
  [{"role": "user", "content": "What is the capital of France?"}]}` through
  the proxy, asserting `choices[0].message.content` contains "Paris" — run
  against a strict Cohere protocol simulator for CI, with the same request
  runnable against a real Cohere endpoint when credentials are available.

### Why this satisfies praxis-proxy/ai#781

| Acceptance criterion | Satisfied by |
|---|---|
| OpenAI → Cohere Chat request transform | `request::to_cohere_chat_body`, fixture-backed |
| Cohere Chat → OpenAI response transform | `response::from_cohere_chat_body`, fixture-backed |
| Error-response transform | `error::to_openai_error`, fixture-backed |
| Streaming transform, arbitrary chunk boundaries | `cohere_stream_events` + reused `SseFrameParser`/`insert_filter_state`, incremental |
| Provider identity from trusted route result only | Provider fixed by filter-chain/cluster config, not consumer input; optional model allowlist for policy, not injection safety |
| Consumer `Authorization` bearer stripped | `cohere_translate::on_request`, unconditional |
| Deterministic mutation ordering | Fixed, documented 5-step order across `on_request`/`on_request_body` |
| Translation after authorization | Filter chain position; framework-enforced execution order |
| Fixture provenance + secret scan | Provenance headers per fixture; pre-merge secret scan, per 00762 |
| End-to-end "capital of France" | Integration test against `Backend::fixed` simulator + real-endpoint-capable |
| Example config + functional integration test | `examples/configs/cohere/*.yaml` + `tests/integration/tests/suite/examples/cohere.rs` |

## Related

- [praxis-proxy/ai#781](https://github.com/praxis-proxy/ai/issues/781) —
  tracking issue this proposal implements.
- [docs/proposals/00762_api-translation.md](00762_api-translation.md) —
  parent proposal; accepted What?/Why? for provider translation in general.
- [docs/proposals/00778_bedrock-translation.md](00778_bedrock-translation.md),
  [docs/proposals/00779_vertex-translation.md](00779_vertex-translation.md),
  [docs/proposals/00780_azure-translation.md](00780_azure-translation.md) —
  sibling proposals; this one is the structural baseline of the four.
