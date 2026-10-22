#!/usr/bin/env python3
"""Checks the command-line reference (docs/public/src/reference/) against
the binaries.

For each binary, runs it without arguments, which prints its usage line
and exits with status 2, and checks that its reference page quotes that
line exactly, and that the page's option table lists every `--option` of
the usage line and no other.

Usage: scripts/cli-docs-check.py [BIN_DIR]   (default target/release)
"""

import re
import subprocess
import sys
from pathlib import Path

ROOT = Path(__file__).resolve().parent.parent
REFERENCE = ROOT / "docs" / "public" / "src" / "reference"
BINARIES = ["hyperstor-meta", "hyperstord", "hyperstor-bench", "hyperstorctl", "hyperstor-install", "hyperstor-mgmt", "hyperstor-ublk"]
OPTION = re.compile(r"--[a-z][a-z-]*")

bin_dir = Path(sys.argv[1]) if len(sys.argv) > 1 else ROOT / "target" / "release"
errors = []
for name in BINARIES:
    page = REFERENCE / f"{name}.md"
    if not page.exists():
        errors.append(f"{page.relative_to(ROOT)}: missing")
        continue
    run = subprocess.run([bin_dir / name], capture_output=True, text=True, timeout=10)
    usage = [l for l in run.stderr.splitlines() if l.startswith("usage:")]
    if run.returncode != 2 or len(usage) != 1:
        errors.append(f"{name}: expected one usage line and exit status 2, got {run.returncode}")
        continue
    usage = usage[0]
    text = page.read_text(encoding="utf-8")
    if usage not in text.splitlines():
        errors.append(f"{page.relative_to(ROOT)}: doesn't quote the usage line\n    {usage}")
    in_usage = set(OPTION.findall(usage))
    in_table = {
        m.group(1)
        for m in re.finditer(r"^\| `(--[a-z][a-z-]*)[^`]*` \|", text, re.M)
    }
    for o in sorted(in_usage - in_table):
        errors.append(f"{page.relative_to(ROOT)}: {o} is not in the option table")
    for o in sorted(in_table - in_usage):
        errors.append(f"{page.relative_to(ROOT)}: {o} is documented but {name} has no such option")

for e in errors:
    print(e)
print(f"cli-docs-check: {len(BINARIES)} binaries, {len(errors)} error(s)", file=sys.stderr)
sys.exit(1 if errors else 0)
