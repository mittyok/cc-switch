# Repository Agent Rules

## Completion Discipline

- Do not use a standalone promise such as “I’ll continue”, “我继续看”, “马上处理”, or “接下来我会…” as a final assistant message.
- If an assistant message promises an action, the same turn must immediately perform the action with a tool call.
- Final replies must state only the actual outcome: completed work, incomplete work, validation status, or the concrete blocker.
- If more work is required but no tool call will be made, phrase it as a user-facing recommendation or next step, not as a promise that the agent is currently executing.

## Empty-Promise Diagnostics

- Use `python3 scripts/detect_empty_promise_turns.py` to scan Codex session logs for suspicious turns where a promise-like assistant message is followed by `task_complete` without a tool/function call.
- This repository should keep cc-switch proxy behavior stable; prefer diagnostics and local agent rules over protocol interception for this class of issue.
