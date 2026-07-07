#!/usr/bin/env python3
"""Find Codex session turns that likely ended with an empty promise.

The detector scans Codex JSONL session files for turns where the final assistant
message before `task_complete` contains action-commitment phrasing but no later
function/tool call occurs in that same turn.
"""

from __future__ import annotations

import argparse
import json
import os
import re
from dataclasses import dataclass
from pathlib import Path
from typing import Any, Iterable

DEFAULT_SESSIONS_DIR = Path.home() / ".codex" / "sessions"

PROMISE_PATTERNS = [
    r"继续(?:看|查|排查|定位|处理|推进|验证|测试|跑|改|做|实现|补|写)",
    r"(?:(?:马上|立刻)(?:去|来)?|现在(?:去|来|继续))(?:看|查|排查|定位|处理|推进|验证|测试|跑|改|做|实现|补|写)",
    r"(?:我会|我将|我来|我去|我再|我继续|接下来(?:我)?(?:会|将|来|去)?|下一步(?:我)?(?:会|将|来|去)?)(?:[^。！？\n]{0,30})(?:看|查|排查|定位|处理|推进|验证|测试|跑|改|做|实现|补|写)",
    r"(?:I'll|I will|I’ll|I'm going to|I am going to|I can|Let me|Next(?:,| I)|Now(?:,| I))\b[^.!?\n]{0,80}\b(?:check|inspect|look|investigate|debug|run|test|update|patch|implement|add|write|fix|verify|continue)",
]
PROMISE_RE = re.compile("|".join(f"(?:{pattern})" for pattern in PROMISE_PATTERNS), re.IGNORECASE)
THINK_BLOCK_RE = re.compile(r"<think>.*?</think>", re.IGNORECASE | re.DOTALL)
CONDITIONAL_OR_RECOMMENDATION_RE = re.compile(
    r"(?:如果|要是|你先|你可以|你想|你给|或者你|需要时|when you|if you|you can)",
    re.IGNORECASE,
)

TOOL_PAYLOAD_TYPES = {
    "function_call",
    "custom_tool_call",
    "mcp_tool_call",
    "web_search_call",
}

TOOL_EVENT_TYPES = {
    "function_call",
    "custom_tool_call",
    "mcp_tool_call",
    "web_search_call",
}


@dataclass
class Finding:
    session: Path
    line: int
    timestamp: str
    snippet: str
    reason: str


def iter_session_files(root: Path) -> Iterable[Path]:
    if root.is_file():
        yield root
        return
    yield from sorted(root.glob("**/*.jsonl"))


def read_jsonl(path: Path) -> Iterable[tuple[int, dict[str, Any]]]:
    try:
        with path.open("r", encoding="utf-8") as handle:
            for line_number, line in enumerate(handle, 1):
                line = line.strip()
                if not line:
                    continue
                try:
                    value = json.loads(line)
                except json.JSONDecodeError:
                    continue
                if isinstance(value, dict):
                    yield line_number, value
    except OSError:
        return


def extract_text(content: Any) -> str:
    if isinstance(content, str):
        return content
    if isinstance(content, list):
        parts: list[str] = []
        for item in content:
            if isinstance(item, str):
                parts.append(item)
            elif isinstance(item, dict):
                text = item.get("text") or item.get("content")
                if isinstance(text, str):
                    parts.append(text)
        return "\n".join(parts)
    if isinstance(content, dict):
        text = content.get("text") or content.get("content")
        return text if isinstance(text, str) else ""
    return ""


def assistant_text(record: dict[str, Any]) -> str:
    payload = record.get("payload") or {}
    if payload.get("role") == "assistant" and payload.get("type") == "message":
        return extract_text(payload.get("content"))
    if payload.get("type") == "agent_message":
        message = payload.get("message")
        return message if isinstance(message, str) else ""
    return ""


def visible_assistant_text(text: str) -> str:
    """Return user-visible assistant text, ignoring inline reasoning blocks."""
    return THINK_BLOCK_RE.sub("", text).strip()


def is_tool_call(record: dict[str, Any]) -> bool:
    payload = record.get("payload") or {}
    payload_type = payload.get("type")
    record_type = record.get("type")
    if payload_type in TOOL_PAYLOAD_TYPES or record_type in TOOL_EVENT_TYPES:
        return True
    if isinstance(payload.get("tool_calls"), list) and payload["tool_calls"]:
        return True
    return False


def is_task_boundary(record: dict[str, Any]) -> bool:
    return (record.get("payload") or {}).get("type") in {"task_started", "turn_aborted"}


def is_task_complete(record: dict[str, Any]) -> bool:
    return (record.get("payload") or {}).get("type") == "task_complete"


def compact(text: str, limit: int = 180) -> str:
    text = re.sub(r"\s+", " ", text).strip()
    if len(text) <= limit:
        return text
    return text[: limit - 1].rstrip() + "…"


def sentence_around(text: str, start: int, end: int) -> str:
    left_boundaries = [text.rfind(mark, 0, start) for mark in "。！？!?\n"]
    right_boundaries = [idx for mark in "。！？!?\n" if (idx := text.find(mark, end)) != -1]
    left = max(left_boundaries) + 1 if left_boundaries else 0
    right = min(right_boundaries) if right_boundaries else len(text)
    return text[left:right].strip()


def promise_match(text: str) -> re.Match[str] | None:
    for match in PROMISE_RE.finditer(text):
        sentence = sentence_around(text, match.start(), match.end())
        if CONDITIONAL_OR_RECOMMENDATION_RE.search(sentence):
            continue
        return match
    return None


def scan_file(path: Path) -> list[Finding]:
    findings: list[Finding] = []
    last_assistant: tuple[int, str, str] | None = None
    tool_after_last_assistant = False

    for line_number, record in read_jsonl(path):
        if is_task_boundary(record):
            last_assistant = None
            tool_after_last_assistant = False
            continue

        text = assistant_text(record)
        if text:
            last_assistant = (line_number, record.get("timestamp", ""), visible_assistant_text(text))
            tool_after_last_assistant = False
            continue

        if is_tool_call(record) and last_assistant is not None:
            tool_after_last_assistant = True
            continue

        if is_task_complete(record):
            if last_assistant is not None and not tool_after_last_assistant:
                assistant_line, timestamp, final_text = last_assistant
                match = promise_match(final_text)
                if match:
                    findings.append(
                        Finding(
                            session=path,
                            line=assistant_line,
                            timestamp=timestamp,
                            snippet=compact(final_text),
                            reason=match.group(0),
                        )
                    )
            last_assistant = None
            tool_after_last_assistant = False

    return findings


def parse_args() -> argparse.Namespace:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument(
        "paths",
        nargs="*",
        type=Path,
        default=[DEFAULT_SESSIONS_DIR],
        help="Session JSONL file or directory to scan. Defaults to ~/.codex/sessions.",
    )
    parser.add_argument(
        "--json",
        action="store_true",
        help="Emit JSON output instead of human-readable lines.",
    )
    parser.add_argument(
        "--limit",
        type=int,
        default=50,
        help="Maximum findings to print. Use 0 for no limit.",
    )
    parser.add_argument(
        "--fail-on-found",
        action="store_true",
        help="Exit with status 1 when findings are detected.",
    )
    return parser.parse_args()


def main() -> int:
    args = parse_args()
    findings: list[Finding] = []
    for input_path in args.paths:
        path = Path(os.path.expanduser(str(input_path)))
        for session_file in iter_session_files(path):
            findings.extend(scan_file(session_file))

    visible = findings if args.limit == 0 else findings[: args.limit]
    if args.json:
        print(
            json.dumps(
                [
                    {
                        "session": str(finding.session),
                        "line": finding.line,
                        "timestamp": finding.timestamp,
                        "reason": finding.reason,
                        "snippet": finding.snippet,
                    }
                    for finding in visible
                ],
                ensure_ascii=False,
                indent=2,
            )
        )
    else:
        print(f"Found {len(findings)} suspicious empty-promise turn(s).")
        for finding in visible:
            location = f"{finding.session}:{finding.line}"
            timestamp = f" [{finding.timestamp}]" if finding.timestamp else ""
            print(f"- {location}{timestamp}")
            print(f"  trigger: {finding.reason}")
            print(f"  text: {finding.snippet}")
        if args.limit and len(findings) > args.limit:
            print(f"... {len(findings) - args.limit} more not shown; rerun with --limit 0")

    return 1 if args.fail_on_found and findings else 0


if __name__ == "__main__":
    raise SystemExit(main())
