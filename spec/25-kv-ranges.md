# Key–value ranges

How `hyperstor-kv` keeps a sorted keyspace in range objects, finds them,
splits and merges them, and changes one row atomically: milestone HS11,
the part of D35 the S3 gateway needs (D191). Transactions across ranges
and the timestamp oracle are HS11b (08-engine.md §7.5). Part of the
hyperstor design; sections keep their numbers across documents, so `§N`
in one document is the same section everywhere (see
[README.md](README.md)).

Status: **built** (2026-11-04: stages 1–4 of §28.8, I255, I256, I260,
I261). HS11 is done; transactions are HS11b. Settled with the
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
  holding given bytes: D194). There is nothing atomic across rows until
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
| `next` | after a split: the range id now holding the rows from `end` on, a hint for clients with an old root; cleared once the root names it |
| `merged` | after a merge: the neighbour whose rows it took, until the root no longer names that one and it is deleted |
| `parent` | for a range a split made: the range it was split from (§28.5) |

- **Attribute values:** ids and the epoch are big-endian; `start` and
  `end` are rows as they are; `state` is a tag byte (1 `pending`,
  2 `serving`, 3 `splitting`, 4 `absorbing`, 5 `merging`), then the range
  id it names and, for `splitting`, the row (I256).

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
- **One new assertion:** `AssertKv(key, value or absent)`, D194's
  per-key assertion (built, I255). Like the other assertions on the wire
  it is decided by the primary, holding the object, and never reaches the
  engine or the replicas (I6). It compares exact bytes (D194 revised,
  point 190). Why: never wrong, and the values asserted (index entries,
  root entries) are small.
- **The table's size.** The engine keeps each table's bytes (keys and
  values) as it changes. A *sized* lookup returns them, for split and
  merge decisions; it travels as its own message kind, so `Lookup` and
  its answer stay as older builds read them.

### 28.3 The root

- Each store has one **root**: a `kv` object in `.meta` with a fixed name
  (the store id) (D197). Its table holds:
  - `t` + table name → the table record: a format byte, the id, the
    index and data pools, and its user's own record (S3's bucket record:
    tenant and creation time, I262). Later fields (settings) are
    appended, and a reader skips those it doesn't know. This is D197's
    bucket record;
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
  root removes the name, asserting the record is the one read; then the
  client deletes the range objects, then the `m` and `i` entries. An `i`
  entry whose name is gone marks a delete that stopped half-way, to be
  finished by whoever finds it (I256).

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
   copy rows ≥ `b` from R into it, in lists of at most 1 MiB, each
   asserting R2 is still `pending`. The frozen rows don't change, so one
   pass is enough.
3. **Open R2:** set its `state` to `serving`, at epoch 2, asserting it
   `pending`.
4. **Shrink R**, asserting epoch E+1: set `end` to `b`, `next` to R2,
   `state` to `serving` and the epoch to E+2, and delete rows ≥ `b`
   (`KvDelRange`, one operation). The split is done at this point.
5. **Update the root:** put R2's entry (asserting it absent), then clear
   R's `next`. Root entries' epochs are hints, and R's isn't rewritten.

- **A crash before step 3** abandons the split: the new primary deletes
  R2 (asserting it `pending`) and sets R `serving` at a new epoch. A crash
  after step 3 finishes steps 4 and 5. Why: a copy is cheap to repeat,
  and abandoning needs no record of how far the copy went. A job whose
  step 4 fails because its split was abandoned meanwhile deletes its R2,
  which nothing names (I260).
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
  1. Set the left range L to `absorbing` (the right one, N), then N to
     `merging` (into L), each at a new epoch; N only if it still starts
     where L ends. N's writes are held, as a frozen half is.
  2. Copy N's rows into L, each list asserting L still absorbs N. L's
     bounds still exclude them, so no reader sees them yet.
  3. Extend L, asserting its epoch: set `end` to N's `end`, `state` to
     `serving`, `merged` to N, and a new epoch.
  4. Update the root: delete N's entry (asserting its value).
  5. Delete N (asserting it `merging` into L), then clear L's `merged`.

  A crash before step 3 abandons the merge: the copied rows are deleted
  from L (`KvDelRange` past its `end`) and both ranges serve again at new
  epochs. A crash after step 3 finishes steps 4 and 5, from L's `merged`.
  N's own primary leaves N to L while L absorbs it or names it in
  `merged`; otherwise the merge was abandoned there, and N serves again.
  N is deleted only after the root no longer names it. Why: a client
  that finds a range missing reads the root again, which must then
  point elsewhere.
- **Taking up a stopped job.** A primary's sweep (every 10 s) takes up a
  range that shows a job, by its state, `next` or `merged`, at the same
  epoch as on the sweep before: finished or undone as above. Why: a
  job still under way changes the epoch within a sweep, and one that is
  taken up anyway fails its next step, which asserts what it left.
- **Leftover objects.** A range's primary looks, every quarter hour, at
  what names each range it holds: its table must exist (the root's `i`
  entry and the record under that name agree), and the root's entry at
  its start must name it, or its `parent` name it by `next` or as the
  range it splits into. A range in the middle of a job is left to the
  job. One named by nothing on every look for an hour (a crash while
  creating a table, an abandoned copy, a table whose delete stopped) is
  deleted, asserting its epoch, with the deleted table's root entries
  for it, and the table's `i` entry with its last range (I261). Why an
  hour, counted by the primary: no clock in the range, and a primary that
  restarts only starts counting again.
- **Root repair** is taking up a stopped job: a split stopped before its
  step 5 shows `next`, a merge before its step 4 shows `merged`.

### 28.6 The deletion queue

D192's collection records, in a form other users can share: a batch on a
row may also add an entry to the range's queue (`q` + a random 128-bit
id), naming objects to delete (pool, namespace, name). The range's
primary deletes them in the background, then removes each entry. A split
leaves the queue with the lower range; a merge moves the right range's
entries into the left one with its rows. Why: the entry is written
atomically with the change that orphans the objects, nothing is
scanned, and the primary that holds the queue drains it. Why random
ids, not sequence numbers: no counter to read before a batch, and a
merge's entries can't collide (I261).

- **At least once.** An object is deleted before its entry, so a primary
  that dies between leaves the entry, and the next deletes the object
  again: nothing. The objects named must be the batch's alone (S3's data
  objects have unique names): a name reused after its entry was written
  would be deleted.
- **Series.** An entry may name a prefix and a count: the objects
  prefix + 0 … prefix + (count − 1), numbers as big-endian u32. Why: a
  5 TiB S3 object has over a million chunks, one series per part (I262).
- **Deleting a range drains it first**: a table's delete and the
  leftover check delete what a range's queue names before the range.
  Why: the queue goes with the range object, and its objects would
  otherwise stay forever (I262).

### 28.7 The library and the daemon

- **`hyperstor-kv`**, a library on `hyperstor-client`:
  - open a store;
  - create, look up, list and delete tables;
  - `get` (a column), `row` (every column of a row), `scan` (rows from,
    to, with a prefix, a limit of rows, and where the next page starts);
  - `apply(row, batch)`: puts, deletes, assertions, and (stage 4) queue
    entries.

  It caches the root, routes by row, handles `Conflict` and `next`, and
  holds writes during a freeze. The S3 gateway (HS12) is its first user.
- **In `hyperstord`**, each primary watches the `kv` objects it holds for
  size, at most once a second per object written, and sweeps them all
  every 10 s. It runs splits, merges, leftover checks and queues, one
  job per range at a time and two per device (I260). These are the only background work in a storage
  daemon that writes to other PGs, through its client.

### 28.8 Build stages and tests

1. `AssertKv` in the engine and on the wire, and the table's bytes in
   `Lookup`. Tests: the engine's randomized test against its model
   covers the new assertion.
2. `hyperstor-kv` without splits: key encoding, the root, tables, rows,
   scans, the routing cache (built, I256). Tests: a randomized test
   against a sorted map, on an in-process cluster, through two stores
   whose caches go stale, with ranges split by hand so that reads and
   writes cross ranges, follow `next` and wait on a frozen half.
3. Splits and merges in `hyperstord` (built, I260). Tests: each job
   stopped after every step and taken up, and a table that splits and
   merges on its own under small limits while a node is killed and
   restarted, with jobs left half done for the primaries to take up.
   After each, the store must equal the model, every row must be in
   exactly one serving range, and no object may be left over.
4. The deletion queue (built, I261). Tests: queued objects deleted only
   after their batch commits, again after a primary that died between
   objects and entry, through primary failover, by a range a split made,
   and carried by a merge; leftover ranges deleted, live ones kept.
