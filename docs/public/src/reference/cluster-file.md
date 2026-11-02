# Cluster file

The cluster file describes the cluster for the
[map service](../admin-guide/map-service.md). It is plain text, one
declaration per line. `#` starts a comment; blank lines are ignored.

```text
# A cluster of three nodes, two of them with two devices.
cluster 42
node 0 rack 0 192.0.2.20:9100 192.0.2.20:9101
node 1 rack 0 192.0.2.21:9100 192.0.2.21:9101
node 2 rack 1 192.0.2.22:9100
pool 1 vol replicated size 3 min 2 pgs 128 chunk 4194304 domain host
```

## `cluster`

```text
cluster ID
```

The cluster's number. Optional; `0` if absent.

## `node`

```text
node ID rack RACK ADDR [ADDR ...]
```

| Field | Meaning |
|---|---|
| `ID` | The node's number, as given to `hyperstord --node` |
| `RACK` | The rack number, for pools whose failure domain is `rack` |
| `ADDR` | `[DEV@]IP:PORT` of one device, in the order of the node's `--shard` options; `DEV@` names its device number |

Each address is one device. Devices get the numbers `ID × 1000 + i`, where
`i` counts the node's addresses from 0, unless the address names its
number (`1002@192.0.2.21:9102`): one of the node's, `ID × 1000` to
`ID × 1000 + 999`, and no two the same. A node whose device was removed
keeps the others' numbers this way.

## `network`

```text
network NAME control|data|both [trusted] subnet IP/LEN
network NAME control|data|both [trusted] ib PREFIX
```

A network the cluster's hosts are on, for hosts with several ports.

| Field | Meaning |
|---|---|
| `NAME` | The network's name, used by `endpoint` lines and `hyperstord --networks` |
| `control`, `data`, `both` | What it carries: the map service, the API and heartbeats (`control`); reads, writes, replication and recovery (`data`); or everything (`both`) |
| `trusted` | A dedicated storage network. Recorded for the data path's encryption, which will be on except on trusted networks; the data path isn't encrypted yet, on any network |
| `subnet IP/LEN` | Ports with an address in this subnet are on it |
| `ib PREFIX` | InfiniBand ports with this subnet prefix (four groups, `fe80:0000:0000:0000`) are on it |

A cluster without `network` lines uses each device's address for
everything, as before.

## `endpoint`

```text
endpoint DEV NETWORK IP:PORT
```

Device `DEV` is also served at `IP:PORT`, on network `NETWORK`. Give the
device's node and the network first. A device may have several
endpoints, on one network or several; its node's `hyperstord` listens on
each (`--endpoint`), and a node or client on that network reaches the
device there instead of at its address, spreading its requests over the
device's endpoints on that network.

```text
network mgmt control subnet 203.0.113.0/24
network ib data trusted ib fe80:0000:0000:0000
node 0 rack 0 203.0.113.10:9100
endpoint 0 ib 192.0.2.10:9100
endpoint 0 ib 192.0.2.11:9100
```

## `pool`

```text
pool ID NAME KIND [OPTION VALUE ...]
```

| Field | Meaning |
|---|---|
| `ID` | The pool's number, used by clients. Pool 0 is `.meta`, which the map service adds itself, for volume headers; a file can't declare it |
| `NAME` | A name, for people |
| `KIND` | `replicated`; `cache`, for data that can be recomputed (one copy unless `copies` says otherwise, nothing recovered when a device fails); or `ecK+M` (for example `ec4+2`) for erasure coding, which is not served yet |

| Option | Default | Meaning |
|---|---|---|
| `size` (or `copies`) | 3 (1 for `cache`, K+M for `ec`) | Copies of each chunk |
| `min` | 2 (1 for `cache`, K+1 for `ec`) | Copies that must be reachable to accept a write. The default doesn't follow `size`: with `size 2`, set `min 1` to keep writing while one copy is down |
| `pgs` | 128 | Placement groups; a power of two |
| `chunk` | 4194304 | Chunk size in bytes; a multiple of 4096 |
| `domain` | `host` | Failure domain: `device`, `host` or `rack` |
| `class` | any | Only use devices of these classes, comma-separated (`nvme,ssd`). A device's class is its kind (`nvme`, `ssd`, `hdd`, `file`) unless set with `hyperstorctl device class` |

## Errors

The map service refuses a file with a mistake and names the line:
an unknown declaration or pool option, a value that isn't a number, a bad
address, a `pgs` that isn't a power of two, a `domain` other than
`device`, `host` or `rack`, a network given twice or with a bad subnet,
or an endpoint for a device or network not given before it.
