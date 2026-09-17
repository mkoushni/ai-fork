## Summary

Adds filtered subrequest support for the `ai_guardrails` NeMo provider.

## Changes

- Requires `outbound_chain` for production guardrails configuration.
- Executes NeMo callouts through the configured filtered outbound chain.
- Propagates request depth, deadlines, and downstream context safely.
- Enforces global `allow_private_upstreams` at runtime.
- Preserves shared subrequest client and circuit-breaker behavior.
- Adds registry validation for missing outbound chains.
- Adds integration coverage for outbound-chain execution, request ID propagation, private endpoint rejection, and provider failures.
- Updates NeMo examples and generated documentation.

## Testing

Passed:

- Guardrails unit tests: 73 passed
- Guardrails integration tests: 37 passed
- Clippy with `-D warnings`
- Formatting and diff checks

The full workspace suite currently has 24 `openai_agentic_loop` integration failures returning HTTP 500; these are unrelated to the guardrails-focused tests.
