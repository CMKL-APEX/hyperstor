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
returned as data. When the primary's copy is damaged, the primary reads
another copy, repairs its own and returns the good data. If no copy is
good, the read fails (`Server` with code `Checksum`); `Checksum` on its
own means the data was damaged on the way to the client.

## Volumes and leases

A volume attached through `attach`, `take_over` or `preempt` has one
writer at a time. After a take-over or preemption, the old holder is
refused by every storage node, so no write of its lands after the new
holder's first write. A write that failed with `LeaseLost` may have
landed or not.

## Snapshots

A snapshot holds every write acknowledged before it was cut, and none
issued after. A write in flight while it is cut is in it or not, as
after a power cut. A consistency group gives the same guarantee across
all its volumes at one instant. Writes to a clone never change its
snapshot, and writes to the volume never change its snapshots.

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
| `Checksum` | Data was corrupted on the way to the client | Retry the read; report it if it persists |
| `NoVolume(id)`, `VolumeExists(id)` | No volume has that id; one already does | Check the id |
| `Shrink { .. }`, `BadVolume(..)` | A smaller size, or a size or name that isn't allowed | Fix the request |
| `LeaseHeld { holder, delegates, remaining_ms, label }` | Another lease holds the volume; `label` says whose | Wait, or `take_over` if its holder is gone |
| `LeaseLost(id)` | This handle's lease was taken over, or this client was blocklisted | Stop using the handle; attach again if that's right |
| `Server { code: Fenced, .. }` | This handle's lease was blocklisted by a take-over or preemption; the handle is now lost | As `LeaseLost` |
| `VolumeAttached(id)` | A delete or rollback of an attached volume | Detach it first, or force the delete |
| `NoSnapshot(epoch)` | The volume has no snapshot at that epoch | Check `volume(id)`'s snapshots |
| `SnapshotInUse(epoch)` | A clone or a rollback still reads from that snapshot | Flatten or delete the clone first |
| `Refused(why)` | The cluster refused a volume or snapshot operation, for example an unknown group | Read `why` |
| `Server { code, .. }` | The storage node refused the request, for example `NoSpace`, `Io`, `Checksum` when no copy is good, or `Busy` when a snapshot couldn't pause the volume's writer in time | Depends on the code; report `Io` and `Checksum` to the administrator |
| `Exhausted(n, why)` | The client retried `n` times without success, for example while a failed node is being detected | Retry after a few seconds; `why` is the last error |
| `Rpc(..)` | A connection or protocol error the client did not retry, for example while fetching the map | Check that the map service is up and reachable, and that client and cluster run the same version |
