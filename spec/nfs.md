# NFS and pNFS

Status: **investigation; nothing built.** D127 (our own Rust server),
D129 (data servers inside `hyperstord`) and D130 (stripe groups) are
settled with the owner (decision-log.md point 74), as are D138–D141 and
the revisions of D128, D131–D134 and D137 (points 75–82, after a review
against RFC 8881, RFC 8434, RFC 8435 and the Linux client). D128 and
D131–D137 as written are open for review, and G25–G36 are the host
checks they rest on. It extends the filesystem
([filesystem.md](filesystem.md)) with a third way in, beside virtio-fs
and FUSE (F8): unmodified NFS clients.

## 26. NFS and pNFS front end (D127–D141)

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
  - pNFS with the files layout (RFC 8881 ch. 13) and the flexible files
    layout (RFC 8435), both tightly coupled, over one layout model
    (D128), with data servers in the storage nodes (D129);
  - I/O through the metadata server for clients without pNFS (RFC 8434
    section 3.1 requires it anyway);
  - AUTH_SYS, protected by RPC-with-TLS (RFC 9289) with hyperstor host
    certificates (D135);
  - NFSv4.2's SEEK, READ_PLUS, ALLOCATE, DEALLOCATE, COPY and CLONE
    (§26.8);
  - exports as cluster objects (D141);
  - several metadata servers, with failover and no grace period (D134,
    D138).
- **Later:**
  - read and write delegations;
  - NFS over RDMA (RFC 8166, RFC 8267), with HS6;
  - Kerberos (RPCSEC_GSS);
  - per-file fencing at the data servers (D139);
  - NFSv3, for Windows and macOS clients (D136).
- **Out:**
  - NFSv4.0;
  - the block (RFC 5663) and SCSI or NVMe (RFC 8154, RFC 9561) layouts;
  - the object layout (RFC 5664), which no current client implements;
  - loosely coupled data servers (RFC 8435 section 2.2).

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
it revokes one (RFC 8434, sections 3.1 and 6). A client that can't use a
DS may send the I/O to the MDS instead, which must serve it (RFC 8434,
section 3.1).

The I/O a client sends to a DS carries a stateid, and it is **never the
layout stateid**: it is the file's open, lock or delegation stateid,
with seqid 0 (RFC 8881 section 13.9.1). A DS accepts only the NFSv4.1
housekeeping operations (`EXCHANGE_ID`, `CREATE_SESSION`, `SEQUENCE`,
`BIND_CONN_TO_SESSION`, `DESTROY_*`, `SECINFO_NO_NAME`, `SET_SSV`,
`BACKCHANNEL_CTL`) and `PUTFH`, `READ`, `WRITE` and `COMMIT` (section
13.6). Both rules shape §26.6 and §26.7.

### 26.3 Clients

| Client | What it gets | Source |
|---|---|---|
| Linux | NFSv4.2 with pNFS: files layout (`nfs_layout_nfsv41_files`), flexible files (`nfs_layout_flexfiles`; striped from Linux 6.18, commits d442670…20b1d75 of 2025-09-26), block and SCSI (NVMe from 6.11); RPC-with-TLS from 6.5 with `tlshd`; NFS over RDMA. Given several layout types it prefers flexible files over files (`ld_prefs` in `fs/nfs/pnfs.c`), with no mount option to change that | kernel sources and merge logs (see §26.12) |
| FreeBSD | pNFS client for files and flexible files (mount option `pnfs`) | pnfs(4) |
| VMware ESXi | NFSv4.1 with session trunking, no pNFS: all I/O through the MDS | vSphere storage documentation, 6.0–7.0 |
| macOS | NFSv4.0 at most, so nothing from this design (NFSv3 is D136) | to check (G31) |
| Windows | NFSv2 and NFSv3 client only | Microsoft NFS overview |

The files layout and its striping are in every Linux kernel that glidex
and common distributions ship. Striped flexible-files layouts need 6.18,
newer than most distributions in 2026 (RHEL 9: 5.14; Ubuntu 24.04: 6.8;
RHEL 10: 6.12); an older kernel rejects a flexible-files mirror with
more than one data server and falls back to I/O through the MDS. That
decides which layout an export offers by default (D128).

### 26.4 Architecture

```text
 NFS clients ── NFSv4.1/4.2 (2049, TLS) ──► hyperstor-nfs (MDS) × N ──► hyperstor-kv (namespace, NFS state, exports)
      │                                           │  I/O for clients without pNFS
      │                                           ▼
      └── READ/WRITE on layouts (TLS) ────► NFS data servers inside hyperstord
                                            (one per reactor, the PG primaries)
```

| Component | Runs | Does |
|---|---|---|
| `hyperstor-nfs` | on map service or gateway hosts, several instances | The MDS: an NFSv4.1/4.2 server over the filesystem's metadata (filesystem.md F1–F3), keeping NFS state in `hyperstor-kv` (D134). Holds the addresses of the exports it serves (D138). Serves I/O itself, through the client library, for clients without pNFS |
| NFS DS | inside `hyperstord`, on each reactor | Serves `PUTFH`, `READ`, `WRITE` and `COMMIT` on the chunks of the PGs that reactor leads (D129), with a session layer of its own (§26.6) |
| `hyperstor-kv` | storage nodes (HS11) | Inodes, directories, exports, and the MDS's client, session, open, lock and layout records |

**The MDS is fsmeta with an NFS face.** F1's metadata servers are
stateless clients of `hyperstor-kv`. `hyperstor-nfs` links the same
metadata code rather than calling a separate fsmeta process, so an NFS
`LOOKUP` or `CREATE` is one kv transaction, not two hops.

**Exports are cluster objects (D141).** An export is a record in kv:
the filesystem and path, the tenant, the hosts allowed to mount it (by
certificate, F12), its service address and interface (D138), root
squash (on by default), the `user@domain` mapping, and the layout
policy (`files`, `flexfiles` or `auto`; D128). It is created and
changed through the cluster API and `hyperstorctl nfs export …`, by the
`storage-admin` role that manages NVMe-oF exports (administration.md
§23), and audited like them. Every MDS instance reads the same records,
so there is no `exports(5)` file to keep in step between instances.

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
  (`NFS4_PNFS_MAX_STRIPE_CNT` in Linux's files layout driver). `W` is a
  power of two, so it divides `C` and a segment's stripe index is
  `i mod W` in both layouts (§26.6).
- **Every client path uses it**, not only NFS: FUSE, virtio-fs and GPU
  clients compute the same `pg(i)`, so a file has one placement. It is
  therefore built with the filesystem's placement in HS14, not with NFS
  (D137): data placed under F4's hash would otherwise have to move.

| Option | For | Against |
|---|---|---|
| **Stripe groups** (chosen) | One layout per 64 GiB; both layouts' repeating stripe patterns express it exactly | A file's segment uses only `W` PGs: one hot file spreads over `W` primaries, not all |
| Keep F4's hash per chunk, one layout segment per chunk | Placement unchanged | One `LAYOUTGET` per 4 MiB; Linux's handling of thousands of small segments per file is unknown (G25) |
| Keep F4, a device per file listing each chunk's primary | One `LAYOUTGET` | A device per file; 4,096 entries cover only 16 GiB; every primary change invalidates the devices of every file it touches |
| Keep F4, every DS forwards to the right primary | Any layout works | A second hop for nearly every byte: the gateway pNFS removes |

### 26.6 Layouts and data servers (D128, D129, D131, D132, D140)

**Two layouts, one model (D128).** Both layouts are sparse (the DS
offset is the file offset), tightly coupled (the DS checks the MDS's
stateids, §26.7), with the chunk as stripe unit and no mirroring, and
both describe the same stripe group. The DS does not know which one a
client holds.

| | Files layout (RFC 8881 ch. 13) | Flexible files (RFC 8435) |
|---|---|---|
| Stripe unit | `nfl_util`: the chunk size; `NFL4_UFLG_DENSE` off; `NFL4_UFLG_COMMIT_THRU_MDS` off | `ffl_stripe_unit`: the chunk size |
| Stripe order | `nfl_first_stripe_index = (s · C) mod W`, `nfl_pattern_offset` = the segment's start | One mirror whose `W` data servers are listed in PG order from the base, so entry `i mod W` is `pg(i)` (RFC 8435 section 6) |
| File handles | `nfl_fh_list`: one, the file's | `ffds_fh_vers`: one per data server, the file's |
| Device | one device: `W` stripe indices, each the DS of a PG's primary | one device per data server (`ff_device_addr4`); the layout lists `W` device IDs |
| Stateid | the client sends its open, lock or delegation stateid | `ffds_stateid`: the MDS puts the file's open stateid there, and the client sends exactly that |
| Credential | the user's | the user's: with `ffdv_tightly_coupled` set, Linux uses the mount's credential, not `ffds_user`/`ffds_group` (which are set to the file's owner and ignored) |
| Flags | — | `FF_FLAGS_NO_IO_THRU_MDS`: on a DS error the client asks for a new layout instead of sending I/O through the MDS; `FF_FLAGS_NO_LAYOUTCOMMIT` off |
| `LAYOUTCOMMIT` after a stable write | only after `DATA_SYNC` (Linux skips it after `FILE_SYNC`) | always |
| Error and latency reports | none; the client returns the layout | `LAYOUTERROR` and `LAYOUTSTATS` (`ff_ioerr4`, `ff_iostats4`) |
| Linux client | every kernel | striped only from 6.18; preferred over files whenever both are offered |

**Which one a client gets is an export policy (D128):**

| Policy | `fs_layout_types` answered | For |
|---|---|---|
| `files` (default) | files | Every Linux kernel stripes it; nothing to get wrong |
| `flexfiles` | flexible files | Error reports drive recalls (D131); no MDS fallback during a primary change; exports whose clients are all 6.18 or FreeBSD |
| `auto` | flexible files to a client whose `EXCHANGE_ID` implementation id names Linux 6.18 or later or FreeBSD, files to everyone else | Mixed clients. A heuristic: Linux sends `"Linux <release> <version> <machine>"` only with `send_implementation_id` on (the default); G34 |

`fs_layout_types` is a per-request attribute, so answering it per client
is within the protocol. Offering both at once is not an option: Linux
always takes flexible files, and before 6.18 then falls back to the MDS.

| Option | For | Against |
|---|---|---|
| **Both, tightly coupled, one model, chosen per export** (D128, chosen) | One DS, one stateid and fencing model, one placement; flexible files adds error reports and fallback control; files works everywhere | A second layout encoder and the XDR of `ff_layout4`, `ff_device_addr4` and `ff_layoutreturn4` |
| Files only | Smallest | No error reports; a DS error sends I/O through the MDS until the client gets a new layout |
| Flexible files only | Reports and flags | Striping needs Linux 6.18; every older client goes through the MDS |
| Flexible files, loosely coupled | No stateid check at the DS | Synthetic uid/gid on data files, anonymous stateids, fencing all clients at once, "equivalent to AUTH_SYS", no RPCSEC_GSS (RFC 8435 sections 2.2, 15.1.1): weaker than what the DS already has |
| Block, SCSI or NVMe layout | Clients write blocks directly | Clients on raw volumes over NVMe-oF with reservations, and the MDS allocating blocks: a block filesystem, not hyperstor's object layer |

**Device IDs name primaries at their last change (D131).** A device ID
is 16 bytes (`NFS4_DEVICEID4_SIZE`):

| Layout | Device ID | `GETDEVICEINFO` answers |
|---|---|---|
| files | `(pool: 2, base PG: 4, W: 2, epoch: 8)` | the addresses of the `W` primaries |
| flexible files | `(pool: 2, PG: 4, epoch: 8, 2 unused)` | the primary's address and versions (`4.1` or `4.2`, `ffdv_tightly_coupled` on) |

`epoch` is the last map epoch at which one of the device's primaries
changed, not the current epoch, so a map change elsewhere in the cluster
does not change the ID. A flexible-files device is shared by every file
on that PG, so a client resolves each PG once. When a primary changes:

1. The old primary, if it is alive, is no longer the primary. Its DS
   forwards the request to the new primary over the cluster's own path,
   so the client sees nothing worse than a slower request.
2. If the old primary is gone, the client's I/O fails. It reports the
   error (`LAYOUTERROR` with flexible files, or `LAYOUTRETURN`), asks for
   a new layout, and gets device IDs at the new epoch. A files-layout
   client sends I/O through the MDS until then; a flexible-files client
   with `FF_FLAGS_NO_IO_THRU_MDS` retries `LAYOUTGET` instead. How
   quickly Linux does this is G27.
3. The MDS may also recall layouts that name a moved PG, or send
   `CB_NOTIFY_DEVICEID`. With flexible files, `LAYOUTERROR` tells it
   which. Both are optimisations; the forwarding in step 1 keeps every
   layout correct.

**DSs live in `hyperstord` (D129).** Each reactor serves NFS on a port
of its own, like its data port (D39), and its DS answers only for the
PGs it leads. A pNFS write then lands on the primary's own reactor,
which replicates it as any client write (replication.md §8.1): one
network hop, as for the native client. A DS is an NFSv4.1 server with
only the operations of RFC 8881 section 13.6, so each reactor keeps a
small session layer: client IDs (from `EXCHANGE_ID` with
`EXCHGID4_FLAG_USE_PNFS_DS`), sessions, slots and a reply cache, all in
memory. Nothing in it is durable: a client that loses a DS session makes
a new one, and `READ` and `WRITE` are idempotent for it.

| Option | For | Against |
|---|---|---|
| **DS per reactor in `hyperstord`** (chosen) | No extra hop or copy; the primary checks its map lease and fencing as for any request | NFS parsing in the storage daemon; more connections per client (one per reactor it uses) |
| A DS process per storage host, forwarding to the local reactors | Keeps NFS out of `hyperstord`; can be restarted alone | A local hop and a copy for every byte |
| DSs on gateway hosts | No change to storage nodes | Two hops for every byte; the gateway tier becomes the bottleneck |

**Reads go to the primary (D140).** A READ layout names the same DSs as
an RW layout: the primaries. The native client reads from the primary
too (replication.md §8.1), so NFS sees the same consistency, EC pools
need nothing special, and forwarding (D131) has one target. Reading from
any in-sync replica would spread reads over `R` nodes, but a replica
mid-recovery could answer stale data until replica read leases (D89, not
built) exist; it can come later as a policy.

**`DATA_SYNC` after every copy (D132).** A primary answers a write only
when every copy has it on stable storage. So a DS answers every `WRITE`
with `DATA_SYNC`, even when the client asked for `UNSTABLE`, and the
client never needs a `COMMIT`. The write verifier is the DS's boot
instance; `COMMIT`, if a client sends one, is a no-op. It is not
`FILE_SYNC` because the inode's size and mtime are not yet committed,
and because the Linux files-layout client skips `LAYOUTCOMMIT` after a
`FILE_SYNC` write (`filelayout_set_layoutcommit`), which would leave the
size wrong for good.

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
duration, as `hyperstor-vhost` does for 512-byte sectors (D126). Any
pool may back an export (decision-log.md point 82): on an EC pool an
unaligned write costs a stripe read-modify-write, which G36 measures
before the default pool policy for NFS trees is final.

### 26.7 File handles, state, failover and fencing (D133, D134, D138, D139)

**File handles** are opaque to clients and up to 128 bytes. Hyperstor's
is `(format, fs, inode, generation)`, about 24 bytes. The generation
changes when an inode number is reused, so an old handle gets
`NFS4ERR_STALE`. F2's inode record needs that generation.

**NFS state lives in kv (D134).** The MDS keeps no state that only it
knows:

| Record | Contents | Written |
|---|---|---|
| client | client owner, verifier, principal, `csa_sequence` of the last `CREATE_SESSION` and its reply, lease renewed at | at `EXCHANGE_ID` and `CREATE_SESSION`; renewals coarsely, at most every lease/3 |
| session | session id, slot count, channel attributes, flags | at `CREATE_SESSION` and `DESTROY_SESSION` |
| open | stateid, share access and deny, open owner | in the `OPEN` transaction |
| lock | byte ranges, lock owner | the same records as F7's locks, so NFS and FUSE clients see each other's locks |
| layout | stateid, file, ranges, iomode, client | at `LAYOUTGET`, `LAYOUTRETURN` |
| cached reply | the reply to a non-idempotent request (`sa_cachethis`), keyed by session and slot | in the same transaction as the change it reports |

Sessions are in kv so that a retried `CREATE` or `RENAME` after a
takeover lands on the same session and slot and gets its cached reply
rather than running twice. With the session durable the server sets
`CREATE_SESSION4_FLAG_PERSIST` truthfully. Slot sequence numbers are in
memory; a slot's first request after a takeover is checked against the
cached reply in kv.

**Every kv transaction checks the lease.** An MDS instance writes NFS
state only in transactions that also assert it still holds the export's
lease (D138). An instance that was partitioned away cannot grant an open
or a lock after another instance took over, the same fence D125 gives
the map service leader.

**Failover without a grace period.** A client reaches one service
address per export, held by one MDS instance at a time through a lease
in kv; standbys take the address over when that lease runs out. The new
instance finds every client, session, open, lock and layout in kv, so
clients keep their client IDs, sessions and stateids. A client that
retries on the new instance reconnects to its session
(`BIND_CONN_TO_SESSION`; G29), and nothing has to be reclaimed.

**The address moves with the lease (D138).** `hyperstor-nfs` adds the
export's address to its own interface when it wins the lease, announces
it with gratuitous ARP (or an unsolicited neighbour advertisement), and
drops it when it loses or releases the lease. It needs `CAP_NET_ADMIN`
for that and nothing else. The address and interface are part of the
export record (D141).

| Option | For | Against |
|---|---|---|
| **`hyperstor-nfs` moves the address** (chosen) | One source of truth for who serves an export: the kv lease; nothing outside hyperstor to install or keep consistent | `CAP_NET_ADMIN`; layer-2 reachability, or a router that learns the move |
| keepalived/VRRP beside the MDS | Standard | A second cluster with its own view of who is alive, which can disagree with the lease |
| Several A records and client trunking | No address moves | Depends on Linux trunking across instances (G30); ESXi doesn't do it |

**Scaling out.** Exports, not requests, are spread over MDS instances:
each export's address is held by one instance. A filesystem with many
clients can be exported under several addresses. NFSv4.1 trunking across
instances (the same `server_owner` from all of them) would let one
client use several MDS instances for one export; whether Linux does that
is G30.

| Option | For | Against |
|---|---|---|
| **All state in kv** (chosen) | Failover in seconds, no grace period, no reclaim storms; any instance can recall or fence | A kv write in every `OPEN`, `LOCK`, `LAYOUTGET` and `CREATE_SESSION` |
| State in memory, a cluster-wide grace period after failover (NFS-Ganesha's clustered recovery) | Cheaper opens | 90 s or more of refused opens after every MDS failure; stable storage is needed anyway for which clients may reclaim (RFC 8881 section 8.4.3) |

**Sealed stateids (D133).** The I/O a DS sees carries an open, lock or
delegation stateid (RFC 8881 section 13.9.1), never a layout stateid, so
those are what the MDS seals:

- The 12-byte `other` field of every open, lock and delegation stateid
  is `(client index: 4, MAC: 8)`. The MAC is computed under a cluster key
  that every `hyperstord` holds, over the client, the file handle, the
  share access granted (read, write or both) and the stateid kind.
- A DS recomputes the MAC over the stateid, the file handle from `PUTFH`
  and the access the operation needs, and refuses a mismatch with
  `NFS4ERR_BAD_STATEID`. A read-only open cannot write. There is no
  lookup and no table.
- With flexible files the MDS copies the same open stateid into
  `ffds_stateid`; the DS check is identical.
- The server sets `EXCHGID4_FLAG_BIND_PRINC_STATEID`, so I/O must carry
  the principal that opened the file, and the access check made at
  `OPEN` covers the I/O (G35 checks that Linux honours it).
- **Revoking a client fences it everywhere:** the MDS puts the NFS
  client's instance on the cluster blocklist (D24, D76), and every DS
  refuses its stateids from then on. This happens when a client's lease
  expires or it doesn't return a recalled layout in time. It meets RFC
  8434's fencing requirement, and RFC 8435 section 2.2's for tightly
  coupled devices (the client sees `NFS4ERR_BAD_STATEID`).
- How the cluster key reaches storage nodes, and how it is rotated, needs
  a security review before it is built.

**The DS is stateless about opens (D139).** A sealed stateid stays valid
at the DS after the client's `CLOSE`, and after a `chmod` or ACL change
that would now refuse the open, until the client is blocklisted or the
key rotates. RFC 8881 section 13.9.2 asks for `CLOSE` and restrictive
permission changes to reach the DSs; hyperstor does not do that yet.

- **Why it is acceptable:** the harm is bounded to a file the same
  authenticated host and user could already write; an open file
  descriptor keeps its access after `chmod` under POSIX too; and the
  only clients that write with a closed stateid are broken or hostile
  ones, which the blocklist handles.
- **Why not propagate:** it is a control message to `W` primaries for
  every `OPEN` and `CLOSE`, and an open table on every reactor, to
  revoke what is rarely revoked.
- **Later:** per-file fencing as a fence generation in the inode, folded
  into the MAC and checked against a small per-reactor cache; it revokes
  one file's stateids without blocklisting the client.

| Option | For | Against |
|---|---|---|
| **Stateless: the sealed stateid alone** (chosen) | No lookup per request; no control traffic per open | Bounded staleness after `CLOSE` or `chmod` |
| Propagate opens and permission changes to the DSs (RFC-exact) | Immediate revocation | A message per open to `W` primaries, an open table per reactor, and a window anyway while it propagates |

### 26.8 NFSv4.2 operations

| Operation | Hyperstor |
|---|---|
| `SEEK`, `READ_PLUS` | Holes are chunks that don't exist and blocks no chunk maps; the DS asks the engine for a chunk's extents |
| `ALLOCATE` | Reserves space in the tenant's quota (tenants.md §24.4); blocks aren't written |
| `DEALLOCATE` | Deletes whole chunks and zeroes partial ones (`Zero`) |
| `CLONE` | Needs files that share chunk versions, as volume clones do (volumes.md §10.3). With a lineage per file it is the same mechanism; until then, refused (`NFS4ERR_NOTSUPP`) |
| `COPY` | Server-side: the MDS has the primaries copy chunk by chunk, so data never reaches the client; asynchronous for large files (`CB_OFFLOAD`) |
| `LAYOUTERROR`, `LAYOUTSTATS` | Flexible files only: DS errors and latencies, kept as metrics and used to recall stale layouts (D131) |

### 26.9 Security and identity (D135)

- **Transport:** RPC-with-TLS (RFC 9289) with mutual TLS. A client host
  presents a hyperstor host certificate (administration.md §23.8),
  which says which tenant and exports it may mount, as F12 does for FUSE
  hosts. Linux has it from 6.5 with `tlshd` (`mount -o xprtsec=mtls`).
  Whether the Linux client also uses TLS for DS connections, with either
  layout, is G28; until that is known, pNFS without TLS is allowed only
  on trusted networks (D25).
- **Users:** AUTH_SYS user and group ids are trusted only from an
  authenticated host, with root squashed by default. Owners travel as
  numeric ids; `user@domain` mapping is configured per export (D141).
  I/O carries the principal that opened the file
  (`EXCHGID4_FLAG_BIND_PRINC_STATEID`, §26.7).
- **Kerberos** (RPCSEC_GSS) comes later. Tight coupling allows it (RFC
  8435 section 15.1.2); it would need keys on every storage node.
- **Ports:** the MDS on 2049 on the client network; a DS port per
  reactor on storage networks (protocol.md §13.2). NFS clients must
  therefore reach the storage networks' DS port range through any
  firewall between them, as native clients do; a client that can't
  still works through the MDS, slowly. Neither port carries
  administration.
- **ACLs:** mode bits first; NFSv4 ACLs with POSIX ACLs (filesystem.md
  §1, later).

### 26.10 Alternatives considered and phasing (D127, D137)

| Option | For | Against |
|---|---|---|
| **Own Rust server, `hyperstor-nfs`** (D127, chosen) | One language, the reactor, state in kv, DSs in `hyperstord`; nothing external in the data path | The largest piece of work: XDR from the RFCs' `.x` files, the COMPOUND engine, sessions, state. No mature Rust NFSv4.1 server exists to start from (`nfsserve` and its forks are NFSv3 only) |
| NFS-Ganesha with a hyperstor FSAL | A mature NFSv4.1 state machine and clustered recovery | C, through FFI to the Rust client library; its pNFS code is unmaintained (Ganesha wiki, December 2025); state in its own recovery backends, not kv |
| The kernel's `nfsd` re-exporting a FUSE mount | Nothing to write; a stopgap like ublk plus `nvmet` (D19) | No pNFS (`nfsd` lays out only block, SCSI and a test-only flexfiles); every byte through one host and FUSE; file handles across FUSE restarts |

**Phasing (D137):**

| Step | Delivers | Depends on |
|---|---|---|
| N0 | `hyperstor-nfs` as an NFSv4.1 server doing all I/O itself, with its state in kv (D134) and exports as cluster objects (D141): every v4.1 client works, and a second instance can take an export over | HS11 (kv), the metadata part of HS14 with stripe groups (D130) |
| N1 | Files layouts and DSs in `hyperstord` with sealed stateids (D128, D129, D133); the address moving with the lease (D138) | N0 |
| N2 | Flexible files layouts, the `auto` policy, error reports driving recalls (D128, D131) | N1 |
| N3 | NFSv4.2 operations, delegations | N1 |
| N4 | NFS over RDMA, Kerberos | N1, HS6 |

N0 keeps its state in kv from the start: an in-memory state machine
would need the grace period and reclaim that D134 exists to avoid, and
would be thrown away at N1.

### 26.11 Host checks

- **G25. Many small layout segments on Linux** (D130). Against a test
  MDS, give a Linux client one files-layout segment per 4 MiB chunk on a
  large file, on 5.14, 6.8, 6.12 and 6.18: `LAYOUTGET` rate, throughput
  and memory, against one 64 GiB segment with stripe groups.
- **G26. Layout type choice** (D128). With the server offering both
  types in `fs_layout_types`, that Linux (5.14 to 6.18) and FreeBSD take
  flexible files; and that a Linux client before 6.18 given a striped
  flexible-files layout falls back to I/O through the MDS rather than
  failing the I/O, so that `auto`'s guard is needed and sufficient.
- **G27. A data server fails** (D131). Kill a DS's node under a pNFS
  writer, with each layout: how long until the client reports the error,
  asks for a new layout, uses the new devices, or falls back to the MDS
  (files) or retries `LAYOUTGET` (flexible files with
  `FF_FLAGS_NO_IO_THRU_MDS`); and that it loses no acknowledged write.
- **G28. TLS and RDMA to data servers** (D135). Whether Linux's
  `xprtsec=mtls` applies to DS connections for the files and the
  flexible files layout, and whether a client mounted over RDMA also
  reaches DSs over RDMA.
- **G29. MDS failover** (D134, D138). Move an export's address between
  MDS instances under load: that the new instance's gratuitous ARP is
  honoured by the test network's switches, that Linux clients
  `BIND_CONN_TO_SESSION` to their persisted session and keep their
  stateids without reclaim, and that a retried non-idempotent request
  gets its cached answer.
- **G30. Trunking across MDS instances** (D134). Whether Linux uses
  several MDS addresses with the same `server_owner` for one mount.
- **G31. Clients without pNFS** (D136). What ESXi 8 and macOS 15
  negotiate, and that ESXi works through the MDS alone.
- **G32. `DATA_SYNC` and `LAYOUTCOMMIT`** (D132). That with every DS
  write answered `DATA_SYNC` the Linux client sends no `COMMIT` and does
  send `LAYOUTCOMMIT`, with both layouts, and that it reports the right
  size after appends and holes. Also what it does with `FILE_SYNC`, to
  confirm the skipped `LAYOUTCOMMIT`.
- **G33. Recall storms** (D131). Move many PG primaries at once (a node
  failure): how Linux clients cope with forwarded requests, failed DS
  I/O and recalls, and how many `LAYOUTGET`s reach the MDS.
- **G34. The implementation id** (D128). Which distributions ship
  `send_implementation_id` on, what RHEL and Ubuntu kernels put in
  `nii_name`, and what FreeBSD sends, so that `auto` classifies them
  right and falls back to files when it can't.
- **G35. Principal binding** (D133, D139). That Linux honours
  `EXCHGID4_FLAG_BIND_PRINC_STATEID` (I/O under a different user on an
  open file goes through a new `OPEN`), and what it does when the DS
  answers `NFS4ERR_BAD_STATEID` to a write on a read-only open.
- **G36. Unaligned writes on EC pools** (D132, decision-log.md point
  82). Throughput and latency of 4 KiB-unaligned pNFS writes on a
  replicated and an EC pool, against aligned ones, to set the default
  pool policy for NFS directory trees.

### 26.12 References

- RFC 8881, *Network File System (NFS) Version 4 Minor Version 1
  Protocol* (2020; obsoletes RFC 5661): ch. 12 (pNFS), ch. 13 (files
  layout; sections 13.6 and 13.9 for data servers), section 2.10
  (sessions), section 8.4 (reclaim), section 18.36 (`CREATE_SESSION`).
- RFC 7862, *NFS Version 4 Minor Version 2 Protocol* (2016).
- RFC 8434, *Requirements for Parallel NFS (pNFS) Layout Types* (2018),
  section 3.1.
- RFC 8435, *Parallel NFS (pNFS) Flexible File Layout* (2018): sections
  2.2 (fencing, coupling), 5.1 (`ffds_stateid`), 6 (striping), 15.1
  (security); draft-haynes-nfsv4-flexfiles-v2 (-08, August 2026,
  individual draft).
- RFC 5663 (block layout), RFC 8154 (SCSI layout), RFC 9561 (SCSI layout
  over NVMe), RFC 5664 (object layout).
- RFC 8166 and RFC 8267 (RPC and NFS over RDMA); RFC 9289 (RPC-with-TLS).
- Linux: `fs/nfs/pnfs.c` (`ld_prefs`, `set_pnfs_layoutdriver`);
  `fs/nfs/filelayout/filelayout.c` (`filelayout_set_layoutcommit`) and
  `filelayout.h` (`NFS4_PNFS_MAX_STRIPE_CNT` 4096,
  `NFS4_PNFS_MAX_MULTI_CNT` 256); `fs/nfs/flexfilelayout/`
  (`ff_layout_get_ds_cred`, `nfs4_ff_layout_select_ds_stateid`,
  `ff_layout_alloc_lseg`; striped layouts in commits d442670 to 20b1d75,
  2025-09-26, released in 6.18); `fs/nfs/nfs4xdr.c`
  (`encode_exchange_id`, the implementation id); the 6.11 NFS client
  merge (NVMe block layout); RPC-with-TLS in the 6.4 server and 6.5
  client.
- FreeBSD pnfs(4); NFS-Ganesha wiki (pNFS status); VMware vSphere storage
  documentation (NFS 4.1); Microsoft Windows Server NFS overview.
