---
issue: https://github.com/praxis-proxy/ai/issues/780
discussion: >-
  Sub-task of praxis-proxy/ai#762 (itself a sub-task of praxis-proxy/ai#114,
  under Epic praxis-proxy/ai#363). The What?/Why? for provider translation in
  general — including Azure OpenAI as one of the four in-scope providers —
  was already discussed and accepted in
  docs/proposals/00762_api-translation.md. This proposal narrows that
  accepted scope to Azure OpenAI specifically and adds the How?, per
  praxis-proxy/ai#762 being split into one per-provider sub-task
  (praxis-proxy/ai#778-#781).
status: proposed
authors:
  - mkoushni
graduation_criteria:
  - "credential_inject header-name/prefix generalization reviewed by its maintainers (shared filters/src code, not apis-crate-local)"
  - "Azure request/response/error/stream fixture manifest reviewed by stakeholders"
stakeholders:
  - shaneutt
  - alexsnaps
  - aslakknutsen
  - szedan-rh
---

# API Translation: Azure OpenAI

## What?

Add an Azure OpenAI-specific translation stage to the Praxis filter pipeline
that rewrites OpenAI Chat Completions-shaped traffic to Azure's deployment
endpoint (path + required `api-version` query parameter) and back. This is
the Azure-scoped instance of the general translation stage already accepted
in [docs/proposals/00762_api-translation.md](00762_api-translation.md); this
proposal adds the **How?** for
[praxis-proxy/ai#780](https://github.com/praxis-proxy/ai/issues/780) only.

### Goals

- Rewrite the OpenAI-shaped request path to Azure's deployment-scoped
  endpoint, with `api-version` appended, resolving the consumer's `model`
  field to an operator-owned Azure deployment name through a validated
  mapping rather than an unchecked pass-through.
- Correctly and honestly scope the *body* translation work to what Azure's
  API actually requires — which, because Azure OpenAI is deliberately
  wire-compatible with OpenAI's own Chat Completions contract, is
  substantially smaller than the Bedrock (praxis-proxy/ai#778) or Vertex
  (praxis-proxy/ai#779) body-mapping work. Do not invent transformation work
  that the two APIs' actual compatibility does not require.
- Strip both of Azure's consumer-facing credential surfaces —
  `api-key` and AAD `Authorization` bearer — and close the one real gap in
  reusing the existing credential-injection filter for Azure's `api-key`
  header scheme, rather than hand-rolling a second injection path.
- Still provide genuine, fixture-backed streaming correctness per 00762's
  "each transport family ... must handle valid end-of-stream termination
  correctly" goal, without adding a decoder that has nothing to decode.

## Why?

### Motivation

Azure OpenAI is the API-compatibility outlier of the four providers in
praxis-proxy/ai#762's scope, and that changes what "translation" even means
here:

1. **The request and response body schemas are not translated — they are
   already the same schema.** Azure OpenAI's Chat Completions endpoint was
   deliberately built by Microsoft to accept and return the same JSON shape
   OpenAI's own API does, specifically so existing OpenAI SDK clients work
   against it with only a base URL and auth change. Treating this like
   Bedrock or Vertex — writing a `request.rs`/`response.rs` full field-mapping
   layer — would be manufacturing complexity the APIs' own compatibility
   contract doesn't require, and 00762 explicitly scopes this work to
   "pinned release fixtures," not invented busywork.
2. **What *does* need translation is entirely in routing and credentials**:
   the consumer's `model` string has to resolve to an Azure *deployment*
   name (an operator-chosen identifier, not necessarily equal to the model
   string), the URL has to gain a required `api-version` query parameter,
   and Azure's primary auth surface (`api-key`) is a header scheme the
   existing `credential_inject` filter cannot express today (it only emits
   `Authorization: Bearer <token>`).

### User Stories

(Inherits the general user stories from 00762 — consumer stability, operator
config-only provider swaps, transparent streaming, provider-identity
integrity, fail-closed translation. Adding the Azure-specific one:)

- As a **platform operator**, I want to point Praxis at an Azure OpenAI
  deployment using the same `credential_inject` mechanism I already use for
  other providers, instead of maintaining a second, Azure-only credential
  path, even though Azure's primary auth header is not `Authorization`.

## How?

### Requirements

Restating praxis-proxy/ai#780's acceptance criteria as requirements this
design must satisfy:

1. Request transform OpenAI → Azure OpenAI deployment endpoint (path
   rewrite + `api-version`), fixture-backed.
2. Response transform Azure OpenAI → OpenAI, fixture-backed.
3. Error-response transform, fixture-backed.
4. Streaming transform across arbitrary chunk boundaries using the SSE
   decoder; event framing and ordering preserved.
5. Provider identity derived exclusively from the trusted route result;
   unrecognized/missing provider rejected before translation.
6. Consumer-supplied credentials stripped (`api-key` header, `Authorization`
   bearer), verified by a fixture.
7. Deterministic, documented header and body mutation ordering.
8. Translation runs only after the caller/model authorization stage.
9. Fixtures carry provenance and are scanned for real credential material.
10. End-to-end "capital of France" test against a real Azure endpoint or a
    strict protocol simulator.
11. Example config in `examples/configs/` plus a functional integration test.

### Design

#### Filter chain

```yaml
filter_chains:
  - name: azure-transform
    filters:
      - filter: azure_translate
        api_version: "2024-10-21"
        deployments:
          gpt-4o: prod-gpt4o-eastus2
          gpt-4o-mini: prod-gpt4o-mini-eastus2
        max_body_bytes: 1048576
        credential:                   # optional; see "Credential handling"
          strategy: bearer_token
          header: api-key             # new, optional field — see below
          value_prefix: ""            # new, optional field — see below
          secret_ref:
            name: azure-api-key
            namespace: azure-gateway
            key: token

      - filter: azure_stream_events
        max_buffer_bytes: 10485760
        response_conditions:
          - when:
              headers:
                content-type: "text/event-stream"

      - filter: credential_inject
        credentials:
          - name: azure-api-key
            namespace: azure-gateway
            key: token
            strategy: bearer_token
            header: api-key            # new, optional; default unchanged
            value_prefix: ""           # new, optional; default "Bearer "
            file: /run/secrets/azure-api-key/token

      - filter: router
        routes:
          - path_prefix: "/"
            cluster: "azure-openai-runtime"

      - filter: load_balancer
        clusters:
          - name: "azure-openai-runtime"
            endpoints:
              - "my-resource.openai.azure.com:443"
```

New crate module layout, deliberately smaller than
`apis/src/bedrock/`/`apis/src/vertex/` because there is less to translate:

```
apis/src/azure/
  mod.rs                 # registers azure_translate + azure_stream_events
  deployment.rs           # model -> deployment allowlist/map + path construction
  error.rs                 # defensive fallback error wrapping (see below)
  stream_events/
    mod.rs               # azure_stream_events filter (frame-boundary safety net)
```

Notice there is no `request.rs`/`response.rs` body-mapping module — see
"Request and response body translation" below for why.

#### Provider identity and the deployment map (requirement 5)

Same structural argument as praxis-proxy/ai#778 and praxis-proxy/ai#779:
provider identity is fixed by which filter chain and `load_balancer`
cluster the operator configured — this chain only ever calls the one Azure
resource its `load_balancer` endpoint points at.

What is consumer-influenced is `model`, and here Azure needs slightly more
than Bedrock/Vertex's flat allowlist: the consumer's `model` string does
not have to equal the Azure *deployment* name (an operator names deployments
however they like — `prod-gpt4o-eastus2` is a realistic example, not
`gpt-4o`). So `azure_translate`'s config is a **map**, `deployments: {model
=> deployment}`, not a bare list. An unrecognized `model` is rejected `404`
(same convention `bedrock_translate`/`vertex_translate` use for "wrong
model"); a missing `model` field is rejected `400`. Only a
mapped-and-validated deployment name is percent-encoded into the path
(`percent-encoding`, already a workspace dependency).

#### Reading the request during the header phase

Identical mechanism to `bedrock_translate`/`vertex_translate`:
`azure_translate` declares `request_body_mode() = BodyMode::StreamBuffer {
max_bytes: Some(config.max_body_bytes) }`, forcing the protocol-layer
pre-read documented on `HttpFilterContext::buffered_request_body`
(`praxis-proxy-filter` crate, `src/context.rs`) before `on_request` runs.
`on_request` reads `model` from `ctx.buffered_request_body`, resolves it
through the `deployments` map, and sets:

```
/openai/deployments/{deployment}/chat/completions?api-version={api_version}
```

as `ctx.rewritten_path` — the same "path plus query" convention the
existing `url_rewrite` builtin filter already uses.

#### Request and response body translation (requirements 1–3)

This is the part of the design that is deliberately *not* a large amount of
new code, and stating why is the substance of this proposal's "How":

- **Request body**: `on_request_body` removes the top-level `model` field
  (Azure derives the model from the already-selected deployment in the URL;
  the field is redundant on the wire, but consumers still send it because
  it is required by the OpenAI request schema they're coding against) and
  passes every other field through byte-for-byte unchanged. This is a real,
  fixture-backed transform — with two concrete assertions: the top-level
  `model` key is absent from the output, and every other field, however
  deeply nested (`messages[]`, `tools[]`, `response_format`, etc.), is
  identical to the input — not a hand-wave, but a narrow one.
- **Response body, 2xx**: pass-through unchanged. Azure's Chat Completions
  response is already OpenAI's own response schema. The one real Azure
  addition — an optional `prompt_filter_results`/`content_filter_results`
  object carrying Azure's content-safety annotations — is left in place
  rather than stripped: 00762 frames translation as "a pure protocol
  concern," and removing safety-relevant provider metadata that additive,
  spec-tolerant JSON fields do not break for any OpenAI-compatible client
  is not a protocol necessity, just data loss. The fixture for this
  transform is, correctly, an identity fixture: same bytes in, same bytes
  out, for the pinned API version — proving compatibility rather than
  manufacturing a mapping function that doesn't need to exist.
- **Error body, non-2xx**: Azure's error envelope
  (`{"error": {"code", "message", "param", "type"}}`) is already OpenAI's
  own error envelope, by the same wire-compatibility design. Pass through
  unchanged for the pinned API version, with one defensive fallback: if a
  response arrives with a non-2xx status but a body that does *not* parse
  as that envelope (for example, from an API Management gateway or WAF
  sitting in front of the Azure endpoint), `error::wrap_unrecognized_error`
  wraps the raw body text into a schema-complete OpenAI error envelope
  rather than forwarding an unparseable body to the consumer — the same
  fail-closed-on-untranslatable-input rule 00762 requires generally, sized
  to the one case Azure's compatibility contract doesn't already cover.

#### Streaming translation (requirement 4)

Because Azure's SSE `data:` payloads are already OpenAI
`chat.completion.chunk` objects, there is nothing to re-encode — adding a
filter that parses each chunk's JSON and re-serializes an identical object
would be reinventing work for no behavioral gain, and 00762's goals do not
ask for translation where none is needed.

What genuinely is worth doing, and what `azure_stream_events` actually
does, is providing the same **stream-completion correctness guarantee**
00762 requires for every provider's transport family ("must handle valid
end-of-stream termination correctly regardless of chunk boundaries") —
*without* re-encoding the body. It self-arms via metadata set by
`azure_translate` (`azure_translate.streaming == "true"`) plus a
`Content-Type: text/event-stream` check, declares
`response_body_mode() = BodyMode::Stream`, and reuses the same
`apis/src/openai/sse::SseFrameParser` + `HttpFilterContext::insert_filter_state`
pattern the Vertex proposal uses (praxis-proxy/ai#779,
docs/proposals/00779_vertex-translation.md) — but in a **read-only**
capacity: it feeds each chunk to the parser purely to track frame
boundaries and detect `SseParseError::MissingTerminalEvent` (a real,
already-modeled error variant in that parser) if the upstream connection
closes before a `data: [DONE]` frame arrives, while forwarding the original
bytes to the client completely untouched. A truncated stream is logged at
`warn` with a `azure_stream.truncated` metadata flag for observability —
the same "surface the failure, don't let it pass silently" principle behind
the token-accounting overflow fix (praxis-proxy/ai#674) — rather than
either re-encoding frames it doesn't need to touch, or saying nothing when
a stream ends abnormally.

#### Credential handling: closing the `api-key` gap in `credential_inject`

Azure OpenAI has two credential surfaces:

- **AAD (Microsoft Entra ID) OAuth2 bearer tokens** in `Authorization` —
  supported by `credential_inject`'s existing `bearer_token` strategy
  **exactly as it exists today, zero code changes**. An operator who wants
  to avoid touching shared `filters/src` code can use this path immediately.
- **`api-key`**, a static header (not `Authorization`, not a `Bearer`
  prefix) — Azure's simpler, more commonly used auth mode, and the one this
  proposal's example config leads with. `credential_inject` cannot express
  this today: its header name and `Bearer ` prefix are both hardcoded
  (`http::header::AUTHORIZATION`, and the literal formatting in its
  injection code).

Rather than writing a second, Azure-only credential-injection filter that
duplicates `credential_inject`'s already-correct secret handling (value/
env_var/file source precedence, `Zeroizing` wrapping, bounded lengths,
"never write token bytes to metadata/tracing/error bodies"), this proposal
adds two small, optional, backward-compatible fields to
`CredentialEntryConfig` in `filters/src/routing/credential_inject.rs`:

- `header` (default: `Authorization`) — the header name to set.
- `value_prefix` (default: `"Bearer "`) — the string prepended to the token.

Omitting both preserves today's exact behavior for every existing
`credential_inject` config, so this is additive, not a breaking change.
Azure's `api-key` scheme becomes `header: api-key`, `value_prefix: ""`.
This is `filters`-crate shared infrastructure, not `apis`-crate-local code,
so it is called out as its own graduation criterion for review by whoever
owns that filter, rather than folded silently into the Azure-specific work.

`azure_translate` strips *both* possible incoming credential headers
unconditionally (`api-key` and `Authorization`) regardless of which one the
operator's `credential_inject` config re-injects, so a consumer cannot
smuggle a stale or forged value through in the header the gateway isn't
actively managing.

#### Mutation ordering (requirement 7)

Within `azure_translate::on_request`: (1) parse the pre-read body, (2)
resolve `model` through the `deployments` map — reject before any mutation
if unmapped, (3) strip `api-key` and `Authorization` headers, (4) build and
set `rewritten_path` (deployment + `api-version`), (5) set credential
metadata if configured, (6) set `azure_translate.{deployment,streaming}`
metadata for `azure_stream_events`. Documented in the filter's module-level
rustdoc, the same ordering discipline `bedrock_translate`/`vertex_translate`
document for themselves.

#### Authorization ordering (requirement 8)

`azure_translate` is placed after any caller/model authorization filters in
the example config's `filters:` list and performs no authorization
decisions itself — identical positioning rule to the other two proposals.

#### Fixtures and testing (requirements 9–11)

- **Unit + fixture tests**: request-body fixture (asserts `model` removed,
  everything else identical), response-body identity fixture (pinned API
  version, byte-for-byte), error-envelope identity fixture plus one
  malformed-upstream-error negative fixture exercising
  `wrap_unrecognized_error`, and a truncated-stream negative fixture
  exercising `SseParseError::MissingTerminalEvent` detection — each scanned
  for secret material before merge.
- **Integration test**: `examples/configs/azure/chat-completions.yaml` (and
  a streaming variant) plus a functional integration test using
  `praxis_test_utils::Backend::fixed`, following the pattern of
  `tests/integration/tests/suite/examples/anthropic_messages.rs`.
- **End-to-end "capital of France" test**: an OpenAI-shaped
  `POST /v1/chat/completions` with `{"model": "gpt-4o", "messages":
  [{"role": "user", "content": "What is the capital of France?"}]}` through
  the proxy, asserting `choices[0].message.content` contains "Paris" — run
  against a strict Azure protocol simulator for CI, with the same request
  runnable against a real Azure OpenAI deployment when credentials are
  available.

### Why this satisfies praxis-proxy/ai#780

| Acceptance criterion | Satisfied by |
|---|---|
| OpenAI → Azure deployment endpoint transform (path + `api-version`) | `deployment.rs` map resolution + `rewritten_path` construction, fixture-backed |
| Azure → OpenAI response transform | Identity pass-through, fixture-verified for the pinned API version (correct because the schemas already match) |
| Error-response transform | Identity pass-through + `wrap_unrecognized_error` fallback, fixture-backed |
| Streaming transform, arbitrary chunk boundaries, framing/ordering preserved | Untouched byte pass-through (trivially preserves framing/ordering) + read-only `SseFrameParser`-based termination check |
| Provider identity from trusted route result only | Provider fixed by filter-chain/cluster config; `model` resolved through an operator-owned map before path construction |
| Consumer credentials stripped, `api-key` + `Authorization` | Both stripped unconditionally in `azure_translate::on_request` |
| Deterministic mutation ordering | Fixed, documented 6-step order in `on_request` |
| Translation after authorization | Filter chain position; framework-enforced execution order |
| Fixture provenance + secret scan | Provenance headers per fixture; pre-merge secret scan, per 00762 |
| End-to-end "capital of France" | Integration test against `Backend::fixed` simulator + real-endpoint-capable |
| Example config + functional integration test | `examples/configs/azure/*.yaml` + `tests/integration/tests/suite/examples/azure.rs` |

## Related

- [praxis-proxy/ai#780](https://github.com/praxis-proxy/ai/issues/780) —
  tracking issue this proposal implements.
- [docs/proposals/00762_api-translation.md](00762_api-translation.md) —
  parent proposal; accepted What?/Why? for provider translation in general.
- [docs/proposals/00778_bedrock-translation.md](00778_bedrock-translation.md),
  [docs/proposals/00779_vertex-translation.md](00779_vertex-translation.md) —
  sibling proposals; share the model-resolution-before-routing and
  body-pre-read design.
- [praxis-proxy/ai#674](https://github.com/praxis-proxy/ai/issues/674) —
  "surface the failure, don't drop it silently" principle reused for
  truncated-stream detection.
