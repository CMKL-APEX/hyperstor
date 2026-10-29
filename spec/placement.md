# Placement

Chunks to PGs, PGs to devices, rebalancing. Part of the hyperstor design; sections keep their numbers across documents, so `§N` in one document is the same section everywhere (see [README.md](README.md)).

## 5. Placement

### 5.1 Chunk to PG

- A volume offset maps to chunk index `offset / chunk_size`.
- The chunk's PG is `pg = hash64(lineage, chunk_index) mod pg_count` (D21).
  `pg_count` is a power of two, chosen when the pool is created (default:
  enough for about 100 PGs per device in the pool's device class).
- **Later:** doubling `pg_count` splits each PG in two. Because the hash
  is reduced by `mod 2^n`, the chunks of PG `p` go to `p` or
  `p + 2^n`, and the split happens on the devices that already hold them.

### 5.2 PG to devices

- **Rendezvous hashing with weights.** For each candidate, the score is
  `-ln(u) / weight`, where `u` is a uniform hash of `(pool, pg, candidate)`.
  The lowest score wins. The weight is the device's capacity.
- **Failure domains.** A pool names a level (`host`, `rack` or `site`;
  sites stay within 1 ms of each other, D63, federation.md §25.2). Placement
  first picks distinct domains at that level by the same scoring over the
  domain's total weight, then one device within each chosen domain.
- **Order.** The ranked list is the `up` set. For a replicated pool the
  first entry is the preferred primary. For an EC pool the position is
  the shard index (shards `0..k-1` hold data, `k..k+m-1` parity).
- **Device classes.** A pool can be limited to a class, so fast and
  dense media make different pools. Each device gets a class from its
  kind (`nvme`, `ssd`, `hdd`), which the operator can override with a
  label (`nvme-qlc`, `archive`). A pool never mixes hard disks with
  flash, because the slowest copy sets a write's latency (replication.md §8.1).
- `.meta` (metadata.md §4.2) is always placed on flash.

### 5.3 Rebalancing

- A map change recomputes `up` for the PGs it affects. While `up`
  differs from `acting`, the PG keeps serving from `acting` and the new
  members are backfilled (replication.md §8.5). When they have caught up, `acting`
  becomes `up` in one Raft write and a new `pg_epoch`.
- **Down and out.** A node that misses heartbeats for 10 s is marked
  `down`: its PGs get new primaries from the rest of `acting`, but data
  doesn't move. After `down_out_interval` (10 min by default) it is
  marked `out` and its PGs are re-placed. A short reboot moves no data.
  As built (implementation-notes.md I101): `hyperstor-meta
  --down-out-secs`; the node's devices are marked in again when it
  returns, unless an operator marked them out.
- Backfill and recovery are rate-limited per device and per node, and
  PGs below `min_size` go first.
  As built (implementation-notes.md I187): only a bound on what is in
  flight, not a rate: each device's backfills to one target keep at
  most 16 MiB of pushes on the wire together. No limit per node yet,
  and PGs start in map order, not below `min_size` first.
