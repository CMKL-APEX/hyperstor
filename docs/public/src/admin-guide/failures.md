# Failures and recovery

## A storage node fails

1. The node stops sending heartbeats. Clients' requests to it fail, and
   the client library retries.
2. After `--down-after-ms` (0.8 s by default) the map service marks the
   node down. Each placement group the node held goes on with the copies
   left, if at least the pool's `min` remain; a new primary takes over
   where the failed node was primary. Before it serves, the new primary
   makes the other copies match its own, so a write the failed node
   never confirmed to its client is kept by every copy or by none.
3. Writes continue with fewer copies (the PGs are `degraded`). Nothing is
   copied yet: the node keeps its place, so a short reboot moves no data.
4. If the node is still down after `hyperstor-meta --down-out-secs` (10
   minutes by default), its devices are marked **out**. Where the pool's
   failure domain allows, their PGs are placed on other devices, and each
   primary backfills the new copy (copies every object of the PG to it)
   while writes go on. Once full, it counts as a copy. Where no other
   domain is free (three nodes and three copies by `host`), the PGs stay
   degraded until the node returns.

If every copy of a PG is down, the PG stays down, and requests for its
data fail until one of its nodes returns. It is never reset to empty.

## A storage node returns

When the node starts again and sends heartbeats, the map service marks it
up, and marks its devices in again if they were marked out while it was
down. Placement wants it back for the PGs it held. For each of them, the
current primary **backfills** it: it compares what the node holds with
its own copy, object by object, and sends only the objects that differ
(deleting those it no longer has), since the node may have missed
writes. Only then does the node count as a full copy again, and any copy
placed elsewhere meanwhile is dropped.

So a node that was down briefly receives about what was written while it
was away. A new or empty node receives all of its PGs' data: plan for
that traffic when you add or replace one.

A device you took out yourself (`hyperstorctl out`) stays out until you
bring it back in.

## A replica is slow or unreachable

A primary that gets no answer from one of a PG's copies within 2 s, or
can't connect to it at all, reports it to the map service. The map
service takes that copy out of that one PG, as long as the pool's `min`
copies remain, and the primary backfills it back once it answers again.
The node itself stays up, and its other PGs are not affected. This also
covers a network cut between two storage nodes only, which the map
service can't see from heartbeats.

The client sees `Unavailable` for the write that timed out, and retries.
A slow *primary* is not handled yet: its clients wait for it.

## A device fails

A device that returns an I/O error is **failed**: its node answers every
request for it with `Unavailable` (clients retry) and reports it to the
map service, which marks it failed and out and places its PGs elsewhere
at once, without waiting for `--down-out-secs`. The node's other devices
go on serving. One I/O error is enough: there is no error-rate threshold
yet.

If the device is broken, replace it and start the node again (the new
device is formatted), then bring it back in with `hyperstorctl in
--device D`: its PGs are backfilled.

## Damaged data

Every 4 KiB block carries a checksum. When a primary reads a block whose
checksum doesn't match, it fetches the object from another copy,
replaces its own copy and answers the read with the good data. A damaged
block is never returned to a client.

## Scrub

Every storage node compares, once a day by default (`hyperstord
--scrub-hours`), the copies of each placement group it leads. A copy
that differs from the others, when the rest agree, is replaced from the
majority's copy, the primary's own included. Once a week (and on the
first scrub after a start) the scrub is **deep**: every copy is read in
full and each block's checksum checked, at a limited rate, so damage that
no client has read yet is found and replaced from the other copies
(`kind="damaged"`). When no majority agrees, nothing is changed: the
object is counted under `hyperstor_scrub_objects_total{kind="unresolved"}`
and logged, and `hyperstorctl status` warns with the PG
(`PG(s) inconsistent`), for an operator to look at.

## A storage node is cut off from the map service

A node that can reach clients but not the map service keeps serving until
its map lease runs out (`--lease-ms`, 0.6 s by default), then stops. The
map service waits longer than the lease before giving its PGs to other
nodes, so no acknowledged write is lost and two nodes never serve the
same PG.

## A client holding a volume fails

A volume is attached by one client at a time, which renews its lease
every second. If that client dies or is cut off, another client takes
the volume over: it waits for the old lease to run out (up to 4 s) and
**blocklists** the old client, so every storage node refuses it from then
on, even if it comes back. See
[the client library](../user-guide/client-library.md#attaching-a-volume).

## The map service fails

See [The map service](map-service.md): with three or five members, the
others elect a new leader within about half a second and I/O pauses for
about one lease. A single member is a single point of failure: the
cluster stops serving within a lease period, and resumes when it is back.

## Not handled yet

- A slow primary.

See [Status](../status.md).
