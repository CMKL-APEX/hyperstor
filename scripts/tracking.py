#!/usr/bin/env python3
"""Implementation tracking (spec/tracking.toml).

    scripts/tracking.py check LOG...   check the file against `cargo test`
                                       output; print a Markdown report
    scripts/tracking.py status         print the public status page
                                       (docs/public/src/status.md)

`check` fails if a tracked test didn't run or didn't pass, if an item's
tests don't fit its status, or if the milestones don't match
spec/milestones.md §21. Tests that ran but aren't tracked are listed, not
errors: new tests land before the file catches up.
"""

import re
import sys
import tomllib
from pathlib import Path

ROOT = Path(__file__).resolve().parent.parent
STATUSES = ("done", "partial", "planned")
LABEL = {"done": "Done", "partial": "In progress", "planned": "Planned"}


def load():
    with open(ROOT / "spec" / "tracking.toml", "rb") as f:
        return tomllib.load(f)["milestone"]


def milestone_status(m):
    items = [i["status"] for i in m.get("item", [])]
    if items and all(s == "done" for s in items):
        return "done"
    if any(s in ("done", "partial") for s in items):
        return "partial"
    return "planned"


RUNNING = re.compile(r"^\s+Running (?:unittests )?\S+ \(.*/deps/(.+)-[0-9a-f]+(?:\.exe)?\)$")
RESULT = re.compile(r"^test (\S+) \.\.\. (ok|FAILED|ignored)")


def parse_logs(paths):
    """Returns {"binary::path": "ok" | "FAILED" | "ignored"}."""
    results = {}
    for path in paths:
        binary = None
        for text in Path(path).read_text(encoding="utf-8", errors="replace").splitlines():
            m = RUNNING.match(text)
            if m:
                binary = m.group(1)
                continue
            if text.startswith("   Doc-tests "):
                binary = None
                continue
            m = RESULT.match(text)
            if m and binary:
                results[f"{binary}::{m.group(1)}"] = m.group(2)
    return results


def check(paths):
    milestones = load()
    results = parse_logs(paths)
    errors = []
    if not results:
        errors.append("no test results found in the logs")

    table = (ROOT / "spec" / "milestones.md").read_text(encoding="utf-8")
    in_spec = re.findall(r"^\| (HS\d+[a-z]?\+?) \|", table, re.M)
    in_spec = [i.rstrip("+") for i in in_spec]
    ids = [m["id"] for m in milestones]
    for i in in_spec:
        if i not in ids:
            errors.append(f"milestone {i} (milestones.md §21) is not tracked")
    for i in ids:
        if i not in in_spec:
            errors.append(f"milestone {i} is not in milestones.md §21")

    tracked = set()
    for m in milestones:
        for item in m.get("item", []):
            where = f"{m['id']}: {item['what']}"
            status, tests = item.get("status"), item.get("tests", [])
            if status not in STATUSES:
                errors.append(f"{where}: status must be one of {', '.join(STATUSES)}")
            if status == "done" and not tests:
                errors.append(f"{where}: done, but names no test")
            if status == "planned" and tests:
                errors.append(f"{where}: planned, but names tests")
            for t in tests:
                if t in tracked:
                    errors.append(f"{where}: {t} is tracked twice")
                tracked.add(t)
                r = results.get(t)
                if r is None:
                    errors.append(f"{where}: {t} did not run (renamed or removed?)")
                elif r != "ok":
                    errors.append(f"{where}: {t} {r}")

    out = ["## Implementation tracking", ""]
    if errors:
        out += ["**Errors:**", ""] + [f"- {e}" for e in errors] + [""]
    passed = sum(1 for r in results.values() if r == "ok")
    out += [
        f"{passed} of {len(results)} tests passed; "
        f"{len(tracked & results.keys())} are tracked against the spec.",
        "",
        "| Milestone | Status | Items done | Tests |",
        "|---|---|---|---|",
    ]
    for m in milestones:
        items = m.get("item", [])
        done = sum(1 for i in items if i["status"] == "done")
        tests = sum(len(i.get("tests", [])) for i in items)
        out.append(
            f"| {m['id']} {m['title']} | {LABEL[milestone_status(m)]} "
            f"| {done}/{len(items)} | {tests} |"
        )
    for m in milestones:
        items = m.get("item", [])
        if milestone_status(m) == "planned" and not items:
            continue
        out += ["", f"### {m['id']} {m['title']}", "", "| Item | Spec | Status | Tests |", "|---|---|---|---|"]
        for i in items:
            tests = i.get("tests", [])
            ok = sum(1 for t in tests if results.get(t) == "ok")
            out.append(
                f"| {i['what']} | {', '.join(i.get('spec', []))} "
                f"| {LABEL.get(i['status'], i['status'])} | {ok}/{len(tests)} |"
            )
    untracked = sorted(set(results) - tracked)
    if untracked:
        out += ["", "### Tests not tracked yet", ""] + [f"- `{t}`" for t in untracked]
    print("\n".join(out))
    for e in errors:
        print(f"error: {e}", file=sys.stderr)
    return 1 if errors else 0


def status():
    milestones = load()
    out = [
        "# Status",
        "",
        "<!-- Generated from spec/tracking.toml by `scripts/tracking.py status`;",
        "     CI fails if it is out of date. Don't edit by hand. -->",
        "",
        "Hyperstor is built in milestones. This page lists what each one",
        "delivers and what is available today; every item marked done is",
        "covered by tests that run on every change.",
        "",
        "| Milestone | Status |",
        "|---|---|",
    ]
    for m in milestones:
        out.append(f"| {m['id']}: {m['title']} | {LABEL[milestone_status(m)]} |")
    for m in milestones:
        if not m.get("item"):
            continue
        out += ["", f"## {m['id']}: {m['title']}", "", "| Feature | Status |", "|---|---|"]
        for i in m["item"]:
            out.append(f"| {i['what']} | {LABEL[i['status']]} |")
    print("\n".join(out))
    return 0


if __name__ == "__main__":
    if len(sys.argv) >= 3 and sys.argv[1] == "check":
        sys.exit(check(sys.argv[2:]))
    if sys.argv[1:] == ["status"]:
        sys.exit(status())
    print(__doc__, file=sys.stderr)
    sys.exit(2)
