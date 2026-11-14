# libblkid's Hyperstor prober

`hyperstor.c` teaches util-linux's libblkid Hyperstor's device superblock,
so `lsblk`, `blkid`, `wipefs`, `fdisk` and `mkfs` see a Hyperstor device
instead of a blank disk (D332–D334, I403). It is meant for util-linux;
until Ubuntu ships it, `scripts/build-libblkid-deb.sh` builds Ubuntu's
own libblkid with it, and `hyperstor-install --libblkid-deb` installs
that build. `register.py` adds the file to a util-linux source tree.

**License:** `hyperstor.c` is LGPL-2.1-or-later, as libblkid is, so it
can be part of it. The rest of this repository is MIT OR Apache-2.0.
