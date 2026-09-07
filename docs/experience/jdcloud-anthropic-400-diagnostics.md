# JDCloud Anthropic 400 diagnostics

## Context

Recent Claude proxy failures from JDCloud Anthropic gateways can return generic HTTP 400 messages such as `bad response status code 400` or `请求被拒绝或参数有误` without a field-level validation path. Before changing routing or provider settings, first correlate the failing provider and its `isFullUrl` flag from `proxy_request_logs` and `providers.meta.isFullUrl`.

## Decision

Add structure-only outbound diagnostics for Bedrock-compatible Anthropic providers. The log is emitted after the outbound body is finalized and before sending to upstream, so it reflects model mapping, sanitizer changes, endpoint preparation, and local body overrides.

The first 2026-09 JDCloud failure pattern was not caused by a missing Full URL flag. The affected provider already had `meta.isFullUrl=true`; the 400s correlated with Anthropic Messages requests shaped as `thinking.type=adaptive` plus `max_tokens=64000`, while diagnostics showed `context_management=false` and unsupported tool-schema counters at zero. For JDCloud/Bedrock-compatible Anthropic providers, strip adaptive thinking and clamp `max_tokens` to `32000` before upstream dispatch so the request falls back to a conservative shape that matched successful observed warmup/disabled calls.

A later `/responses` → Claude fallback failure had already passed that downgrade: `max_tokens=16384`, no `context_management`, no adaptive thinking, top-level unsupported schema count zero, but `nested_unsupported=12` remained. Treat nested JSON schema composition/enum keywords as JDCloud/Bedrock-incompatible too; recursively strip unsupported schema keywords while preserving property names under `properties`.

A follow-up 400 after recursive schema cleanup showed `nested_unsupported=0`; nearby logs had the reactive thinking-signature rectifier remove historical thinking blocks and then succeed. Because JDCloud sometimes returns only generic `upstream request failed` instead of a signature-specific error, proactively run the same history thinking/signature cleanup for JDCloud/Bedrock Anthropic requests before dispatch.

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
