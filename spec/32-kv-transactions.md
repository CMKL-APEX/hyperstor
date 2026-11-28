# Key–value transactions

`hyperstor-kv`'s transactions across rows and ranges (milestone HS11b),
which the filesystem's metadata needs (00-filesystem.md F1, 29-hsfs.md
§32.10, stage H2). They build on the ranges of 25-kv-ranges.md §28 and
the outline of 08-engine.md §7.5 (D35). Part of the hyperstor design;
sections keep their numbers across documents, so `§N` in one document is
the same section everywhere (see [README.md](README.md)).

Status: **design.** D425–D428 are settled with the owner (05-decision-log.md
points 385–388, 2026-11-28); D429–D432 were taken while drafting and are
open for review. Host checks G73–G74 are to run. Nothing is built.

## 35. Key–value transactions

### 35.1 What it is

- **Snapshot isolation, and locks on what a transaction read** (D425).
  A transaction reads a snapshot taken at its start and commits only if
  no other transaction wrote a key it writes since. It can also lock a
  key it only read; such a key then conflicts as if written. Why:
  snapshot isolation (Percolator's) costs nothing for reads, and the
  filesystem's one known write skew, two directory renames that together
  make a cycle (00-filesystem.md §4), is closed by locking the ancestors
  each rename read. Serializable everywhere would track every read for a
  case only renames have.
- **Per table** (D426). A table is made transactional or not, and stays
  so. Transactional tables keep versions of each key by commit
  timestamp; the others keep today's one value per key and one-row
  batches (§28.1). S3 buckets and the dataset name index stay as they
  are. Why: S3 needs nothing wider (D191), and versions cost bytes and
  collection on every write.
- **The client coordinates.** A transaction runs in the library of the
  process that opened it (`hyperstor-fsmeta`'s servers, F1); there is no
  coordinator service. A transaction left half-way by a client that
  stopped is finished, or undone, by the next reader that meets it
  (§35.4). Why: as Percolator; nothing to fail over.
- **Single-range transactions commit in one operation** (D35). Every key
  in one range: one atomic operation list on the range's object, with no
  locks. Others take two phases (§35.4).

### 35.2 Records

A transactional table's range holds, for each user key (row and column,
§28.2):

| Key | Value | What |
|---|---|---|
| `v` + row + column + slot 0 | the lock: primary key, start timestamp, time-to-live, kind (`put`, `delete`, `lock`), the value, and on the primary the other keys | a transaction in flight |
| `v` + row + column + slot `!ts` | start timestamp, kind, the value | a version committed at `ts` |
| `v` + row + column + slot `!ts` | start timestamp, `rollback` | a transaction undone (its start `ts`) |

- **Slots** (D429). A key's lock and versions sit together, after its
  encoded row and column (both encoded as rows are, §28.2), by a slot:
  0 for the lock, then `!ts` (the timestamp's bitwise complement,
  big-endian) for each version, newest first. Why: one `Lookup` from slot
  0 returns the lock, if any, then the newest versions; a split, which
  falls only between rows (D200), never parts a key from its lock or
  versions.
- **The value travels in the lock** (D429). A commit, or a reader rolling
  a key forward, makes the version from the lock alone: one operation
  that asserts the lock (`AssertKv`), deletes it and puts the version.
  Values are at most 64 KiB in transactional tables. Why: the reader that
  finishes another client's transaction has only the lock; the
  filesystem's records (inodes, entries, F2) are far smaller.
- **A new assertion, `AssertKvEmpty(from, to)`** (D431): no key of the
  object's table in `[from, to)`. Decided by the primary, as `AssertKv`
  (I6), it never reaches the engine or the replicas. A prewrite asserts
  that a key has no version newer than its start (`[slot 0 + 1, slot
  !start)` empty) and no lock. Why: `AssertKv` names one key, and a
  newer version can have any timestamp. A new message kind, so older
  builds refuse it rather than misread it.
- **The table record says transactional** (D426): a field appended to
  the root's `t` entry (§28.3), which older readers skip. A plain batch
  on a transactional table is refused by the library. Why: a write
  without locks would break isolation.

### 35.3 Timestamps

- **The map service's leader hands them out** (D428, confirming D35's
  oracle). It serves them from memory in increasing order and writes a
  high-water mark through Raft that reserves the next three seconds'
  worth, so a new leader starts above anything handed out. One Raft write
  per three seconds, whatever the rate.
- **Clients batch.** A client asks the leader for timestamps on the map
  service connection it already holds, at most one request in flight,
  and hands the answer to every transaction that waited for one. A
  member that isn't the leader answers with the leader, as other
  leader-only requests do. Why: one round trip to the leader per batch,
  not per transaction.
- **A leader change pauses transactions** for its length (the Raft
  election), as other leader-only work pauses.
  Hybrid logical clocks would avoid it at the cost of trusting clock
  skew bounds and restarting reads near them; not taken.

### 35.4 A transaction

1. **Begin:** take a start timestamp `s`.
2. **Read** at `s`: per key, the lock and the newest version at or before
   `s` (§35.2). A lock with a start at or before `s` must be resolved
   first (below). Scans read each range in turn at the same `s`, so a
   scan across ranges is a snapshot (§28.1 had none).
3. **Write and lock:** puts, deletes and `lock(key)` are kept in the
   client until commit.
4. **Commit, one range:** take a commit timestamp `c`, then one operation
   on the range: for each key, assert no lock and no version after `s`
   (`AssertKvEmpty`), and put its version at `c` (a `lock` key gets a
   version of kind `lock`, which conflicts as a write would). Committed
   when it applies.
5. **Commit, several ranges** (D427): choose a primary key (the first
   written). **Prewrite** every key, the primary first: per range, one
   operation that asserts no lock and no version after `s` and puts the
   lock. Then take `c` and **commit the primary**: assert its lock, delete
   it, put its version at `c`. The transaction is committed at that
   moment. The other keys are committed the same way afterwards, in the
   background, and a reader that gets there first does it instead.
6. **Conflicts:** an assertion that fails at prewrite is a conflict:
   the client removes the locks it put (writing `rollback` versions) and
   the transaction fails with `Conflict`. Callers retry from the start,
   as with one-row batches.

**Resolving a lock** another transaction left on a key, as a reader or a
writer meets it:

- the primary has a version with the lock's start: committed; roll the
  key forward (§35.2);
- the primary has a `rollback` with that start, or no lock and no
  version: undone; delete the lock (writing `rollback` on the primary
  first, if it had neither, so a late commit fails its assertion);
- the primary still has its lock, within its time-to-live: wait (50 ms
  steps, as for a split, D201), then look again;
- past its time-to-live: undo it, as above.

**Time-to-live** (D432): 3 s from the lock's start; a client whose
transaction runs longer renews its primary's every second. No
transaction runs past 10 minutes; the library aborts it. Why: a stopped
client's locks stall readers for at most 3 s, and the bound lets old
versions be collected (§35.6).

**Async commit later** (D427): the primary's lock already names the other
keys, so a later stage can count the transaction committed once every
prewrite has landed (about one round trip less, as TiKV's async commit),
when G73's latencies call for it.

### 35.5 Splits and merges

Nothing new. Locks and versions are keys of their row (§35.2), so they
move with it. A prewrite or commit sent under an old epoch fails the
epoch assertion (§28.2) and is sent again to the range that now holds
the row, as a one-row batch is. A transaction's keys are found by row,
so a split between prewrite and commit costs one more hop.

### 35.6 Old versions

- **Safe point** (D430): the leader keeps one in Raft: the timestamp of
  10 minutes ago, held back to the oldest filesystem snapshot that is
  still kept. Why: no transaction lives longer (§35.4); a snapshot reads
  its metadata at its timestamp (00-filesystem.md §4), so its versions
  stay.
- **Collection:** each range's primary, in its background work (§28.6),
  removes, per key, every version older than the newest one at or before
  the safe point, and every `rollback` older than the safe point. A key
  whose newest version is a `delete` at or before the safe point goes
  entirely.
- **Snapshots** name their timestamps in Raft, so the safe point never
  passes one while it exists. Design of filesystem snapshots themselves
  is H3's (29-hsfs.md §32.10).

### 35.7 Stages

| Stage | What | Shows it |
|---|---|---|
| K1 | Transactional tables, `AssertKvEmpty`, the oracle, single-range transactions | A model of snapshot isolation over one range |
| K2 | Several ranges: prewrite, primary commit, resolution, time-to-live, `lock` | Jepsen-style histories checked for snapshot isolation under node and client faults (17-milestones.md HS11b) |
| K3 | Old versions: the safe point, collection, snapshots' timestamps | A model with collection running; bytes kept flat under steady churn (G74) |
| K4 | Async commit, if G73 calls for it | The same histories |

### 35.8 Host checks

- **G73. Commit latency on the pilot.** Single-range and two-phase
  commits of 2, 4 and 8 keys over 1, 2 and 4 ranges, at QD1 and QD32,
  against a one-row batch, and the oracle's batches per second: whether
  two-phase commit's extra round trip matters for metadata operations
  (D427).
- **G74. Collection keeps up.** A table rewritten at the filesystem's
  expected metadata rate for an hour: bytes stay flat once the safe
  point passes, and collection's cost to the range's primary.
