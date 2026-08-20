---
issue: https://github.com/praxis-proxy/ai/issues/779
discussion: >-
  Sub-task of praxis-proxy/ai#762 (itself a sub-task of praxis-proxy/ai#114,
  under Epic praxis-proxy/ai#363). The What?/Why? for provider translation in
  general — including Google Vertex AI as one of the four in-scope providers,
  and the specific decision to force `?alt=sse` so Vertex shares the SSE
  decoder path with Azure/Cohere — was already discussed and accepted in
  docs/proposals/00762_api-translation.md. This proposal narrows that
  accepted scope to Vertex AI specifically and adds the How?, per
  praxis-proxy/ai#762 being split into one per-provider sub-task
  (praxis-proxy/ai#778-#781).
status: proposed
authors:
  - mkoushni
graduation_criteria:
  - "Gemini request/response/error/stream fixture manifest reviewed by stakeholders"
  - "OAuth2 access-token rotation/hot-reload strategy for credential_inject reviewed by stakeholders"
stakeholders:
  - shaneutt
  - alexsnaps
  - aslakknutsen
  - szedan-rh
---

# API Translation: Google Vertex AI (Gemini)

## What?

Add a Vertex AI-specific translation stage to the Praxis filter pipeline
that bidirectionally rewrites OpenAI Chat Completions-shaped traffic to and
from Vertex AI's Gemini `generateContent` / `streamGenerateContent` APIs,
including forcing genuine SSE framing on the streaming path (`?alt=sse`) so
it can share the SSE decoder family with Azure/Cohere rather than needing a
bespoke transport, per 00762's explicit resolution of this point. This is
the Vertex-scoped instance of the general translation stage already accepted
in [docs/proposals/00762_api-translation.md](00762_api-translation.md); this
proposal adds the **How?** for
[praxis-proxy/ai#779](https://github.com/praxis-proxy/ai/issues/779) only.

### Goals

- Translate OpenAI Chat Completions requests to Gemini `generateContent`
  request bodies, and translate Gemini responses (including errors) back to
  OpenAI Chat Completions shape.
- Always request `?alt=sse` on the streaming path so the wire format is
  genuine SSE, and decode it incrementally, across arbitrary chunk
  boundaries, using the codebase's existing byte-level SSE reassembly
  utility rather than a new one.
- Build the Vertex resource path (`projects/{project}/locations/{location}/
  publishers/google/models/{model}:...`) from operator-owned config plus an
  independently allowlisted model — never from an unchecked consumer path
  or query value.
- Strip the consumer's `Authorization` bearer token unconditionally, and
  guarantee (not just strip) that no consumer-supplied value can reach the
  project/location/model path segments in the first place.
- Reuse existing, tested building blocks (`apis/src/openai/sse::SseFrameParser`,
  `percent-encoding`, `credential_inject`) instead of re-implementing SSE
  reassembly, path encoding, or credential injection that this codebase
  already has.

## Why?

### Motivation

Vertex AI shares Azure/Cohere's SSE transport family (once `?alt=sse` is
forced — see 00762's motivation section, added in response to review
feedback that the default `streamGenerateContent` response is a raw JSON
array with no guaranteed alignment between HTTP chunk boundaries and JSON
object boundaries), but it has two features neither Azure nor Cohere have,
which this proposal has to address:

1. **The model is a URL path segment, not a body field**, same structural
   problem as Bedrock (praxis-proxy/ai#778) — but layered under two
   additional operator-owned path segments, `project` and `location`, that
   are not part of the OpenAI request contract at all and must never be
   consumer-influenced.
2. **Gemini's request/response schema is unrelated to every other schema
   Praxis already translates.** Not OpenAI-shaped (obviously), and — unlike
   Bedrock's Anthropic-family InvokeModel body — not Anthropic-shaped
   either: Gemini uses `contents[]`/`parts[]` with role `"model"` (not
   `"assistant"`), and `functionCall`/`functionResponse` parts instead of
   OpenAI's `tool_calls`/`tool` messages or Anthropic's `tool_use`/
   `tool_result` blocks. This translation genuinely needs its own mapping
   code; the reuse story here is in transport and infrastructure, not
   content-block mapping.

### User Stories

(Inherits the general user stories from 00762 — consumer stability, operator
config-only provider swaps, transparent streaming, provider-identity
integrity, fail-closed translation. Adding the Vertex-specific one:)

- As a **platform operator**, I want Vertex's `project`/`location` resource
  identifiers to live entirely in my gateway configuration, so that no
  consumer request — malicious or merely malformed — can ever cause Praxis
  to call a Vertex project or region I did not configure.

## How?

### Requirements

Restating praxis-proxy/ai#779's acceptance criteria as requirements this
design must satisfy:

1. Request transform OpenAI → Vertex Gemini `generateContent`, fixture-backed.
2. Response transform Vertex Gemini → OpenAI, fixture-backed.
3. Error-response transform, fixture-backed.
4. Streaming transform across arbitrary chunk boundaries using the Vertex
   event-stream decoder; event framing and ordering preserved.
5. Provider identity derived exclusively from the trusted route result;
   unrecognized/missing provider rejected before translation.
6. Consumer-supplied credentials stripped across every credential-bearing
   location (`Authorization` header, URI-path/query tokens), verified by a
   fixture.
7. Deterministic, documented header and body mutation ordering.
8. Translation runs only after the caller/model authorization stage.
9. Fixtures carry provenance and are scanned for real credential material.
10. End-to-end "capital of France" test against a real Vertex endpoint or a
    strict protocol simulator.
11. Example config in `examples/configs/` plus a functional integration test.

### Design

#### Filter chain

Two new filters — `vertex_translate` and `vertex_stream_events` — following
the same structural precedent as the Bedrock design
(praxis-proxy/ai#778/docs/proposals/00778_bedrock-translation.md) and the
existing `apis/src/anthropic/to_openai/` + `apis/src/anthropic/stream_events/`
pair:

```yaml
filter_chains:
  - name: vertex-transform
    filters:
      - filter: vertex_translate
        project: my-gcp-project
        location: us-central1
        models:
          - gemini-1.5-pro-002
          - gemini-1.5-flash-002
        max_body_bytes: 1048576
        credential:                   # optional; see "Credential handling"
          strategy: bearer_token
          secret_ref:
            name: vertex-access-token
            namespace: vertex-gateway
            key: token

      - filter: vertex_stream_events
        max_buffer_bytes: 10485760
        response_conditions:
          - when:
              headers:
                content-type: "text/event-stream"

      - filter: credential_inject
        credentials:
          - name: vertex-access-token
            namespace: vertex-gateway
            key: token
            strategy: bearer_token
            file: /run/secrets/vertex-access-token/token

      - filter: router
        routes:
          - path_prefix: "/"
            cluster: "vertex-runtime"

      - filter: load_balancer
        clusters:
          - name: "vertex-runtime"
            endpoints:
              - "us-central1-aiplatform.googleapis.com:443"
```

Note the `load_balancer` endpoint's regional host
(`us-central1-aiplatform.googleapis.com`) and `vertex_translate`'s
`location: us-central1` are two independent, operator-set values that must
agree — validated at filter construction (`from_config` rejects a blank
`project` or `location`, the same bounded-non-blank validation
`filters/src/routing/descriptor.rs::validate_name` already applies to
routing identifiers).

New crate module layout, mirroring `apis/src/bedrock/` (praxis-proxy/ai#778)
and `apis/src/anthropic/`:

```
apis/src/vertex/
  mod.rs                 # registers vertex_translate + vertex_stream_events
  request.rs             # OpenAI -> Gemini generateContent body mapping
  response.rs            # Gemini -> OpenAI body mapping
  error.rs                # Gemini error -> OpenAI error envelope mapping
  path.rs                 # model allowlist validation + path construction
  stream_events/
    mod.rs               # vertex_stream_events filter (SSE decode)
```

#### Provider identity, and why project/location need no "stripping" (requirements 5–6)

Per 00762, provider identity must come exclusively from the trusted route
result, never from consumer input. As with the Bedrock proposal, in a
single-provider filter chain like the one above this is structural: the
chain only ever calls Vertex because the operator's `load_balancer` cluster
points at a Vertex regional endpoint, not because of anything in the
request.

Vertex adds two more identifiers that must be equally protected: `project`
and `location`. The issue's acceptance criteria ask for consumer-supplied
credentials to be "stripped ... across every credential-bearing location
(Authorization header, URI-path/query tokens)". This design goes further
than stripping: `vertex_translate`'s `project` and `location` come
exclusively from its own filter config (operator-owned, set once at
construction) — they are **never read from the incoming request at all**,
so there is no consumer-controlled value in that position to strip in the
first place. This is a strictly stronger guarantee than "detect and remove,"
the same way `provider_route` prefers exact-match allowlisting over
attempting to sanitize untrusted input.

`model`, however, is legitimately consumer-supplied (it is the OpenAI
`model` field, exactly as with Bedrock) and is the only piece of the Vertex
resource path this filter reads from the request. It is validated against
the `models:` allowlist before being percent-encoded into the path — an
unrecognized model is rejected `404`, a missing one `400` — the identical
policy `bedrock_translate` applies, because the risk (an unvalidated string
landing in a URL path) is identical.

As additional defense in depth, `vertex_translate::on_request` also strips
the incoming `Authorization` header unconditionally and drops any incoming
query string entirely before path construction (the outbound query string
is always fully reconstructed by this filter, never merged with an inbound
one), which covers the "URI-path/query tokens" wording in the acceptance
criteria even though — per the above — no consumer-supplied value could
reach those positions through this filter's own logic to begin with.

#### Reading the request during the header phase

Exactly the same mechanism as `bedrock_translate`
(docs/proposals/00778_bedrock-translation.md): `vertex_translate` declares
`request_body_mode() = BodyMode::StreamBuffer { max_bytes: Some(config.max_body_bytes) }`,
which forces the protocol layer's pre-read of the full body before
`on_request` runs for the whole chain (documented directly on
`HttpFilterContext::buffered_request_body` in the `praxis-proxy-filter`
crate's `src/context.rs`). `on_request` reads `model` and `stream` from
`ctx.buffered_request_body`, runs the allowlist check, and builds the
outbound path + query:

- non-streaming: `/v1/projects/{project}/locations/{location}/publishers/google/models/{model}:generateContent`
- streaming: `/v1/projects/{project}/locations/{location}/publishers/google/models/{model}:streamGenerateContent?alt=sse`

and sets `ctx.rewritten_path` to the combined path-and-query string — the
same field (and the same "path plus optional query" convention) the
existing `url_rewrite` builtin filter already uses for its own
`add_query_params`/`strip_query_params` operations, so forcing `?alt=sse`
onto the streaming path needs no new mechanism, just the existing one used
directly. `project`/`location`/`model` are percent-encoded with
`percent-encoding` (already a workspace dependency, `apis/Cargo.toml:30`),
the same crate `bedrock_translate` uses for its own path segment.

#### Request body translation (requirement 1)

`on_request_body` (full body, `end_of_stream = true`, same one-shot
`StreamBuffer` pattern as `anthropic_to_openai`/`bedrock_translate`) calls
`request::to_generate_content_body`:

- OpenAI `messages[]` (role `system`/`user`/`assistant`/`tool`) become
  Gemini `contents[]` with role `user`/`model` (`assistant` → `model`;
  `system` messages are hoisted into the top-level `systemInstruction`
  field, the same hoisting pattern `anthropic_to_openai` already applies
  for Anthropic's `system` field, just targeting a differently-named Gemini
  field); each message's text becomes one `parts: [{text: ...}]` entry.
- OpenAI `tool_calls`/`tool`-role messages become Gemini `functionCall`/
  `functionResponse` parts; OpenAI `tools[]` (JSON-schema function defs)
  become `tools: [{functionDeclarations: [...]}]`.
- `temperature`/`top_p`/`max_tokens`/`stop` map to Gemini's
  `generationConfig {temperature, topP, maxOutputTokens, stopSequences}`.

Malformed or untranslatable input (no `messages`, unsupported field shape)
is rejected `400` before ever reaching Vertex — the same fail-closed rule
00762 requires for every provider, and the same rule `bedrock_translate`
and `anthropic_to_openai` already apply.

#### Response and error translation (requirements 2–3)

`on_response` buffers non-streaming bodies (`BodyMode::StreamBuffer`) and
dispatches by status:

- **2xx**: `response::from_generate_content_body` maps
  `candidates[0].content.parts[]` (`text` / `functionCall`) to OpenAI
  `choices[0].message.content` / `tool_calls[]`; `candidates[0].finishReason`
  (`STOP`, `MAX_TOKENS`, `SAFETY`, `RECITATION`, `OTHER`) to OpenAI
  `finish_reason` (`stop`, `length`, `content_filter`, `content_filter`,
  `stop`); `usageMetadata.{promptTokenCount,candidatesTokenCount,totalTokenCount}`
  to OpenAI `usage.{prompt,completion,total}_tokens`.
- **non-2xx**: `error::to_openai_error` maps Gemini's error envelope
  (`{"error": {"code", "message", "status"}}` — already wrapped in an
  `"error"` object, unlike Bedrock's flat `{"message": ...}`) into OpenAI's
  `{"error": {"message", "type", "code"}}`: `message` copies directly,
  `status` (an enum string like `INVALID_ARGUMENT`) maps to `type`, `code`
  (Google's numeric gRPC-style code) maps to OpenAI's `code` field.

#### Streaming translation (requirement 4)

`vertex_stream_events` self-arms via metadata set by `vertex_translate`
(`vertex_translate.streaming == "true"`) plus a
`Content-Type: text/event-stream` check — genuine SSE, guaranteed by the
`?alt=sse` query parameter `vertex_translate` unconditionally appended to
the outbound request, per 00762's resolution of the Vertex streaming
question. It declares `response_body_mode() = BodyMode::Stream`.

Because the transport actually is SSE (unlike Bedrock's binary event-stream
in praxis-proxy/ai#778), this filter reuses the codebase's own byte-level
SSE reassembly utility instead of writing a second one. `apis/src/openai/sse/mod.rs`
already re-exports it at `pub(crate)` visibility for exactly this kind of
in-crate reuse:

```rust
//! - [`frame::SseFrameParser`] — byte-level SSE chunk reassembly
...
pub(crate) use frame::{SseFrame, SseFrameParser, SseParseError};
```

and the newer `apis/src/openai/responses/stream_events/` filter already
establishes the pattern for owning one `SseFrameParser` instance per
request, via `HttpFilterContext::insert_filter_state`/`get_filter_state_mut`
(typed per-request scratch state keyed by the currently-executing filter,
not a filter-metadata string):

```rust
pub(super) struct StreamEventsState {
    frame_parser: SseFrameParser,
    ...
}
...
ctx.insert_filter_state(StreamEventsState {
    frame_parser: SseFrameParser::new(self.parser_config.max_buffer_bytes),
    ...
});
```

`vertex_stream_events` follows this identical pattern with its own
`VertexStreamState { frame_parser: SseFrameParser, ... }`. Its own,
Vertex-specific work is only what comes *after* frame reassembly: each
complete `SseFrame`'s `data` is Gemini's own streaming delta JSON — one
`candidates[0].content.parts[]` fragment plus, on the terminal frame,
`finishReason` and `usageMetadata` — translated into an OpenAI
`chat.completion.chunk` SSE frame, using the same field mapping as the
non-streaming response path (`response::from_generate_content_body`,
applied incrementally instead of once), followed by a final
`data: [DONE]\n\n`. A structurally malformed frame (not valid JSON, or
missing `candidates`) does not abort the whole response — it emits one
terminal OpenAI-shaped error chunk and stops, the same fail-safe-degrade
behavior `bedrock_stream_events` uses for a corrupted binary frame,
adapted from the same overflow/error-recovery philosophy behind the
token-accounting fix (praxis-proxy/ai#674).

#### Credential handling

Same reuse story as Bedrock: no OAuth2 token-minting logic lives in
`vertex_translate` — translation stays a pure protocol concern per 00762.
Upstream authentication is delegated to the existing `credential_inject`
filter's `bearer_token` strategy. `vertex_translate` writes the same
`intelligent_route.credential.{strategy,name,namespace,key}` metadata
contract `credential_inject` already reads (normally written by
`intelligent_route`/`provider_route`, neither of which is appropriate for a
single-provider chain — see the identical reasoning in
docs/proposals/00778_bedrock-translation.md's "Credential handling"
section) when its own optional `credential:` config block is present.

**Known limitation, flagged as a graduation criterion, not hidden, and
sharper here than for Bedrock:** GCP OAuth2 access tokens minted for a
service account typically expire in **one hour** — shorter than Bedrock's
12-hour API keys. `credential_inject`'s `file` source is read once at
filter construction, so an operator running this in production needs an
external process that refreshes the mounted token file *and* triggers a
Praxis config/secret reload at least that often. This proposal reuses the
existing injection seam as-is and surfaces the rotation cadence explicitly,
rather than assuming a reload mechanism that does not yet exist in scope.

#### Mutation ordering (requirement 7)

Within `vertex_translate::on_request`: (1) parse the pre-read body, (2)
validate `model` against the allowlist — reject before any mutation if
invalid, (3) strip the consumer `Authorization` header, (4) build and set
`rewritten_path` (query string always fully reconstructed, never merged
with any inbound query), (5) set credential metadata if configured, (6) set
`vertex_translate.{model,streaming}` metadata for `vertex_stream_events`.
Documented in the filter's module-level rustdoc, matching the same ordering
discipline `bedrock_translate` documents for itself.

#### Authorization ordering (requirement 8)

`vertex_translate` is placed after any caller/model authorization filters
in the example config's `filters:` list and performs no authorization
decisions itself — identical positioning rule to `bedrock_translate`.

#### Fixtures and testing (requirements 9–11)

- **Unit + fixture tests** in `apis/src/vertex/{request,response,error}.rs`
  and `stream_events/mod.rs`: golden request/response/error JSON fixtures
  with provenance (Gemini API version, capture/spec source), plus negative
  fixtures (missing `contents`, malformed SSE frame, oversized event,
  mid-stream Gemini error) — each scanned for secret material before merge.
- **Integration test**: `examples/configs/vertex/generate-content.yaml`
  (and a `generate-content-streaming.yaml` variant) plus a functional
  integration test using `praxis_test_utils::Backend::fixed`, following the
  pattern of `tests/integration/tests/suite/examples/anthropic_messages.rs`.
- **End-to-end "capital of France" test**: an OpenAI-shaped
  `POST /v1/chat/completions` with `{"model": "gemini-1.5-pro-002",
  "messages": [{"role": "user", "content": "What is the capital of
  France?"}]}` through the proxy, asserting `choices[0].message.content`
  contains "Paris" — run against a strict Vertex protocol simulator (a
  fixed backend serving the pinned Gemini fixture) for CI, with the same
  request runnable against a real Vertex endpoint when credentials are
  available.

### Why this satisfies praxis-proxy/ai#779

| Acceptance criterion | Satisfied by |
|---|---|
| OpenAI → Gemini `generateContent` request transform | `request::to_generate_content_body`, fixture-backed |
| Gemini → OpenAI response transform | `response::from_generate_content_body`, fixture-backed |
| Error-response transform | `error::to_openai_error`, fixture-backed |
| Streaming transform, arbitrary chunk boundaries, "Vertex event-stream decoder" | Forced `?alt=sse` + reused `SseFrameParser`/`insert_filter_state`, per 00762's resolution that Vertex shares the SSE family |
| Provider identity from trusted route result only | Provider fixed by filter-chain/cluster config; `project`/`location` are operator config, never read from the request; `model` independently allowlisted |
| Consumer credentials stripped, Authorization + path/query tokens | `Authorization` stripped unconditionally; `project`/`location`/`model` path never sourced from consumer path/query at all |
| Deterministic mutation ordering | Fixed, documented 6-step order in `on_request` |
| Translation after authorization | Filter chain position; framework-enforced execution order |
| Fixture provenance + secret scan | Provenance headers per fixture; pre-merge secret scan, per 00762 |
| End-to-end "capital of France" | Integration test against `Backend::fixed` simulator + real-endpoint-capable |
| Example config + functional integration test | `examples/configs/vertex/*.yaml` + `tests/integration/tests/suite/examples/vertex.rs` |

## Related

- [praxis-proxy/ai#779](https://github.com/praxis-proxy/ai/issues/779) —
  tracking issue this proposal implements.
- [docs/proposals/00762_api-translation.md](00762_api-translation.md) —
  parent proposal; accepted What?/Why? for provider translation in general,
  including the `?alt=sse` decision for Vertex.
- [docs/proposals/00778_bedrock-translation.md](00778_bedrock-translation.md) —
  sibling proposal; shares the model-allowlist and body-pre-read design.
- [praxis-proxy/ai#674](https://github.com/praxis-proxy/ai/issues/674) —
  overflow-recovery philosophy reused for malformed mid-stream frames.
