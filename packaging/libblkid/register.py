"""Adds hyperstor.c to a util-linux source tree (2.39 and later): copies
the prober and names it in libblkid's list of superblocks and in both
build systems. Run from scripts/build-libblkid-deb.sh as
`python3 register.py SRCDIR`; fails if a file isn't as expected."""

import pathlib
import shutil
import sys

src = pathlib.Path(sys.argv[1])
here = pathlib.Path(__file__).parent
sb = src / "libblkid/src/superblocks"
shutil.copy(here / "hyperstor.c", sb / "hyperstor.c")


def add(path, after, line):
    """Adds `line` after the first line containing `after`."""
    lines = path.read_text().split("\n")
    for i, l in enumerate(lines):
        if after in l:
            lines.insert(i + 1, line)
            path.write_text("\n".join(lines))
            return
    sys.exit(f"{path}: no line with {after!r}")


add(sb / "superblocks.h", "extern const struct blkid_idinfo hfs_idinfo;",
    "extern const struct blkid_idinfo hyperstor_idinfo;")
add(sb / "superblocks.c", "&hfs_idinfo,", "\t&hyperstor_idinfo,")
add(src / "libblkid/src/Makemodule.am", "libblkid/src/superblocks/hfs.c",
    "\tlibblkid/src/superblocks/hyperstor.c \\")
# Meson lists the file relative to libblkid/src, quoted; Debian builds
# with autotools, so a tree whose meson.build differs is left alone there.
meson = src / "meson.build"
if meson.exists():
    text = meson.read_text()
    for quote in ("'", '"'):
        old = f"{quote}superblocks/hfs.c{quote},"
        if old in text:
            meson.write_text(text.replace(
                old, f"{old}\n  {quote}superblocks/hyperstor.c{quote},", 1))
            break
