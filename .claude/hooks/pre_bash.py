#!/usr/bin/env python3
"""PreToolUse security gate for Bash commands."""
import sys
import json

try:
    inp = json.load(sys.stdin)
    cmd = inp.get("tool_input", {}).get("command", "").strip()
except Exception:
    sys.exit(0)

blocked_tokens = [
    ("git", "commit", "--no-verify"),
    ("git", "push", "-f"),
    ("chmod", "777"),
]

tokens = cmd.lower().split()
for pattern in blocked_tokens:
    if all(tok in tokens for tok in pattern):
        print(f"[security-hook] Blocked: {' '.join(pattern)}", file=sys.stderr)
        sys.exit(2)

sys.exit(0)
