# Data guarantees and errors

## What a successful write means

When `write` returns `Ok`, every copy of the data that the pool's PGs
currently use has written it to its device and flushed it. Normally that
is `size` copies; while a node is down, it can be as few as the pool's
`min`. A write that returned `Ok` survives a crash or power loss of any
node holding a copy, and the failure of all but one of its copies.

A write that returns an error may have reached some copies and not
others. Write the same data again: writes to a range are idempotent.

## Order and concurrency

- A read that starts after a write returned sees that write.
- Two writes in flight at the same time to overlapping ranges land in an
  order you can't choose. Wait for one before starting the other if the
  order matters.
- A write that spans several chunks is not atomic across them: after a
  failure, some chunks may have the new data and others the old.

## Integrity

Every 4 KiB block carries a CRC32C checksum from the client to the
device and back. A block that doesn't match its checksum is never
returned as data: the read fails with `Checksum`.

## Errors

The client already retries what is worth retrying: a map change, a
primary that moved, a PG with no reachable primary, too few copies
reachable (`Unavailable`), a dropped connection. It refreshes its map
between attempts and gives up after 20 attempts, about two seconds. A
node failure takes a few seconds to detect (see the administrator's
[failures](../admin-guide/failures.md) page), so a program should be
ready to retry an `Exhausted` error itself.

The errors your program sees:

| Error | Meaning | What to do |
|---|---|---|
| `Unaligned` | Offset or length is not a multiple of 4096 | Fix the request |
| `NoPool(p)` | Pool `p` is not in the map | Check the pool number with the administrator |
| `NoPrimary(pool, pg)` | The map has no such PG | Check the pool's configuration with the administrator |
| `Checksum` | Data was corrupted on the way or on a device | Retry the read; report it if it persists |
| `Server { code, .. }` | The storage node refused the request, for example `NoSpace` or `Io` | Depends on the code; report `Io` to the administrator |
| `Exhausted(n, why)` | The client retried `n` times without success, for example while a failed node is being detected | Retry after a few seconds; `why` is the last error |
| `Rpc(..)` | A connection or protocol error the client did not retry, for example while fetching the map | Check that the map service is up and reachable, and that client and cluster run the same version |
