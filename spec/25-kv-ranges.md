# Key–value ranges

How `hyperstor-kv` keeps a sorted keyspace in range objects, finds them,
splits and merges them, and changes one row atomically: milestone HS11,
the part of D35 the S3 gateway needs (D191). Transactions across ranges
and the timestamp oracle are HS11b (08-engine.md §7.5). Part of the
hyperstor design; sections keep their numbers across documents, so `§N`
in one document is the same section everywhere (see
[README.md](README.md)).

Status: **designed, not yet built** (2026-11-01). Settled with the
owner: D191–D199 (points 128–137) and D200–D202 (points 169–172 of
[05-decision-log.md](05-decision-log.md)). The rest of this document
(key encoding, ids, the root's layout, the deletion queue) was decided
while drafting and is open for review.

## 28. Key–value ranges (HS11)

### 28.1 Model

- **Store.** A keyspace with its own root, one per user of `hyperstor-kv`:
  the S3 gateways share store 1; each filesystem (HS14) will get its own.
  Why: one root per user keeps their names and load apart.
- **Table.** A store holds tables, each with a name, an id and an index
  pool. An S3 bucket is a table (D197, D198). A range never holds two
  tables' rows. Why: each table's ranges live in that table's pool, and
  deleting a table deletes whole ranges.
- **Row and column.** A key is a row and a column, both byte strings.
  An S3 row is an object name; its columns are its versions and its
  "latest" entry. Ranges split only between rows, so a row is never
  divided (D200).
- **Atomic unit: one row** (D200). A batch of changes to one row's
  columns applies atomically, with assertions on its columns (absent, or
  equal to a checksum: D194). There is nothing atomic across rows until
  HS11b. Why: a split can't break a one-row batch, so it never fails for
  a reason the caller can't see; S3 needs nothing wider (D191).
- **Reads** are of a column, a row, or the rows from one row to another
  (ordered, paged, with a prefix). They come from the range's primary
  under its read lease, so a read sees every change acknowledged before.
  A scan that spans ranges reads each range in turn. It is not a
  snapshot across ranges (HS11b brings one).

### 28.2 Range objects

A range is an object in namespace `kv` (08-engine.md §7.2) of its
table's index pool, named by a random 128-bit range id. Why random: no
allocator to coordinate, as volume ids.

- **Attributes:**

| Attribute | Value |
|---|---|
| `store`, `table` | ids |
| `start`, `end` | the first row it holds, and the first it doesn't (absent: the table's end) |
| `epoch` | grows with every change of `start`, `end` or `state` |
| `state` | `pending` (being filled, not yet serving), `serving`, `splitting` (at a row, into a named range id), `absorbing` (a named neighbour), `merging` (into a named neighbour) |
| `next` | after a split: the range id now holding the rows from `end` on, a hint for clients with an old root |

- **Key–value table.** User keys are stored as `r` + row + column. The
  row is encoded so that its end can be found and order is kept: each
  0x00 byte becomes 0x00 0xFF, and the row ends with 0x00 0x01 (as
  FoundationDB's tuple layer encodes byte strings). Keys under `q` hold
  the range's deletion queue (§28.6). Why: a split point found by
  decoding keys is always a row boundary.
- **Writes carry the epoch.** Every batch a client sends to a range
  asserts its `epoch` (`AssertAttr`, which exists). A batch prepared
  under old bounds therefore fails as a whole with `Conflict`. The
  client then reads the range's attributes again and either sends the
  batch under the new epoch or goes elsewhere. Why: no new message and
  no bounds check in the daemon. Clients are trusted library code, as
  with every object operation.
- **Reads return the attributes.** `Lookup` answers with the
  attributes and the key slice from one read on the primary, so a reader
  checks the bounds and state against the keys it got.
- **One new engine operation:** `AssertKv(key, checksum or absent)`,
  D194's per-key assertion, in the engine and on the wire (`WireOp`).
  `Lookup` also reports the table's byte count, for split and merge
  decisions.

### 28.3 The root

- Each store has one **root**: a `kv` object in `.meta` with a fixed name
  (the store id) (D197). Its table holds:
  - `t` + table name → the table record: id, index and data pools,
    state, settings. This is D197's bucket record;
  - `i` + table id → the table name, so a range can find its table;
  - `m` + table id + start row → range id and epoch, one entry per range.
- **The root is a map, and range objects have the final word.** A range's
  own `start`, `end` and `state` decide what it serves. A root entry that
  is out of date costs a client one more hop (to `next`, or a fresh read
  of the root), never a wrong answer.
- **Clients cache the root's entries**, by table, on first use. A
  `Conflict` on the epoch, a row outside a range's bounds, or a missing
  range object makes them read the affected entries again.
- **The root doesn't split.** At about 100 bytes an entry, a 64 MiB root
  maps 600,000 ranges, about 40 PiB of index. Health warns past
  32 MiB. Why: one fixed object is simple to find, and a second level
  can be added under the same name if it is ever needed.
- **Creating a table:** the client writes the first range object
  (`serving`, the whole table), then, in one operation on the root,
  asserts the name absent (`AssertKv`) and puts the `t`, `i` and `m`
  entries. A crash between leaves a range object no root entry names. Its
  primary deletes it after an hour (§28.5).
- **Deleting a table** (an S3 bucket must be empty): one operation on the
  root marks the table `deleting` and removes the name; then the client
  deletes the range objects and the `m` and `i` entries.

### 28.4 Splits

The primary of the range's PG splits a range that grows past 64 MiB
(D201, D202). Each step is one operation on one object and is recorded
in the range's attributes, so a new primary after failover resumes it.
Range R holds rows `[a, c)` at epoch E:

1. **Freeze the upper half.** Choose the middle row `b` by bytes, then
   set R's `state` to `splitting` (at `b`, into a new id R2) and its epoch
   to E+1. Clients see the new state and hold writes to rows ≥ `b`,
   retrying every 50 ms; writes below `b` go on at E+1 (D201).
2. **Copy.** Create R2 (`pending`, rows `[b, c)`, same table and pool) and
   copy rows ≥ `b` from R into it, in batches under the engine's limit
   on one list (`max_list_bytes`). The frozen rows don't change, so one
   pass is enough.
3. **Open R2:** set its `state` to `serving`.
4. **Shrink R**, asserting epoch E+1: set `end` to `b`, `next` to R2,
   `state` to `serving` and the epoch to E+2, and delete rows ≥ `b`
   (`KvDelRange`, one operation). The split is done at this point.
5. **Update the root:** put R2's entry (asserting it absent) and R's new
   epoch.

- **A crash before step 3** abandons the split: the new primary deletes
  R2 and sets R `serving` at a new epoch. A crash after step 3 finishes
  steps 4 and 5. Why: a copy is cheap to repeat, and abandoning needs no
  record of how far the copy went.
- **Freezing costs a pause** of about the copy's time, for writes to the
  upper half only: 32 MiB at a few hundred MB/s is roughly 0.1–0.3 s.
  Copying first and catching up after (a short last pass) can come later
  if measurements call for it.
- **A row larger than 64 MiB** isn't split. The range stays large and
  health names it.
- **No split on write rate** yet (D202). Why: no measured hot spot calls
  for it; size alone keeps every range object bounded (I188).

### 28.5 Merges and housekeeping

- **Merging** (D202): when a range and its right neighbour in the same
  table hold less than 16 MiB together, the left one's primary merges
  them. Why 16 MiB: a quarter of the split size, so a range that just
  split doesn't merge back.
  1. Set the left range L to `absorbing` (the right one, N), and N to
     `merging` (into L) at a new epoch. N's writes are held, as a frozen
     half is.
  2. Copy N's rows into L. L's bounds still exclude them, so no reader
     sees them yet.
  3. Extend L, asserting its epoch: set `end` to N's `end`, `state` to
     `serving`, and a new epoch.
  4. Update the root: delete N's entry (asserting its value), put L's
     new epoch.
  5. Delete N.

  A crash before step 3 abandons the merge: the copied rows are deleted
  from L (`KvDelRange` past its `end`) and both ranges serve again at new
  epochs. A crash after step 3 finishes steps 4 and 5. N is deleted only
  after the root no longer names it. Why: a client
  that finds a range missing reads the root again, which must then
  point elsewhere.
- **Leftover objects.** A range's primary checks, an hour after a range
  object appears, that the root or a neighbour's `splitting` state names
  it. One that nothing names (a crash while creating a table, or an
  abandoned copy) is deleted.
- **Root repair.** A primary that finds the root's entry for its range out
  of date (a split stopped before its step 5, a merge before its step 4) puts it right.

### 28.6 The deletion queue

D192's collection records, in a form other users can share: a batch on a
row may also add entries to the range's queue (`q` + a sequence number),
each naming objects to delete (pool, namespace, name). The range's
primary deletes them in the background, then removes each entry. A split
leaves the queue with the lower range. Why: the entry is written
atomically with the change that orphans the objects, nothing is
scanned, and the primary that holds the queue drains it.

### 28.7 The library and the daemon

- **`hyperstor-kv`**, a library on `hyperstor-client`:
  - open a store;
  - create, look up, list and delete tables;
  - `get`, `scan` (rows, prefix, limit, continuation);
  - `apply(row, batch)`: puts, deletes, assertions, and queue entries.

  It caches the root, routes by row, handles `Conflict` and `next`, and
  holds writes during a freeze. The S3 gateway (HS12) is its first user.
- **In `hyperstord`**, each primary watches the `kv` objects it holds for
  size, at most once a second per object written. It runs splits,
  merges, leftover checks and queues, one job per range at a time and a
  few per device. These are the only background work in a storage
  daemon that writes to other PGs, through its client.

### 28.8 Build stages and tests

1. `AssertKv` in the engine and on the wire, and the table's bytes in
   `Lookup`. Tests: the engine's randomized test against its model
   covers the new assertion.
2. `hyperstor-kv` without splits: key encoding, the root, tables, rows,
   scans, the routing cache. Tests: a randomized test against a sorted
   map, on an in-process cluster.
3. Splits and merges in `hyperstord`. Tests: the same randomized test,
   with sizes that split and merge often and primaries killed at every
   step (the fault rig). After each step the store must equal the model,
   every row must be in exactly one serving range, and no object may be
   left over.
4. The deletion queue. Tests: queued objects deleted once and only
   after their batch commits, through primary failover.
