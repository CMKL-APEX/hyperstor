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
[cluster API](../reference/cluster-api.md), which `hyperstorctl`, the
management service and other management clients use. Add `--api-tls
DIR` to require client certificates; without it, keep the API on
loopback. The installer also gives it a local socket (`--api-socket`),
which root and the `hyperstor-admin` group use without a certificate,
so an expired or lost certificate never locks the administrator out.

## Leases and failure detection

| Option | Default | Effect |
|---|---|---|
| `--lease-ms` | 600 | How long a storage node may serve after its last heartbeat was answered |
| `--down-after-ms` | 800 | How long without a heartbeat before a node is marked down; at least 1.25 × the lease |
| `--down-out-secs` | 600 | How long a node stays down before its devices are marked out and its data is copied elsewhere |

A node marked down loses its roles: its placement groups get new
primaries and run with the other copies, and a node cut off from the map
service has already stopped serving, because its lease ran out first.
Nothing is copied until `--down-out-secs` pass, so a reboot moves no data
(see [Failures and recovery](failures.md)). Shorter times detect failures
sooner, at the cost of false alarms on a slow or busy network.

Storage nodes also watch each other: a node that two others report
silent is marked down even while its heartbeats arrive. A node marked
down is held down for at least 2 s, longer each time it goes down again
within 10 minutes (up to a minute), so a node that keeps going down and
up doesn't move its PGs back and forth; health then warns that it flaps
(see [Failures and recovery](failures.md#a-node-that-flaps)).

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
  members, one at a time. Hosts can also join with a token, and devices
  and pools be added and removed with `hyperstorctl`, with no restart
  (see [Managing a cluster](managing.md)).
- **Any member answers** `hyperstorctl` and the web UI. What only the
  leader knows (devices' space, when each node was last heard from,
  flapping nodes, scrub's findings) a member that doesn't lead asks the
  leader for, so the answers are the same wherever you ask.
- **Members talk over mutual TLS** with their `--api-tls` certificates,
  which must name each member's address. `hyperstor-install` sets this up
  when members join the first one with a token (see
  [Installation](installation.md#joining-hosts-with-tokens)).
- **Changing members** while the group runs, and **recovering a group
  that lost its majority**, are below: see
  [Adding and removing members](#adding-and-removing-members) and
  [Recovering a lost majority](#recovering-a-lost-majority).

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

## Adding and removing members

Members can be added and removed while the map service runs. Install the
new member first, with a `meta` token from the leader and a member list
that includes it, and start it; then add it on the leader:

```sh
# on the new host (member 4)
sudo hyperstor-install install --join 192.0.2.10:8850 --token-file token \
    --role meta --members "$M,4=192.0.2.13:8851/192.0.2.13:8849" --member-id 4
# on the leader
hyperstorctl member add --id 4 --raft 192.0.2.13:8851 --map 192.0.2.13:8849
hyperstorctl member list
```

The new member is a learner until it has the log, then a voter. `member
remove --id N` takes one out; fewer than three voters survive no failure,
so going below three needs `--force`. Replacing a member on another host
is add, then remove. Storage nodes keep the member list they were
installed with and are redirected to the leader by any member they
reach; installing them again gives them the new list.

## Recovering a lost majority

If most members are lost for good, the rest can serve the map but not
change it: nothing is marked down and no host joins. Force one survivor
alone, on its host, with every other member stopped:

```sh
sudo systemctl stop hyperstor-meta
echo "HS_ONCE=--force-alone CLUSTER_ID" | sudo tee /etc/hyperstor/meta-once.env
sudo systemctl start hyperstor-meta
hyperstorctl member list            # one member, leading
sudo rm /etc/hyperstor/meta-once.env
```

It refuses while another member answers, since two groups would then
split the cluster, and unless given the cluster's id. It revokes the
other members' certificates issued until then, so an old member that
comes back with its log is refused; reinstall those with fresh data
directories, join them with `meta` tokens, and add them. Map changes the
survivor hadn't received before the loss are lost: hosts joined in that
window join again.

