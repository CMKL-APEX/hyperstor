# NFS and pNFS

Status: **investigation; nothing built.** D127 (our own Rust server),
D129 (data servers inside `hyperstord`) and D130 (stripe groups) are
settled with the owner (decision-log.md point 74). D128 and D131–D137 are
proposals open for review, and G25–G33 are the host checks they rest
on. It extends the filesystem
([filesystem.md](filesystem.md)) with a third way in, beside virtio-fs
and FUSE (F8): unmodified NFS clients.

## 26. NFS and pNFS front end (D127–D137)

### 26.1 Goal and scope

Unmodified NFS clients mount a hyperstor filesystem and, where the client
supports parallel NFS (pNFS), read and write file data straight from the
storage nodes that hold it, in parallel, with no gateway on the data
path. Clients without pNFS still mount it and get plain NFSv4.1 through
the metadata server.

- **Why:** the FUSE client (F8) needs software on every host. Many hosts
  can't or won't take it: appliances, other teams' clusters, HPC nodes
  with a locked image, hypervisors. The Linux NFS client is already on
  all of them, and with pNFS it does the striping itself.
- **In:**
  - NFSv4.1 (RFC 8881) and NFSv4.2 (RFC 7862) over TCP;
  - pNFS with the files layout (RFC 8881 ch. 13; D128), data servers in
    the storage nodes (D129);
  - I/O through the metadata server for clients without pNFS (RFC 8434
    section 3.2 requires it anyway);
  - AUTH_SYS, protected by RPC-with-TLS (RFC 9289) with hyperstor host
    certificates (D135);
  - NFSv4.2's SEEK, READ_PLUS, ALLOCATE, DEALLOCATE, COPY and CLONE
    (§26.8);
  - several metadata servers, with failover and no grace period (D134).
- **Later:**
  - read and write delegations;
  - NFS over RDMA (RFC 8166, RFC 8267), with HS6;
  - Kerberos (RPCSEC_GSS);
  - the flexible files layout (RFC 8435), if a client needs it (D128);
  - NFSv3, for Windows and macOS clients (D136).
- **Out:**
  - NFSv4.0;
  - the block (RFC 5663) and SCSI or NVMe (RFC 8154, RFC 9561) layouts;
  - the object layout (RFC 5664), which no current client implements.

### 26.2 How pNFS works, in short

A pNFS server is split in two:

- The **metadata server (MDS)** is a full NFSv4.1 server. It owns the
  namespace, attributes, open, lock and delegation state, and the
  **layouts**: maps from byte ranges of a file to data servers.
- **Data servers (DSs)** serve `READ`, `WRITE` and `COMMIT` on the file
  handles a layout names. How the MDS talks to them (the "control
  protocol") is up to the implementation (RFC 8434).

A client opens a file on the MDS, asks for a layout (`LAYOUTGET`, for a
byte range and an iomode, READ or RW), resolves the layout's device IDs
to addresses (`GETDEVICEINFO`), then does I/O with the DSs directly. It
reports the new size and mtime with `LAYOUTCOMMIT`, and gives the layout
back with `LAYOUTRETURN`. The MDS can recall a layout
(`CB_LAYOUTRECALL`) and must be able to fence a client from the DSs when
it revokes one (RFC 8434, sections 3.1 and 6). A client that can't use a DS sends
the I/O to the MDS instead, which must serve it (RFC 8434, section 3.2).

### 26.3 Clients

| Client | What it gets | Source |
|---|---|---|
| Linux | NFSv4.2 with pNFS: files layout (`nfs_layout_nfsv41_files`), flexible files (`nfs_layout_flexfiles`, striped only from Linux 6.18), block and SCSI (NVMe from 6.11); RPC-with-TLS from 6.5 with `tlshd`; NFS over RDMA | kernel sources and merge logs (see §26.11) |
| FreeBSD | pNFS client for files and flexible files (mount option `pnfs`) | pnfs(4) |
| VMware ESXi | NFSv4.1 with session trunking, no pNFS: all I/O through the MDS | vSphere storage documentation, 6.0–7.0 |
| macOS | NFSv4.0 at most, so nothing from this design (NFSv3 is D136) | to check (G31) |
| Windows | NFSv2 and NFSv3 client only | Microsoft NFS overview |

The files layout and its striping are in every Linux kernel that glidex
and common distributions ship. Striped flexible-files layouts need 6.18,
newer than most distributions in 2026 (RHEL 9: 5.14; Ubuntu 24.04: 6.8;
RHEL 10: 6.12). That decides D128.

### 26.4 Architecture

```text
 NFS clients ── NFSv4.1/4.2 (2049, TLS) ──► hyperstor-nfs (MDS) × N ──► hyperstor-kv (namespace, NFS state)
      │                                           │  I/O for clients without pNFS
      │                                           ▼
      └── READ/WRITE on layouts (TLS) ────► NFS data servers inside hyperstord
                                            (one per reactor, the PG primaries)
```

| Component | Runs | Does |
|---|---|---|
| `hyperstor-nfs` | on map service or gateway hosts, several instances | The MDS: an NFSv4.1/4.2 server over the filesystem's metadata (filesystem.md F1–F3), keeping NFS state in `hyperstor-kv` (D134). Serves I/O itself, through the client library, for clients without pNFS |
| NFS DS | inside `hyperstord`, on each reactor | Serves `READ`, `WRITE`, `COMMIT` and `GETATTR` on the chunks of the PGs that reactor leads (D129) |
| `hyperstor-kv` | storage nodes (HS11) | Inodes, directories, and the MDS's client, open, lock and layout records |

**The MDS is fsmeta with an NFS face.** F1's metadata servers are
stateless clients of `hyperstor-kv`. `hyperstor-nfs` links the same
metadata code rather than calling a separate fsmeta process, so an NFS
`LOOKUP` or `CREATE` is one kv transaction, not two hops.

### 26.5 Placing file data for layouts (D130)

A layout has to say which DS holds each byte of a file, in a few bytes,
for the whole file. F4 places each chunk by its own hash
(`pg = hash(inode, chunk) mod pg_count`), which has no pattern a layout
can express: describing it takes one layout segment per 4 MiB, or a
device as long as the file. Both put a request on the MDS for every
chunk, the "MDS bottleneck" pNFS exists to avoid.

**Stripe groups (D130, settled; amends F4).** A file is cut into
**segments** of `C` chunks (64 GiB at 4 MiB chunks). Segment `s` picks a
base PG, `b = hash(fs, inode, s) mod pg_count`, and chunk `i` of the file
goes to PG

```text
pg(i) = (b + (i mod W)) mod pg_count        with s = i / C
```

so the segment is striped round-robin over `W` consecutive PGs.

- **One layout segment per 64 GiB** names `W` data servers and a 4 MiB
  stripe unit: one `LAYOUTGET` for a file of any size up to 64 GiB.
- **Balance comes from the bases:** each segment of each file starts at
  a random PG, so load and space still spread over every PG. A large file
  spreads too: each 64 GiB has its own base.
- **`W` is a directory policy** stored in the inode at create, like the
  pool and chunk size (F4). The proposed default is 16; up to 4,096
  (`NFS4_PNFS_MAX_STRIPE_CNT` in Linux's files layout driver).
- **Every client path uses it**, not only NFS: FUSE, virtio-fs and GPU
  clients compute the same `pg(i)`, so a file has one placement.

| Option | For | Against |
|---|---|---|
| **Stripe groups** (chosen) | One layout per 64 GiB; the files layout's repeating stripe pattern expresses it exactly | A file's segment uses only `W` PGs: one hot file spreads over `W` primaries, not all |
| Keep F4's hash per chunk, one layout segment per chunk | Placement unchanged | One `LAYOUTGET` per 4 MiB; Linux's handling of thousands of small segments per file is unknown (G25) |
| Keep F4, a device per file listing each chunk's primary | One `LAYOUTGET` | A device per file; 4,096 entries cover only 16 GiB; every primary change invalidates the devices of every file it touches |
| Keep F4, every DS forwards to the right primary | Any layout works | A second hop for nearly every byte: the gateway pNFS removes |

### 26.6 Layouts and data servers (D128, D129, D131, D132)

**Files layout, sparse, tightly coupled (D128).** A segment's layout:

| Field | Value |
|---|---|
| `nfl_util` | the stripe unit, the chunk size (4 MiB); `NFL4_UFLG_DENSE` off (sparse: the DS offset is the file offset); `NFL4_UFLG_COMMIT_THRU_MDS` off |
| `nfl_first_stripe_index` | `(s · C) mod W`, so stripe 0 of the segment is the segment's first chunk |
| `nfl_pattern_offset` | the segment's start |
| `nfl_fh_list` | one file handle, the file's (§26.7) |
| device | `W` stripe indices, one per PG of the group, each naming the DS of that PG's primary |

**Device IDs name PG groups at a map epoch (D131).** A device ID
encodes `(pool, b, W, map epoch)`. `GETDEVICEINFO` answers with the
addresses of the `W` primaries at that epoch. When a primary changes:

1. The old primary, if it is alive, is no longer the primary. Its DS
   forwards the request to the new primary over the cluster's own path,
   so the client sees nothing worse than a slower request.
2. If the old primary is gone, the client's I/O fails. It reports the
   error (`LAYOUTERROR`, or `LAYOUTRETURN` with the error), asks for a
   new layout, and gets device IDs at the new epoch. Until then it sends
   I/O through the MDS. How quickly Linux does this is G27.
3. The MDS may also recall layouts that name a moved PG, or send
   `CB_NOTIFY_DEVICEID`. Both are optimisations; the forwarding in step 1
   keeps every layout correct.

**DSs live in `hyperstord` (D129).** Each reactor serves NFS on a port
of its own, like its data port (D39), and its DS answers only for the
PGs it leads. A pNFS write then lands on the primary's own reactor,
which replicates it as any client write (replication.md §8.1): one
network hop, as for the native client.

| Option | For | Against |
|---|---|---|
| **DS per reactor in `hyperstord`** (chosen) | No extra hop or copy; the primary checks its map lease and fencing as for any request | NFS parsing in the storage daemon; more connections per client (one per reactor it uses) |
| A DS process per storage host, forwarding to the local reactors | Keeps NFS out of `hyperstord`; can be restarted alone | A local hop and a copy for every byte |
| DSs on gateway hosts | No change to storage nodes | Two hops for every byte; the gateway tier becomes the bottleneck |

**`FILE_SYNC` after every copy (D132).** A primary answers a write only
when every copy has it on stable storage. So a DS answers every `WRITE`
with `FILE_SYNC`, even when the client asked for `UNSTABLE`, and the
client never needs a `COMMIT`. The write verifier is the DS's boot
instance; `COMMIT`, if a client sends one, is a no-op.

**Size and mtime through `LAYOUTCOMMIT`.** DS writes don't touch the
inode. The client's `LAYOUTCOMMIT` (at close, `fsync`, or when the layout
is returned) carries the last byte written and the mtime, and the MDS
commits them to the inode in one kv transaction. This is F5's "size and
mtime at close or `fsync`", reached through the protocol instead of the
FUSE client. A client that dies before `LAYOUTCOMMIT` leaves data past
the recorded size, as a crash would with F5; the MDS recovers the true
size from the chunks when it fences the client.

**Partial blocks.** NFS writes need not be 4 KiB aligned; the engine is
(implementation-notes.md I8). The DS reads, merges and writes back a
block it covers in part, holding the block's key on the primary for the
duration, as `hyperstor-vhost` does for 512-byte sectors (D126).

### 26.7 File handles, state and failover (D133, D134)

**File handles** are opaque to clients and up to 128 bytes. Hyperstor's
is `(format, fs, inode, generation)`, about 24 bytes. The generation
changes when an inode number is reused, so an old handle gets
`NFS4ERR_STALE`. F2's inode record needs that generation.

**NFS state lives in kv (D134).** The MDS keeps no state that only it
knows:

| Record | Contents | Written |
|---|---|---|
| client | client owner, verifier, principal, lease renewed at | at `EXCHANGE_ID`; renewals coarsely, at most every lease/3 |
| open | stateid, share access and deny, open owner | in the `OPEN` transaction |
| lock | byte ranges, lock owner | the same records as F7's locks, so NFS and FUSE clients see each other's locks |
| layout | stateid, file, ranges, iomode, client | at `LAYOUTGET`, `LAYOUTRETURN` |
| cached reply | the reply to a non-idempotent request (`sa_cachethis`) | in the same transaction as the change it reports |

**Failover without a grace period.** A client reaches one service
address per export, held by one MDS instance at a time through a lease
in kv; standbys take the address over when that lease runs out. The new
instance finds every client, open, lock and layout in kv, so clients keep
their client IDs and stateids. Sessions stay in memory. A client that
retries on the new instance either reconnects to its session (G29) or
creates a new one under the same client ID, and nothing has to be
reclaimed. Slot sequence numbers are in memory too. A slot's first
request after a takeover is checked against the cached replies in kv, so
a retried `CREATE` or `RENAME` gets its first answer rather than being
done twice.

| Option | For | Against |
|---|---|---|
| **All state in kv** (proposed) | Failover in seconds, no grace period, no reclaim storms; any instance can recall or fence | A kv write in every `OPEN`, `LOCK` and `LAYOUTGET` |
| State in memory, a cluster-wide grace period after failover (NFS-Ganesha's clustered recovery) | Cheaper opens | 90 s or more of refused opens after every MDS failure; stable storage is needed anyway for which clients may reclaim (RFC 8881 section 8.4.3) |

**Scaling out.** Exports, not requests, are spread over MDS instances:
each export's address is held by one instance. A filesystem with many
clients can be exported under several addresses. NFSv4.1 trunking across
instances (the same `server_owner` from all of them) would let one
client use several MDS instances for one export; whether Linux does that
is G30.

**Fencing (D133).** RFC 8434 requires the MDS to stop a client's DS I/O
when it revokes a layout:

- **Layout stateids are sealed by the MDS:** the 12-byte `other` field
  carries the client, file, iomode and a MAC under a cluster key that
  every `hyperstord` holds. A DS checks a stateid without asking anyone,
  and refuses a READ-iomode stateid on a `WRITE`.
- **Revoking a client fences it everywhere:** the MDS puts the NFS
  client's instance on the cluster blocklist (D24, D76), and every DS
  refuses its stateids from then on. This happens when a client's lease
  expires or it doesn't return a recalled layout in time.
- **No per-file fence:** a DS can't revoke one layout of a client that is
  otherwise healthy without a lookup per request. RFC 8434 allows fencing
  the whole client, and a recall timeout is rare. Per-file fencing can
  come later, for example as a fence generation in the stateid checked
  against the inode.
- How the cluster key reaches storage nodes, and how it is rotated, needs
  a security review before it is built.

### 26.8 NFSv4.2 operations

| Operation | Hyperstor |
|---|---|
| `SEEK`, `READ_PLUS` | Holes are chunks that don't exist and blocks no chunk maps; the DS asks the engine for a chunk's extents |
| `ALLOCATE` | Reserves space in the tenant's quota (tenants.md §24.4); blocks aren't written |
| `DEALLOCATE` | Deletes whole chunks and zeroes partial ones (`Zero`) |
| `CLONE` | Needs files that share chunk versions, as volume clones do (volumes.md §10.3). With a lineage per file it is the same mechanism; until then, refused (`NFS4ERR_NOTSUPP`) |
| `COPY` | Server-side: the MDS has the primaries copy chunk by chunk, so data never reaches the client; asynchronous for large files (`CB_OFFLOAD`) |
| `LAYOUTERROR`, `LAYOUTSTATS` | DS errors and latencies, kept as metrics and used to recall stale layouts |

### 26.9 Security and identity (D135)

- **Transport:** RPC-with-TLS (RFC 9289) with mutual TLS. A client host
  presents a hyperstor host certificate (administration.md §23.8),
  which says which tenant and exports it may mount, as F12 does for FUSE
  hosts. Linux has it from 6.5 with `tlshd` (`mount -o xprtsec=mtls`).
  Whether the Linux client also uses TLS for DS connections is G28; until
  that is known, pNFS without TLS is allowed only on trusted networks
  (D25).
- **Users:** AUTH_SYS user and group ids are trusted only from an
  authenticated host, with root squashed by default. Owners travel as
  numeric ids; `user@domain` mapping is configured per export.
- **Kerberos** (RPCSEC_GSS) comes later. With tightly coupled DSs it
  would need keys on every storage node.
- **Ports:** the MDS on 2049 on the client network; a DS port per
  reactor on storage networks (protocol.md §13.2). Neither carries
  administration.
- **ACLs:** mode bits first; NFSv4 ACLs with POSIX ACLs (filesystem.md
  §1, later).

### 26.10 Alternatives considered

| Option | For | Against |
|---|---|---|
| **Own Rust server, `hyperstor-nfs`** (D127, chosen) | One language, the reactor, state in kv, DSs in `hyperstord`; nothing external in the data path | The largest piece of work: XDR from the RFCs' `.x` files, the COMPOUND engine, sessions, state. No mature Rust NFSv4.1 server exists to start from (`nfsserve` and its forks are NFSv3 only) |
| NFS-Ganesha with a hyperstor FSAL | A mature NFSv4.1 state machine and clustered recovery | C, through FFI to the Rust client library; its pNFS code is unmaintained (Ganesha wiki, December 2025); state in its own recovery backends, not kv |
| The kernel's `nfsd` re-exporting a FUSE mount | Nothing to write; a stopgap like ublk plus `nvmet` (D19) | No pNFS (`nfsd` lays out only block, SCSI and a test-only flexfiles); every byte through one host and FUSE; file handles across FUSE restarts |
| Flexible files layout instead of files (D128) | Error and statistics reporting built in; NFSv3 DSs possible | Striping only from Linux 6.18; its client-side mirroring is not wanted (copies are made by the primary); `FF_FLAGS_WRITE_ONE_MIRROR` isn't in Linux |
| SCSI or NVMe layout | Clients write blocks directly | Clients must reach raw volumes over NVMe-oF with reservations, and the MDS must allocate blocks: a block filesystem, not hyperstor's object layer |

**Phasing (proposed, D137):**

| Step | Delivers | Depends on |
|---|---|---|
| N0 | `hyperstor-nfs` as an NFSv4.1 server doing all I/O itself: every v4.1 client works | HS11 (kv), the metadata part of HS14 |
| N1 | Stripe groups (D130), files layouts and DSs in `hyperstord` | N0 |
| N2 | NFS state in kv and failover between instances | N0 |
| N3 | NFSv4.2 operations, delegations | N1 |
| N4 | NFS over RDMA, Kerberos | N1, HS6 |

### 26.11 Host checks

- **G25. Many small layout segments on Linux** (D130). Against a test
  MDS, give a Linux client one files-layout segment per 4 MiB chunk on a
  large file, on 5.14, 6.8, 6.12 and 6.18: `LAYOUTGET` rate, throughput
  and memory, against one 64 GiB segment with stripe groups.
- **G26. Layout type choice** (D128). With the server offering files and
  flexible files in `fs_layout_type`, which one Linux and FreeBSD clients
  use, and whether the order matters.
- **G27. A data server fails** (D131). Kill a DS's node under a pNFS
  writer: how long until the client reports the error, asks for a new
  layout, uses the new devices, or falls back to the MDS; and that it
  loses no acknowledged write.
- **G28. TLS and RDMA to data servers** (D135). Whether Linux's
  `xprtsec=mtls` applies to DS connections, and whether a client mounted
  over RDMA also reaches DSs over RDMA.
- **G29. MDS failover** (D134). Move an export's address between MDS
  instances under load, with and without `CREATE_SESSION4_FLAG_PERSIST`:
  that Linux clients resume without reclaim, keep their stateids, and get
  the cached answer for a retried non-idempotent request.
- **G30. Trunking across MDS instances** (D134). Whether Linux uses
  several MDS addresses with the same `server_owner` for one mount.
- **G31. Clients without pNFS** (D136). What ESXi 8 and macOS 15
  negotiate, and that ESXi works through the MDS alone.
- **G32. FILE_SYNC and LAYOUTCOMMIT** (D132). That with every DS write
  answered `FILE_SYNC` the Linux client sends no `COMMIT`, and its
  `LAYOUTCOMMIT` reports the right size after appends and holes.
- **G33. Recall storms** (D131). Move many PG primaries at once (a node
  failure): how Linux clients cope with forwarded requests, failed DS
  I/O and recalls, and how many `LAYOUTGET`s reach the MDS.

### 26.12 References

- RFC 8881, *Network File System (NFS) Version 4 Minor Version 1
  Protocol* (2020; obsoletes RFC 5661): ch. 12 (pNFS), ch. 13 (files
  layout), section 2.10 (sessions), section 8.4 (reclaim).
- RFC 7862, *NFS Version 4 Minor Version 2 Protocol* (2016).
- RFC 8434, *Requirements for Parallel NFS (pNFS) Layout Types* (2018).
- RFC 8435, *Parallel NFS (pNFS) Flexible File Layout* (2018);
  draft-haynes-nfsv4-flexfiles-v2 (-08, August 2026, individual draft).
- RFC 5663 (block layout), RFC 8154 (SCSI layout), RFC 9561 (SCSI layout
  over NVMe), RFC 5664 (object layout).
- RFC 8166 and RFC 8267 (RPC and NFS over RDMA); RFC 9289 (RPC-with-TLS).
- Linux: `fs/nfs/filelayout/filelayout.h` (`NFS4_PNFS_MAX_STRIPE_CNT`
  4096, `NFS4_PNFS_MAX_MULTI_CNT` 256) and
  `fs/nfs/flexfilelayout/flexfilelayout.h`; the 6.11 NFS client merge
  (NVMe block layout) and the 6.18 merge (striped flexible files);
  RPC-with-TLS in the 6.4 server and 6.5 client.
- FreeBSD pnfs(4); NFS-Ganesha wiki (pNFS status); VMware vSphere storage
  documentation (NFS 4.1); Microsoft Windows Server NFS overview.
