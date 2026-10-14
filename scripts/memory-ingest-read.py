from __future__ import annotations

import argparse
import json
import os
import sys
import time
from pathlib import Path

def user_texts(path: Path) -> list[str]:
    out: list[str] = []
    with path.open(encoding="utf-8", errors="replace") as handle:
        handle.readline()
        for line in handle:
            if '"role":"user"' not in line:
                continue
            try:
                message = json.loads(line).get("message") or {}
            except json.JSONDecodeError:
                continue
            if message.get("role") != "user":
                continue
            content = message.get("content")
            if not isinstance(content, list):
                continue
            text = "".join(
                part.get("text", "")
                for part in content
                if isinstance(part, dict) and part.get("type") == "text"
            ).strip()
            if text:
                out.append(text)
    return out

def main() -> None:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--sessions", default=os.path.expanduser("~/.gray/sessions"))
    parser.add_argument("--cwd", required=True, help="only transcripts whose header cwd is exactly this")
    parser.add_argument("--hours", type=int, default=24)
    parser.add_argument("--max-sessions", type=int, default=10)
    parser.add_argument("--max-bytes", type=int, default=200_000)
    args = parser.parse_args()

    cutoff = time.time() - args.hours * 3600
    try:
        files = sorted(
            (p for p in Path(args.sessions).glob("*.jsonl")),
            key=lambda p: p.stat().st_mtime,
            reverse=True,
        )
    except OSError as exc:
        print(f"cannot list sessions: {exc}", file=sys.stderr)
        raise SystemExit(1) from exc

    used = matched = 0
    for path in files:
        if matched >= args.max_sessions or used >= args.max_bytes:
            break
        try:
            if path.stat().st_mtime < cutoff:
                break
            with path.open(encoding="utf-8", errors="replace") as handle:
                header = json.loads(handle.readline() or "{}")
        except (OSError, json.JSONDecodeError):
            continue
        if header.get("cwd") != args.cwd:
            continue
        texts = user_texts(path)
        if not texts:
            continue
        matched += 1

        stamp = time.strftime("%Y-%m-%d %H:%M", time.localtime(path.stat().st_mtime))
        block = f"=== {path.stem} {stamp} ===\n" + "\n---\n".join(texts) + "\n"
        room = args.max_bytes - used
        sys.stdout.write(block[:room])
        used += min(len(block), room)

    if not matched:
        print(f"(no user messages in the last {args.hours}h for {args.cwd})")

if __name__ == "__main__":
    main()
