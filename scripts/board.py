#!/usr/bin/env python3
"""The implementation progress board (spec/23-progress-board.md).

Each workstream, the work one agent or person is carrying, keeps one file
in spec/board/, so agents working in parallel never edit the same file.
The board page is made from those files and from spec/tracking.toml (how
far each milestone is built):

    scripts/board.py write    regenerates spec/23-progress-board.md
    scripts/board.py check    checks every board file, and that the page
                              is what `write` makes (CI runs it)

A board file, spec/board/<workstream>.toml:

    title = "Returning nodes without flapping"   # required, one line
    status = "active"        # required: active | blocked | review | done
    owner = "Claude Code session https://claude.ai/code/session_..."
                             # required: who carries it now
    updated = 2026-10-30     # required: when this file last changed
    started = 2026-10-29     # optional
    milestone = "HS2"        # optional: a milestone id in tracking.toml
    branch = "fix/rejoin-flaps"           # optional
    prs = [72, 73, "glidex#203"]          # optional: hyperstor PR
                                          # numbers, or "glidex#N"
    now = "..."              # required unless done: what is under way
    next = "..."             # optional: the step after
    blocked_on = "..."       # required when blocked: on what or whom

The page shows the dates as written, never compared with today, so it
stays the same until a board file changes.
"""

import datetime
import re
import sys
import tomllib
from pathlib import Path

ROOT = Path(__file__).resolve().parent.parent
BOARD = ROOT / "spec" / "board"
PAGE = ROOT / "spec" / "23-progress-board.md"
TRACKING = ROOT / "spec" / "tracking.toml"
REPOS = {"": "CMKL-APEX/hyperstor", "glidex": "CMKL-APEX/glidex"}

STATUSES = ["active", "blocked", "review", "done"]
HEADINGS = {
    "active": "In progress",
    "blocked": "Blocked",
    "review": "In review",
    "done": "Done",
}
REQUIRED = {"title": str, "status": str, "owner": str, "updated": datetime.date}
OPTIONAL = {
    "started": datetime.date,
    "milestone": str,
    "branch": str,
    "prs": list,
    "now": str,
    "next": str,
    "blocked_on": str,
}
NAME = re.compile(r"[a-z0-9]+(?:-[a-z0-9]+)*\.toml")
PR = re.compile(r"(?:(glidex)#)?(\d+)")


def milestones():
    with open(TRACKING, "rb") as f:
        return tomllib.load(f)["milestone"]


def load(errors):
    """Every board file, as (name, fields), with what's wrong in `errors`."""
    ids = {m["id"] for m in milestones()}
    entries = []
    for path in sorted(BOARD.glob("*")):
        where = f"spec/board/{path.name}"
        if not NAME.fullmatch(path.name):
            errors.append(f"{where}: name it <workstream>.toml (lower case, dashes)")
            continue
        try:
            with open(path, "rb") as f:
                e = tomllib.load(f)
        except tomllib.TOMLDecodeError as err:
            errors.append(f"{where}: {err}")
            continue
        for key, kind in {**REQUIRED, **OPTIONAL}.items():
            if key in e and not isinstance(e[key], kind):
                errors.append(f"{where}: {key} must be a {kind.__name__}")
        for key in REQUIRED:
            if key not in e:
                errors.append(f"{where}: {key} is required")
        for key in e:
            if key not in REQUIRED and key not in OPTIONAL:
                errors.append(f"{where}: unknown key {key}")
        status = e.get("status")
        if status not in STATUSES:
            errors.append(f"{where}: status is one of {', '.join(STATUSES)}")
        if status != "done" and not e.get("now"):
            errors.append(f"{where}: say what is under way (now)")
        if status == "blocked" and not e.get("blocked_on"):
            errors.append(f"{where}: say what it is blocked on (blocked_on)")
        if status != "blocked" and "blocked_on" in e:
            errors.append(f"{where}: blocked_on only while blocked")
        if e.get("milestone") is not None and e["milestone"] not in ids:
            errors.append(f"{where}: no milestone {e['milestone']} in tracking.toml")
        for p in e.get("prs", []):
            if not PR.fullmatch(str(p)):
                errors.append(f'{where}: a PR is a number or "glidex#N", not {p!r}')
        for key in ("title", "now", "next", "blocked_on"):
            if "\n" in str(e.get(key, "")):
                errors.append(f"{where}: {key} is one line")
        s, u = e.get("started"), e.get("updated")
        if isinstance(s, datetime.date) and isinstance(u, datetime.date) and s > u:
            errors.append(f"{where}: started after updated")
        entries.append((path.stem, e))
    return entries


def cell(text):
    return str(text).replace("|", "\\|")


def links(e):
    out = []
    if e.get("branch"):
        out.append(f"`{e['branch']}`")
    for p in e.get("prs", []):
        m = PR.fullmatch(str(p))
        if m:
            repo, n = m.group(1) or "", m.group(2)
            label = f"{repo}#{n}" if repo else f"#{n}"
            out.append(f"[{label}](https://github.com/{REPOS[repo]}/pull/{n})")
    return ", ".join(out) or "—"


def render(entries):
    by = {s: [] for s in STATUSES}
    for name, e in entries:
        if e.get("status") in by:
            by[e["status"]].append((name, e))
    out = [
        "# Progress board",
        "",
        "<!-- Generated by scripts/board.py from spec/board/*.toml and",
        "     spec/tracking.toml. Edit those, then run `scripts/board.py write`. -->",
        "",
        "Who is carrying what, and how far each milestone is built. Each",
        "workstream keeps one file in [board/](board/), so agents working in",
        "parallel never edit the same file; CLAUDE.md (\"Working on the host\")",
        "says when to update it. Dates are as last written: an entry not",
        "updated for a while may have been left behind.",
        "",
        "| " + " | ".join(HEADINGS[s] for s in STATUSES) + " |",
        "|" + "---|" * len(STATUSES),
        "| " + " | ".join(str(len(by[s])) for s in STATUSES) + " |",
    ]
    for s in STATUSES:
        rows = sorted(by[s], key=lambda x: (str(x[1].get("updated")), x[0]), reverse=True)
        out += ["", f"## {HEADINGS[s]}", ""]
        if not rows:
            out.append("None.")
            continue
        if s == "done":
            out += [
                "| Workstream | Owner | Milestone | Branch and PRs | Updated |",
                "|---|---|---|---|---|",
            ]
        else:
            last = "Blocked on" if s == "blocked" else "Next"
            out += [
                f"| Workstream | Owner | Milestone | Branch and PRs | Now | {last} | Updated |",
                "|---|---|---|---|---|---|---|",
            ]
        for name, e in rows:
            row = [
                f"**{cell(e['title'])}** (`{name}`)",
                cell(e["owner"]),
                cell(e.get("milestone", "—")),
                links(e),
            ]
            if s != "done":
                row.append(cell(e.get("now", "")))
                row.append(cell(e.get("blocked_on" if s == "blocked" else "next", "—")))
            row.append(str(e["updated"]))
            out.append("| " + " | ".join(row) + " |")
    out += [
        "",
        "## Milestones",
        "",
        "Items by status in [tracking.toml](tracking.toml), which CI checks",
        "against the tests.",
        "",
        "| Milestone | Title | Done | Partial | Planned |",
        "|---|---|---|---|---|",
    ]
    for m in milestones():
        items = [i["status"] for i in m.get("item", [])]
        counts = [str(items.count(k)) for k in ("done", "partial", "planned")]
        out.append(f"| {m['id']} | {cell(m['title'])} | " + " | ".join(counts) + " |")
    return "\n".join(out) + "\n"


def main():
    cmd = sys.argv[1] if len(sys.argv) == 2 else ""
    if cmd not in ("write", "check"):
        print("usage: scripts/board.py write|check", file=sys.stderr)
        return 2
    errors = []
    entries = load(errors)
    page = render(entries)
    if cmd == "write":
        PAGE.write_text(page, encoding="utf-8")
    elif not PAGE.exists() or PAGE.read_text(encoding="utf-8") != page:
        errors.append(f"{PAGE.relative_to(ROOT)} is out of date: run scripts/board.py write")
    for e in errors:
        print(e)
    print(f"board: {len(entries)} workstream(s); {len(errors)} error(s)", file=sys.stderr)
    return 1 if errors else 0


if __name__ == "__main__":
    sys.exit(main())
