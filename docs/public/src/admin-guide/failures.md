# Failures and recovery

## A storage node fails

1. The node stops sending heartbeats. Clients' requests to it fail, and
   the client library retries.
2. After `--down-after-ms` (0.8 s by default) the map service marks the
   node down. Each placement group the node held goes on with the copies
   left, if at least the pool's `min` remain; a new primary takes over
   where the failed node was primary.
3. Writes continue with fewer copies. Where the pool's failure domain
   allows another node to take the failed one's place, the map names a
   **stand-in**, and the primary backfills it (copies every object of the
   PG to it) while writes go on. Once full, the stand-in counts as a
   copy. Where no other domain is free (three nodes and three copies by
   `host`), the PG stays degraded until the node returns.

If every copy of a PG is down, the PG stays down, and requests for its
data fail until one of its nodes returns. It is never reset to empty.

## A storage node returns

When the node starts again and sends heartbeats, the map service marks it
up, and placement wants it back for the PGs it held. For each of them,
the current primary **backfills** it: it compares what the node holds
with its own copy, object by object, and sends only the objects that
differ (deleting those it no longer has), since the node may have missed
writes. Only then does the node count as a full copy again, and any
stand-in is dropped.

So a node that was down briefly receives about what was written while it
was away. A new or empty node receives all of its PGs' data: plan for
that traffic when you add or replace one.

## Scrub

Every storage node compares, once a day by default (`hyperstord
--scrub-hours`), the copies of each placement group it leads. A copy
that differs from the others, when the rest agree, is replaced from the
node's own copy. When no majority agrees, nothing is changed: the
object is counted under `hyperstor_scrub_objects_total{kind="unresolved"}`
and logged, for an operator to look at.

## A storage node is cut off from the map service

A node that can reach clients but not the map service keeps serving until
its map lease runs out (`--lease-ms`, 0.6 s by default), then stops. The
map service waits longer than the lease before giving its PGs to other
nodes, so no acknowledged write is lost and two nodes never serve the
same PG.

## The map service fails

See [The map service](map-service.md#one-map-service): the cluster stops
serving within a lease period, and resumes when the map service is back.

## A device fails

A device that returns an I/O error refuses every later change until it is
opened again, by restarting its node. If the device is broken, replace it
and start the node again: the new device is formatted, and its PGs are
backfilled. A block whose checksum doesn't match is never returned to a
client.

## Not handled yet

- Taking a node out for good (marking it *out*) without editing the
  cluster file and restarting the map service.
- Scrubbing: reading all copies in the background to find silent
  corruption.
- Recovery that copies only the writes a node missed.

See [Status](../status.md).
