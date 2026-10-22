# The map service

`hyperstor-meta` holds the cluster map, answers map requests from
storage nodes and clients, detects failed nodes from their heartbeats,
and grants **map leases**: a storage node serves I/O only while its lease
is current.

## Starting it

```sh
hyperstor-meta --listen 192.0.2.10:9000 --cluster /etc/hyperstor/cluster.txt
```

It prints the size of the map it loaded, then runs until stopped. All
options are in the [reference](../reference/hyperstor-meta.md).

With `--api 127.0.0.1:8850` it also serves the
[cluster API](../reference/cluster-api.md), which `hyperstorctl` uses to
report health and take devices out and in. Add `--api-tls DIR` to
require client certificates; without it, keep the API on loopback.

## Leases and failure detection

| Option | Default | Effect |
|---|---|---|
| `--lease-ms` | 600 | How long a storage node may serve after its last heartbeat was answered |
| `--down-after-ms` | 800 | How long without a heartbeat before a node is marked down; at least 1.25 × the lease |

A node marked down loses its roles: its placement groups go to other
nodes, and a node cut off from the map service has already stopped
serving, because its lease ran out first. Shorter times detect failures
sooner, at the cost of false alarms on a slow or busy network.

## Several members

Run the map service as a group of **three members** (five for large
clusters), each on its own host, so that it keeps working when one is
down (two, with five). Each member keeps the map in a store of its own
(`--data`), and they replicate it with Raft:

```sh
# On each of the three hosts, with its own --id and the same --member list:
hyperstor-meta --listen 192.0.2.10:9000 --cluster /etc/hyperstor/cluster.txt \
  --data /var/lib/hyperstor/meta --id 1 \
  --member 1=192.0.2.10:8851/192.0.2.10:9000 \
  --member 2=192.0.2.11:8851/192.0.2.11:9000 \
  --member 3=192.0.2.12:8851/192.0.2.12:9000 \
  --api-tls /etc/hyperstor/pki
```

Give storage nodes and clients every member (`hyperstord --meta
192.0.2.10:9000,192.0.2.11:9000,192.0.2.12:9000`). Any member serves the
map; the **leader** also grants leases, watches heartbeats and changes the
map, and the others send nodes to it.

- **When the leader fails,** the others elect a new one within about half
  a second. Storage nodes keep serving on their leases meanwhile and get
  new ones from the new leader at once, so I/O pauses for about one lease
  at most. The new leader then waits until every lease the old one gave
  has run out before it marks any node down or moves any data.
- **The map survives restarts,** of one member or all of them: a member
  that comes back catches up with the others. The cluster file seeds the
  map the first time; after that, hosts and pools you add to the file
  (on every member) are added when a leader is next elected: restart the
  members, one at a time.
  Removing them goes through `hyperstorctl out` and `in`.
- **Members talk over mutual TLS** with their `--api-tls` certificates,
  which must name each member's address.

## One member

Without `--member`, the map service is one member: with `--data` it
keeps its map across restarts, and without it, it keeps the map in memory
only. A single member is a single point of failure:

- **While it is down,** clients can't connect, and storage nodes stop
  serving I/O once their leases run out. Run it on a reliable host and
  restart it automatically.
- **Without `--data`, it forgets.** On restart it reads the cluster file
  again and starts with every node up; it forgets which nodes were down
  and which were being refilled, so **restart it only while all storage
  nodes are up and no recovery is running** (see
  [Failures and recovery](failures.md)).

## Backups and restoring

Installed map services keep backups of their store in
`/var/lib/hyperstor/meta-backups`: the leader writes one when the map,
the join tokens or the certificate lists have changed, at most every 5
minutes, and keeps the last 24. Take one at any time, to keep off the
cluster:

```sh
hyperstorctl map backup --out map.hsbackup
```

A backup holds what the map service holds, not data: volumes and their
contents live on the storage nodes. If every member's store is lost,
start one member from a backup, into an empty data directory, and the
storage nodes reconnect to it:

```sh
sudo systemctl stop hyperstor-meta
sudo mv /var/lib/hyperstor/meta /var/lib/hyperstor/meta.lost
# The backup must be readable by the service's user.
echo "HS_ONCE=--restore /var/lib/hyperstor/meta-backups/map-…hsbackup" \
    | sudo tee /etc/hyperstor/meta-once.env
sudo systemctl start hyperstor-meta
hyperstorctl status --wait 60
sudo rm /etc/hyperstor/meta-once.env
```

`/etc/hyperstor/meta-once.env` holds options for one start; the restored
store stays in the data directory, and the next start needs none.
It is a map service of one; with several
members, the others join it again. Changes made after the backup was
taken are lost: hosts joined since then join again.

