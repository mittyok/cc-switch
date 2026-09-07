# JDCloud Anthropic 400 diagnostics

## Context

Recent Claude proxy failures from JDCloud Anthropic gateways can return generic HTTP 400 messages such as `bad response status code 400` or `请求被拒绝或参数有误` without a field-level validation path. Before changing routing or provider settings, first correlate the failing provider and its `isFullUrl` flag from `proxy_request_logs` and `providers.meta.isFullUrl`.

## Decision

Add structure-only outbound diagnostics for Bedrock-compatible Anthropic providers. The log is emitted after the outbound body is finalized and before sending to upstream, so it reflects model mapping, sanitizer changes, endpoint preparation, and local body overrides.

The log deliberately includes only request-shape facts:

- provider id/name and effective endpoint
- model, streaming flag, max token ceiling, thinking type
- message/tool counts and system-message shape
- whether `context_management`, `metadata`, and Anthropic beta headers are present
- counts of tool schemas that still contain top-level or nested Bedrock-unsupported JSON schema keywords
- a canonical body hash for correlation across repeated 400s

It must not log prompt text, tool names/descriptions, tool input examples, metadata values, API keys, auth headers, or raw request/response bodies.

## Gotchas

- Full endpoint URLs use `meta.isFullUrl` in serialized JSON, not `meta.is_full_url`.
- A provider can be correctly configured with Full URL enabled and still receive 400s from JDCloud/Bedrock schema validation or unsupported Anthropic beta fields.
- Existing sanitizer removes top-level tool schema keywords; diagnostics also counts nested unsupported keywords so future logs can show whether deeper schema constraints may explain generic upstream 400s.
