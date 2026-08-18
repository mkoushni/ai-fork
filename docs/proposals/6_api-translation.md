---
issue: https://github.com/praxis-proxy/ai/issues/762
discussion: >-
  Originally opened as a sub-task of epic issue
  opendatahub-io/praxis-extproc#6, which served as the approved discussion
  artifact per maintainer agreement (no separate GitHub Discussion opened).
  Per opendatahub-io/praxis-extproc#31 (comment), this proposal was migrated
  into this repository because the work belongs in this repo's filter tree.
  It is re-scoped here as the proposal for praxis-proxy/ai#762, a sub-task
  of the pre-existing praxis-proxy/ai#114 (itself under Epic
  praxis-proxy/ai#363) carrying the translator acceptance criteria adapted
  from opendatahub-io/praxis-extproc#6, since Anthropic <-> OpenAI
  translation is already covered by praxis-proxy/ai#103 and
  praxis-proxy/ai#638. See opendatahub-io/praxis-extproc#6 and
  praxis-proxy/ai#762.
status: proposed
authors:
  - mkoushni
graduation_criteria:
  - "How? section with requirements and design"
stakeholders:
  - # TODO: add relevant maintainer/domain-expert handles before merge
---

# API Translation

## What?

Introduce a provider-aware translation stage to the Praxis filter pipeline
that bidirectionally rewrites inference traffic between the OpenAI-shaped
consumer-facing contract and the wire formats of AWS Bedrock, Google
Vertex AI, Azure OpenAI, and Cohere — the provider set tracked by
[praxis-proxy/ai#762](https://github.com/praxis-proxy/ai/issues/762), a
sub-task of [praxis-proxy/ai#114](https://github.com/praxis-proxy/ai/issues/114).
The stage operates after authorization and route selection have completed,
covers both buffered and streaming (SSE and non-SSE) responses, and
produces a deterministic, ordered set of header and body mutations.

Anthropic Messages <-> OpenAI translation is **explicitly out of scope**
here: it already has a working, tested implementation
(`apis/src/anthropic/to_openai/`, `apis/src/anthropic/stream_events/`)
tracked by praxis-proxy/ai#103 and praxis-proxy/ai#638. This proposal
covers only the remaining, currently untranslated providers.

The translation stage derives the target provider exclusively from the
trusted route result written into stream state by the authorization and
routing stage. Provider identity is never inferred from consumer-controlled
inputs such as model names, URI paths, or request headers. If the trusted
route result is absent or names a provider not in the configured allowlist,
the stage rejects the request and halts the pipeline.

Translation is a **pure protocol concern** — request/response schema and
streaming-transport rewriting only.

### Goals

- **Bidirectional format translation.** Rewrite OpenAI-shaped consumer
  inference requests to the wire format required by the selected provider,
  and rewrite provider responses back to the canonical OpenAI-shaped
  consumer-facing schema. Supported providers are defined by a versioned,
  operator-configured allowlist. Each allowlist entry must declare: the
  provider identifier, transport protocol, request and response schemas,
  and a fixture manifest. Providers for the first release, matching
  [praxis-proxy/ai#762](https://github.com/praxis-proxy/ai/issues/762):
  **AWS Bedrock** (Converse and InvokeModel), **Google Vertex AI**
  (Gemini), **Azure OpenAI**, and **Cohere**. Anthropic is intentionally
  excluded — it is already covered by the existing `anthropic_to_openai`
  filter (praxis-proxy/ai#103, praxis-proxy/ai#638). Adding a provider
  requires an explicit allowlist entry with all required fields — fixture
  inclusion alone does not grant support scope.

- **Streaming correctness with transport-specific framing.** Provider
  streaming transports are not uniform and must be handled with
  transport-specific decoders rather than a single SSE path:

  - **Azure OpenAI / Cohere** use Server-Sent Events (`text/event-stream`):
    `data:` lines, blank-line delimiters, and `data: [DONE]`-style
    termination, though each provider's event payload shape differs from
    OpenAI's and must be mapped explicitly rather than assumed compatible.
  - **Amazon Bedrock** (`InvokeModelWithResponseStream`) uses
    `application/vnd.amazon.eventstream` binary framing: length-prefixed
    messages with headers, payload, and CRC32 checksums. This is not SSE
    and must not be processed through the SSE decoder. In-stream exception
    events and normal stream completion must both be mapped to the
    consumer contract.
  - **Vertex AI** uses its own event-stream framing and must be handled
    separately.

  Each transport requires its own decoder with fixtures covering: normal
  chunk events, stream completion, and in-stream exception or error events.

  All decoders must operate **incrementally**: maintain only a bounded
  incomplete-frame buffer, emit complete events as soon as they are
  recognised, and never buffer the full stream. This filter framework
  already exposes `BodyMode::Stream` for exactly this purpose (see e.g.
  `apis/src/anthropic/stream_events/`) — the streaming translation path
  for each new provider must use it rather than `BodyMode::StreamBuffer`,
  and must handle valid end-of-stream termination correctly regardless of
  chunk boundaries.

- **Deterministic mutation ordering.** Header and body mutations produced
  by translation must be applied in a fixed, documented order. This makes
  the translation pipeline predictable, independently testable, and safe
  to compose with other filter stages.

- **Fixture-backed correctness with provenance and negative coverage.**
  Fixtures are the acceptance gate — a translation is correct when its
  output matches the fixture, not when it passes a unit test written from
  the same assumptions as the implementation. To prevent a stale or
  incomplete contract from passing the gate, fixtures must satisfy the
  following requirements:

  - **Provenance.** Each fixture records the provider API version, model
    schema version, and the source of truth it was derived from (e.g.
    provider SDK test suite, live endpoint capture, specification). Fixture
    updates require a corresponding provenance update.
  - **Secret scan.** Fixtures must be scanned for secret material before
    merge. Any fixture containing a real key, token, or signature must
    be rejected.
  - **Positive coverage.** Normal request/response, error response, all
    supported streaming transports (SSE for Azure/Cohere, Bedrock
    event-stream, Vertex AI event-stream), and normal end-of-stream
    termination.
  - **Negative coverage.** Unsupported or unknown request fields,
    malformed stream frames, arbitrary chunk splits across frame
    boundaries, in-stream exception and error events, and mid-stream
    provider failures. A negative fixture that does not assert a
    rejection is not a negative fixture.

- **Fail closed on untranslatable input.** If the translation stage
  cannot produce a valid provider request (missing required field,
  unsupported model parameter, schema mismatch), it must reject the
  request with a stable error response rather than forwarding a malformed
  request to the provider.

- **Trusted provider context, explicit allowlist.** The translation stage
  must operate from an explicit, operator-configured allowlist of known
  providers. Provider identity is read exclusively from the trusted route
  result in stream state — it must never be inferred or overridden from
  consumer-controlled inputs (model names, URI paths, headers, or body
  fields). A missing route result, an unrecognized provider identifier,
  or a conflict between route-state values must cause an immediate
  rejection before any translation or mutation is applied.

- **Authorization ordering enforced.** Translation must not execute
  before the authorization stage has completed. The enforcement strategy
  will be detailed in the How? section.

### Non-Goals

- **Anthropic Messages translation.** Already implemented and tracked
  separately by praxis-proxy/ai#103 and praxis-proxy/ai#638
  (`apis/src/anthropic/to_openai/`). Not duplicated here.
- **Provider-specific field passthrough.** Preserving unknown/
  provider-specific request fields (e.g. Anthropic's `top_k`, Bedrock's
  `guardrailConfig`) through translation without validation errors is
  tracked separately by praxis-proxy/ai#385.
- **Routing and authorization.** Provider selection and caller
  authorization are upstream concerns, already covered by existing Praxis
  stages.
- **Feature parity with every provider capability.** The scope is the
  pinned release fixtures, not a complete adapter for every provider
  endpoint or extension.
- **Consumer-to-consumer format bridging.** This proposal covers
  consumer-to-provider and provider-to-consumer translation only. It does
  not introduce a canonical intermediate representation as a new
  public API surface.

## Why?

### Motivation

Each inference provider exposes an incompatible proprietary API: field
names, request envelope shapes, error codes, and streaming event schemas
all differ across Bedrock, Vertex AI, Azure OpenAI, and Cohere — the
provider set tracked by
[praxis-proxy/ai#762](https://github.com/praxis-proxy/ai/issues/762).
(Anthropic already has this solved; see praxis-proxy/ai#103.) Praxis can
already select a provider by routing, but the routed request still
carries the consumer's OpenAI-shaped wire format. Without a translation
layer for these remaining providers, one of two failure modes applies:

1. **Consumer complexity.** Every calling application must implement
   provider-specific adapters, exposing business code to the full surface
   area of every provider and making provider migration an application
   change.
2. **Operational fragmentation.** Operators run separate ingress
   endpoints per provider, preventing unified policy enforcement,
   observability, and traffic shaping.

A translation layer eliminates both failure modes by making provider
heterogeneity an infrastructure concern:

- **Consumer stability.** Applications code to one stable inference
  contract. Provider migrations are configuration changes, not code
  changes.
- **Operational coherence.** A single gateway endpoint handles all
  providers. Policy, rate limiting, observability, and routing all
  operate uniformly.
- **Testability.** Deterministic, ordered mutations with golden fixtures
  mean translation correctness can be verified independently of the
  routing and authorization stages.

### User Stories

- As an **AI application developer**, I want to send requests in a stable
  consumer-facing format regardless of which provider backs my model,
  so that I can migrate between providers without changing my application
  code or adding provider-specific logic to my client.

- As a **platform operator**, I want provider format translation to be
  handled automatically by the gateway after a route is selected, so
  that I can add, remove, or swap providers through configuration without
  requiring any change to consumer-side code or interfaces.

- As a **platform operator**, I want streaming and SSE responses to be
  translated transparently — preserving event framing and ordering — so
  that clients using streaming inference observe consistent behavior
  regardless of which provider served the request.

- As a **security engineer**, I want provider identity derived
  exclusively from the trusted route result stored in stream state, and
  never inferred from consumer-controlled model names, URI paths, or
  headers, so that a consumer cannot steer the translation stage toward
  a provider they are not authorized to reach.

- As a **security engineer**, I want translation to fail closed when
  input cannot be translated to a valid provider request, so that
  malformed or unexpected consumer payloads never reach a provider in an
  undefined state.

- As a **platform engineer**, I want every provider translation to be
  covered by golden request, response, error, and streaming fixtures,
  so that regressions in format correctness are caught before reaching
  production, independently of provider availability.

## Related

- [praxis-proxy/ai#762](https://github.com/praxis-proxy/ai/issues/762) —
  tracking issue this proposal implements (translator acceptance criteria
  adapted from opendatahub-io/praxis-extproc#6).
- [praxis-proxy/ai#114](https://github.com/praxis-proxy/ai/issues/114) —
  parent issue: Schema translation: OpenAI to Bedrock, Vertex AI, Azure,
  Cohere.
- [praxis-proxy/ai#363](https://github.com/praxis-proxy/ai/issues/363) —
  parent Epic: API Translation.
- [praxis-proxy/ai#103](https://github.com/praxis-proxy/ai/issues/103),
  [praxis-proxy/ai#638](https://github.com/praxis-proxy/ai/issues/638) —
  Anthropic Messages <-> OpenAI translation (already implemented; not
  duplicated here).
- [praxis-proxy/ai#385](https://github.com/praxis-proxy/ai/issues/385) —
  provider-specific field passthrough (separate, not duplicated here).
