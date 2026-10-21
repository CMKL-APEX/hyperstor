#!/usr/bin/env python3
"""Checks that every relative link in a built site points to a file in it.

mdBook doesn't check links between pages, or from the book to the API
reference next to it; this runs over the assembled site.

Usage: scripts/site-link-check.py SITE_DIR
"""

import re
import sys
from pathlib import Path
from urllib.parse import unquote, urlsplit

site = Path(sys.argv[1]).resolve()
# rustdoc's own pages are generated with their links; check what we wrote.
skip = {site / "api"}
HREF = re.compile(r"""(?:href|src)=["']([^"']+)["']""")
errors = []
pages = 0
for page in sorted(site.rglob("*.html")):
    if any(s in page.parents for s in skip):
        continue
    pages += 1
    for link in HREF.findall(page.read_text(encoding="utf-8", errors="replace")):
        url = urlsplit(link)
        if url.scheme or url.netloc or link.startswith("#") or not url.path:
            continue
        if url.path.startswith("/"):
            continue  # site-url relative; the book's own 404 page uses these
        target = (page.parent / unquote(url.path)).resolve()
        if target.is_dir():
            target = target / "index.html"
        if not target.exists():
            errors.append(f"{page.relative_to(site)}: broken link {link}")

for e in errors:
    print(e)
print(f"site-link-check: {pages} pages, {len(errors)} broken link(s)", file=sys.stderr)
sys.exit(1 if errors else 0)
