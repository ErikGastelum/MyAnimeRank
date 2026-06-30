#!/usr/bin/env python3
"""Stop hook: report uncommitted changes at session end."""
import subprocess
import sys

result = subprocess.run(["git", "diff", "--name-only"], capture_output=True, text=True)
changed = result.stdout.strip()
print(f"[session-end] Changed files: {changed or 'none'}", flush=True)
sys.exit(0)
