# Failures and recovery

## A storage node fails

1. The node stops sending heartbeats to the map service and answering
   its peers' pings. Clients' requests to it fail, and the client
   library retries.
2. After `--down-after-ms` (0.8 s by default) without a heartbeat, or
   once two other nodes report it silent, the map service marks the
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

A node marked down stays down for at least 2 s, and longer if it keeps
going down and up (see [A node that flaps](#a-node-that-flaps)).

So a node that was down briefly receives about what was written while it
was away. A new or empty node receives all of its PGs' data: plan for
that traffic when you add or replace one.

A device you took out yourself (`hyperstorctl out`) stays out until you
bring it back in.

## A node that flaps

A node whose thread is held (a stalled device, a host short of CPU or
memory) can miss its peers' pings for long enough to be reported, then
answer again: it goes down and up. Each time is costly, since its PGs
change primaries and are refilled. So the map service holds a node that
went down: 2 s the first time, doubled for each further time within 10
minutes, up to a minute; ten quiet minutes reset it. Health warns
`node N flapping (K transitions in 10 min)`. Look at that node's log for
what holds it (see [Health](managing.md#health)).

## A storage node is cut off from its peers

Storage nodes ping each other several times a second, over UDP (TCP for
a peer that doesn't answer over UDP). A node silent for about 300 ms is
reported to the map service. A node reported by two others is marked
down even while its heartbeats still reach the map service: it can't
serve its PGs' copies if its peers can't reach it. One report alone
marks nothing down, and a node not heard from since the map service
started is not marked down by reports: it may still be opening its
devices.

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
yet. A device that takes more than 30 s (`hyperstord --io-timeout-secs`)
to write and flush a commit is failed the same way, and its node's log says why (`the device took over
30000 ms to write a commit`); before, a hung device held its node's
thread, and the whole node went down. A read that waits more than 2 s
for its device (`--read-timeout-ms`) is served from another copy, and
counted in `hyperstor_read_timeouts_total`; three in a row fail the
device the same way (`3 reads in a row took over 2000 ms`).

If the device is broken, replace it, format the new one (a block device
with `hyperstor-install format-device`; a file is formatted by the node)
and start the node again, then bring it back in with `hyperstorctl in
--device D`: its PGs are backfilled. To retire a device instead, remove
it (see [Removing a device](pools-and-devices.md#removing-a-device)).

## A device fills up

Past the `full` threshold (95 % by default) a device refuses new data,
and its pools are `full`: new volumes, clones and snapshots in them are
refused. Overwrites, deletes and discards still work, so space can
always be freed. A VM's disk holds a write refused for space for up to a
minute before the guest sees an I/O error. See
[Full devices](pools-and-devices.md#full-devices).

## Cache pools

A cache pool keeps one copy by default and doesn't recover a lost one:
when a device fails, its cache PGs start empty elsewhere, and clients
recompute what they miss. When a node goes down, its cache PGs wait for
it for the pool's grace (60 s by default): back in time, it keeps their
data. Meanwhile reads of them miss (a volume reads zeros there), and a
write to one moves it at once to another device, empty. Health warns
while PGs wait.

## Damaged data

Every 4 KiB block carries a checksum. When a primary reads a block whose
checksum doesn't match, it fetches the object from another copy,
replaces its own copy and answers the read with the good data. A damaged
block is never returned to a client.

## Scrub

Every storage node compares, once a day by default, the copies of each
placement group it leads. Each pool has its own scrub settings, changed
with `hyperstorctl scrub set` (see [Scrub settings](#scrub-settings)). A copy
that differs from the others, when the rest agree, is replaced from the
majority's copy, the primary's own included. Once a week, and the first
time a PG is scrubbed, the scrub is **deep**: every copy is read in
full and each block's checksum checked, at a limited rate, so damage that
no client has read yet is found and replaced from the other copies
(`kind="damaged"`). When no majority agrees, nothing is changed: the
object is counted under `hyperstor_scrub_objects_total{kind="unresolved"}`
and logged, and `hyperstorctl status` warns with the PG
(`PG(s) inconsistent`), for an operator to look at.

Scrub also checks that copies hold the same blocks. Before the fix for
issue #158 (November 2026), recovery copied an object with zeros in its unwritten blocks.
For a clone or a snapshotted volume, those zeros hide the parent's or
the older version's data, which the unwritten blocks should read. Such a
copy has the same data digest as a good one. Scrub logs each object
whose copies differ only this way, with the device whose copy keeps its
holes, and counts it under `kind="holes"`, and by default replaces the other
copies with that one, whatever the majority (`kind="holes_repaired"`).
To see first how many objects would change, set the pool's hole repair
to `report`.

A PG whose holes scrub couldn't check is logged (`holes not checked`, with
the reason) and counted under
`hyperstor_scrub_pgs_total{kind="holes_unchecked"}`: a member didn't
answer, runs a version from before the check, or the PG changed while
checking. A pass that found no holes but counted unchecked PGs hasn't
looked at them; they are checked on a later pass.

### What scrub found, and when

The map service keeps, for each PG, what its last light and its last
deep scrub found and when they ran, replicated like the map, so a new
leader or a restart of the whole map service keeps them. A storage node
starts each pass at the PG scrubbed longest ago, and scrubs a PG deep when
its last deep scrub is a week old (with the defaults), so restarts neither
starve the last PGs nor scrub everything deep.

```
hyperstorctl scrub status [--pool P]
hyperstorctl scrub pgs --pool P [--overdue]
```

`scrub status` shows, per pool, how long since its PG scrubbed longest
ago was last scrubbed (`OLDEST`) and scrubbed deep (`DEEPEST`), the PGs
overdue, unchecked, damaged or inconsistent, and what the last week's
scrubs found. `scrub pgs` lists each PG's last light and deep scrub and
what the newer one found. Both are up to a minute behind: the leader
writes the records once a minute.

`hyperstorctl status` warns or fails for:

| Reason | Level | When |
|---|---|---|
| `scrub overdue on N PG(s)` | warn | No scrub in twice the pool's interval, or no deep scrub in twice the deep interval (14 days with the defaults). A new PG counts from when it was made |
| `holes unchecked in the last two scrubs of N PG(s)` | warn | The hole check was skipped twice running (see above) |
| `N PG(s) damaged, with no good copy to repair from` | error | The PG's last scrub left damaged objects on the primary that no other copy replaced |
| `scrub reduced` | warn | The pool's settings are weaker than the defaults (below) |
| `PG(s) inconsistent` | warn | Copies differ with no majority (above) |

Each names the pool and up to ten PGs. The next scrub of the PG that
finds it clean clears it. In the web UI, the Overview's reasons link to
the pool's PGs in the grid's Scrub view.

### Scrubbing now, and after recovery

To check a pool without waiting for its turn, after a fix or an
incident:

```
hyperstorctl scrub start --pool 1 --deep --wait 3600
hyperstorctl scrub start --pool 1 --pg 7 --pg 12
```

It is an operation: `op list` shows how many PGs are done, `op cancel`
stops it, and the audit log records who started it. The PGs' primaries
scrub them one at a time, a deep one at the pool's deep rate, while the
regular schedule goes on.

Only the map service's leader starts operations. Asked of another
member, `scrub start` goes on to the leader's API, with the
certificates of `--tls` or of `hyperstorctl.conf`, and says so. It
finds that API in `member list`: each member records its own when it
begins to lead. When it can't go there (no certificates, over the local
socket, or the leader hasn't recorded its API yet, as with members from
before this), it exits with status 3 and says what to run instead.

In the web UI, a pool's **Scrub** card (Pools, then the pool's PGs)
shows its settings, how long since its PGs were last scrubbed, light and
deep, and the PGs overdue, with **Scrub now** and **Scrub deep now**
buttons and **Change** for the settings. The PG grid's **Scrub** view
shades each PG by the age of its last deep scrub: green when fresh,
amber as it falls due, red once overdue, with a dark centre for a PG
whose last scrubs found something. Click a PG for its last light and
deep scrub.

After a node comes back, a device is replaced, or anything else changes
which devices hold a PG, the PG is scrubbed light, holes checked, about a
minute after it is clean again: recovery copies data, and a copy made
wrong is best found while the copy it came from is still there. The
leader keeps that list in memory; after a change of leader, PGs already
clean wait for their turn.


### Scrub settings

Each pool has four, in the map, so every node follows the same ones and
a change needs no restart:

| Setting | Default | `scrub set` flag | Meaning |
|---|---|---|---|
| hours | 24 | `--hours` | Every placement group is compared once in this many hours; 0 turns scrub off |
| deep every | 7 | `--deep-every` | A PG is scrubbed deep once every N scrubs, and the first time, reading every block |
| hole repair | `repair` | `--hole-repair` | Replace copies holding zeros where another has holes, or only `report` them |
| deep rate | 64 MiB/s | `--deep-rate` | The most a deep scrub reads per device |

```
hyperstorctl scrub set --pool 1 --hours 12
hyperstorctl scrub set --pool 3 --deep-rate 32
```

Scrub turned off loses nothing at once, which is what makes it
dangerous: damage then goes unseen until a read needs the copy. So a
change that checks less than the defaults (scrub off, more than 168
hours, deep every more than 7 passes, holes only reported, a deep rate
under 32 MiB/s) is refused unless you give `--reason`, which goes in the
audit log; through the web UI or `--mgmt`, it also needs a fresh login.
While a pool's scrub is weaker than the defaults, `hyperstorctl status`
warns (`scrub reduced`), naming the pool and what is weaker.

`hyperstord`'s `--scrub-hours` and `--hole-repair`, from earlier
versions, are ignored with a line in the log.
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
**blocklists** the old lease, so every storage node refuses the old
client's requests for that volume from then on, even if it comes back.
Each attached volume has a lease of its own, so the old client's other
volumes are not affected. See
[the client library](../user-guide/client-library.md#attaching-a-volume).

The same holds for VM disks: when a VM moves to another host because its
old host failed, that host's `hyperstor-vhost` takes the volume over or
preempts the old one, and only that disk is fenced; the other VMs on the
old host keep theirs. See [VM disks](managing.md#vm-disks).

## The map service fails

See [The map service](map-service.md): with three or five members, the
others elect a new leader within about half a second and I/O pauses for
about one lease. A single member is a single point of failure: the
cluster stops serving within a lease period, and resumes when it is back.

## Not handled yet

- A slow primary.

See [Status](../status.md).
