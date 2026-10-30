#!/usr/bin/env python3
"""Checks that the design in spec/ hangs together, and that code cites it
correctly (spec/README.md, "Conventions"; CLAUDE.md, "Design work").

- Every spec document is listed in spec/README.md and spec/SUMMARY.md, and
  every relative link in spec/ points to a file that exists.
- Decisions (D, F, I) and host checks (G, FG) are each defined once.
- Every decision or host check cited in spec/, crates/ or
  spec/tracking.toml is defined. Citations of glidex documents
  (`clustering.md D7`) are glidex's and not checked.
- Every section cited (`§8.1`, `08-engine.md §7.3`) exists: a bare `§N` in a
  document with its own numbering (00-filesystem.md, 18-workloads.md) may be
  local; anywhere else it is a global section (spec/README.md).
- Every `spec/<file>.md` cited in code exists.
- Spec documents are named `NN-topic.md`, numbered in the order they were
  written, each number once (README.md, SUMMARY.md: as mdBook wants
  them); no file in the repository names a document by its topic alone.

Usage: scripts/spec-check.py   (from the repository root; exits 1 on errors)
"""

import re
import sys
from pathlib import Path

ROOT = Path(__file__).resolve().parent.parent
SPEC = ROOT / "spec"

errors = []


def error(path, line, msg):
    errors.append(f"{path.relative_to(ROOT)}:{line}: {msg}")


def lines(path):
    return enumerate(path.read_text(encoding="utf-8").splitlines(), 1)


docs = sorted(p for p in SPEC.glob("*.md"))
names = {p.name for p in docs}

# --- Names: NN-topic.md ----------------------------------------------------

UNNUMBERED = {"README.md", "SUMMARY.md"}
NUMBERED = re.compile(r"(\d{2})-([a-z0-9]+(?:-[a-z0-9]+)*)\.md")
numbered = {}  # number -> file
by_topic = {}  # "topic.md" -> "NN-topic.md"
for p in docs:
    if p.name in UNNUMBERED:
        continue
    m = NUMBERED.fullmatch(p.name)
    if not m:
        error(p, 1, "not named NN-topic.md (the next free number; spec/README.md)")
        continue
    if m.group(1) in numbered:
        error(p, 1, f"number {m.group(1)} is also {numbered[m.group(1)]}")
    numbered[m.group(1)] = p.name
    by_topic[m.group(2) + ".md"] = p.name

# --- Every document is listed in README.md and SUMMARY.md ----------------

readme = (SPEC / "README.md").read_text(encoding="utf-8")
summary = (SPEC / "SUMMARY.md").read_text(encoding="utf-8")
for p in docs:
    if p.name in ("README.md", "SUMMARY.md"):
        continue
    if f"]({p.name})" not in readme:
        error(p, 1, "not listed in spec/README.md")
    if f"]({p.name})" not in summary:
        error(p, 1, "not listed in spec/SUMMARY.md")

# Documents whose sections are global: README.md's table names their
# sections (`§4, §9`); the others say "(own)" or "—".
global_docs = set()
for m in re.finditer(r"^\| \[([\w-]+\.md)\]\([\w-]+\.md\) \| ([^|]*)\|", readme, re.M):
    if "§" in m.group(2):
        global_docs.add(m.group(1))
own_docs = {"00-filesystem.md", "18-workloads.md"}

# --- Relative links ------------------------------------------------------

LINK = re.compile(r"\]\(([^)#\s]+)(#[^)]*)?\)")
for p in docs:
    for n, text in lines(p):
        for m in LINK.finditer(text):
            target = m.group(1)
            if re.match(r"[a-z]+:", target):
                continue
            if not (p.parent / target).exists():
                error(p, n, f"link to missing file {target}")

# --- Sections ------------------------------------------------------------

HEADING = re.compile(r"^#{2,4} (\d{1,2}(?:\.\d+)*)\.?\s")
sections = {}  # file -> set of section numbers
for p in docs:
    found = set()
    for _, text in lines(p):
        m = HEADING.match(text)
        if m:
            found.add(m.group(1))
    sections[p.name] = found
global_sections = {}
for name in global_docs:
    for s in sections.get(name, ()):
        if s in global_sections:
            error(SPEC / name, 1, f"§{s} is also defined in {global_sections[s]}")
        global_sections[s] = name

# --- Decisions and host checks -------------------------------------------

# Where each kind of identifier is defined, and how.
TABLE_ROW = r"^\| \*{{0,2}}({prefix}\d+)\*{{0,2}} \|"
BULLET = r"^- \*\*({prefix}\d+)[.:]"
DEFINITIONS = {
    "D": ("04-decisions.md", TABLE_ROW),
    "F": ("00-filesystem.md", TABLE_ROW),
    "I": ("01-implementation-notes.md", TABLE_ROW),
    "G": (None, BULLET),
    "FG": ("00-filesystem.md", BULLET),
}
defined = {kind: {} for kind in DEFINITIONS}
for kind, (home, shape) in DEFINITIONS.items():
    pattern = re.compile(shape.format(prefix=kind))
    for p in docs if home is None else [SPEC / home]:
        for n, text in lines(p):
            m = pattern.match(text)
            if not m:
                continue
            ident = m.group(1)
            if ident in defined[kind]:
                error(p, n, f"{ident} is already defined at {defined[kind][ident]}")
            else:
                defined[kind][ident] = f"{p.name}:{n}"

# Names of glidex documents (03-overview.md, "References to glidex"): an
# identifier right after one of them is glidex's.
GLIDEX_DOCS = (
    "storage|clustering|host-topology|networking|metering|security|tenancy"
    "|credentials|hypervisors|architecture|images|grpc"
)
CITATION = re.compile(r"(?<![\w.-])(FG|D|F|I|G)(\d+)(?![\w])")
GLIDEX_PREFIX = re.compile(rf"(?:{GLIDEX_DOCS})\.md(?:\]\([^)]*\))?\s*$")
SECTION = re.compile(r"(?:\[?([\w-]+\.md)\]?(?:\([^)]*\))?\s+)?§(\d{1,2}(?:\.\d+)*)")


def check_citations(path, text_lines, own=False, local=()):
    for n, text in text_lines:
        # Code spans in spec/ quote other things (names, flags); in Rust
        # files every comment can cite.
        for m in CITATION.finditer(text):
            kind, ident = m.group(1), m.group(1) + m.group(2)
            if GLIDEX_PREFIX.search(text[: m.start()]):
                continue
            if ident not in defined[kind]:
                error(path, n, f"{ident} is cited but not defined")
        for m in SECTION.finditer(text):
            doc, sec = m.group(1), m.group(2)
            if doc and doc not in names:
                continue  # a glidex document
            if doc:
                if sec not in sections[doc] and global_sections.get(sec) != doc:
                    error(path, n, f"{doc} has no §{sec}")
            elif not ((own and sec in local) or sec in global_sections):
                error(path, n, f"§{sec} is not a section")


def strip_code(text_lines):
    """Drops fenced blocks and inline code spans from Markdown."""
    fenced = False
    for n, text in text_lines:
        if text.lstrip().startswith("```"):
            fenced = not fenced
            continue
        if not fenced:
            yield n, re.sub(r"`[^`]*`", "", text)


for p in docs:
    if p.name == "SUMMARY.md":
        continue
    check_citations(
        p, strip_code(lines(p)), own=p.name in own_docs, local=sections[p.name]
    )

SPEC_PATH = re.compile(r"spec/([\w-]+\.md)")
for p in sorted(ROOT.glob("crates/**/*.rs")):
    comments = []
    for n, text in lines(p):
        i = text.find("//")
        if i >= 0:
            comments.append((n, text[i:]))
    check_citations(p, comments)
    for n, text in comments:
        for m in SPEC_PATH.finditer(text):
            if m.group(1) not in names:
                error(p, n, f"cites missing spec/{m.group(1)}")

# --- No document named by its topic alone ------------------------------------
# Anywhere in the repository, `topic.md` for a spec document is its old name.
# Under docs/public, which has pages of its own named like some topics, only
# `spec/topic.md` is.
if by_topic:
    alt = "|".join(re.escape(t[:-3]) for t in sorted(by_topic, key=len, reverse=True))
    bare = re.compile(rf"(?<![\w./-])({alt})\.md(?![\w-])")
    in_spec = re.compile(rf"spec/({alt})\.md(?![\w-])")
    import subprocess

    tracked = subprocess.run(
        ["git", "ls-files"], cwd=ROOT, capture_output=True, text=True, check=True
    ).stdout.split()
    for f in tracked:
        path = ROOT / f
        try:
            text_lines = list(lines(path))
        except (UnicodeDecodeError, IsADirectoryError, FileNotFoundError):
            continue
        patterns = [in_spec] if f.startswith("docs/public/") else [in_spec, bare]
        for n, text in text_lines:
            for m in (m for p in patterns for m in p.finditer(text)):
                old = m.group(1) + ".md"
                error(path, n, f"{old} is now {by_topic[old]}")

tracking = SPEC / "tracking.toml"
if tracking.exists():
    check_citations(tracking, [(n, t) for n, t in lines(tracking) if "spec =" in t])

for e in errors:
    print(e)
counts = ", ".join(f"{len(v)} {k}" for k, v in defined.items())
print(
    f"spec-check: {len(docs)} documents, {len(global_sections)} global sections, "
    f"{counts}; {len(errors)} error(s)",
    file=sys.stderr,
)
sys.exit(1 if errors else 0)
