# Installation

There are no packages yet: build the binaries from source and copy them
to each machine.

## Requirements

- **Linux** on x86-64 (other architectures are untested), with io_uring
  enabled. `hyperstor-install check` fails below Linux 5.19 and warns
  below 6.1, which is what Hyperstor is tested on. Some distributions
  turn io_uring off with the `kernel.io_uring_disabled` sysctl; it must
  be `0`.
- **Rust** 1.88 or later, with cargo.
- For storage devices: a filesystem that supports `O_DIRECT` for device
  files, or whole block devices.
- For the web UI: [bun](https://bun.sh), to build it (optional; without
  it, the management service serves a simpler built-in dashboard).
- TCP connectivity between all storage nodes, and from clients to the map
  service and to every storage node; UDP too between storage nodes (see
  [Planning a cluster](planning.md#network)). See the security note in
  [Running a cluster](index.md).

## Building

```sh
git clone https://github.com/CMKL-APEX/hyperstor
cd hyperstor
cargo build --release
```

| Binary | Install on | Purpose |
|---|---|---|
| `target/release/hyperstor-meta` | the map service host | [map service](map-service.md) |
| `target/release/hyperstord` | every storage node | [storage node](storage-nodes.md) |
| `target/release/hyperstor-bench` | any host, optional | [load generator](benchmarking.md) |
| `target/release/hyperstorctl` | any host that reaches the cluster API | [managing a cluster](managing.md) |
| `target/release/hyperstor-mgmt`, `hyperstor-authd` | the map service host | [management service](../reference/hyperstor-mgmt.md) and its [password helper](../reference/hyperstor-authd.md) |
| `target/release/hyperstor-vhost` | hosts that run VMs | [VM disks](../reference/hyperstor-vhost.md) |
| `target/release/hyperstor-ublk` | any host, optional | [volumes as block devices](../reference/hyperstor-ublk.md) |
| `target/release/hyperstor-install` | run from the build directory | [installer](../reference/hyperstor-install.md) |

Build once, on a machine with the same CPU architecture and a C library
no newer than the target hosts', and copy the binaries.
`hyperstor-install` installs the binaries found beside it.

The web UI is built separately, and installed with `--ui`:

```sh
(cd crates/hyperstor-mgmt/ui && bun install && bun run build)   # → dist/
sudo target/release/hyperstor-install install --ui crates/hyperstor-mgmt/ui/dist
```

## Installing a trial cluster

`hyperstor-install` sets up a whole cluster on one Linux host: the map
service with its cluster API, three storage nodes on files, a pool with
three copies and a cache pool, systemd units, and `hyperstorctl`. It is for trying
Hyperstor and testing changes, not for data you want to keep.

```sh
cargo build --release
target/release/hyperstor-install check           # host checks, changes nothing
sudo target/release/hyperstor-install install --all-in-one
hyperstorctl status
hyperstorctl io-test
```

`check` reports each requirement as `ok`, `info`, `warn` or `FAIL`. On a
storage host whose devices have [endpoints](planning.md#several-networks),
it also warns about endpoint addresses that share a subnet with another
port and have no routing rule of their own (`routing`).
`install` runs the checks, copies the binaries to `/usr/local/bin`,
creates the `hyperstor` user, writes the configuration in
`/etc/hyperstor`, the cluster's certificates in `/etc/hyperstor/pki`,
and the units, and starts `hyperstor-meta`, `hyperstord@0` to
`hyperstord@2`, the management service `hyperstor-mgmt` and its
password helper `hyperstor-authd`. The cluster API (8850), the map
service (8849) and the storage nodes (9100 to 9102) listen on
`127.0.0.1`; the management service listens on every address over
HTTPS, on 8852 (`--mgmt-listen loopback` keeps it on `127.0.0.1`). Device
files go to `/var/lib/hyperstor` (4 GiB each, sparse).

The installer is convergent: run it again after rebuilding and it
updates what changed (binaries, configuration, units) and restarts the
services affected; with nothing changed it says so and does nothing.
The options of the last run are saved, so `sudo hyperstor-install
install` is enough later. `sudo hyperstor-install uninstall` stops and
removes the services and binaries and keeps the configuration and data.
All options are in the [reference](../reference/hyperstor-install.md).

The trial cluster also runs an S3 gateway,
[`hyperstor-s3`](../reference/hyperstor-s3.md), on port 8848 of every
address (`--s3-port P` for another, `--no-s3` for none), with new
buckets in the pool `vol`. Make a key and point an S3 client at it,
trusting the cluster's root certificate:

```sh
sudo hyperstorctl s3 key create --tenant 1      # prints the key's id and secret
aws --endpoint-url https://localhost:8848 --ca-bundle /etc/hyperstor/pki/root.crt s3 mb s3://first
```

`hyperstorctl` needs the administrator certificate, readable by root
and the `hyperstor-admin` group: run it with `sudo`, or join the group
(`sudo usermod -aG hyperstor-admin $USER`, then log in again).

To log in to the web UI, see [Logins and roles](managing.md#logins-and-roles).
Then see [Managing a cluster](managing.md).

## Joining hosts with tokens

Install the map service on one host first. It makes the cluster's
certificates and serves the cluster API on its own address, where the
other hosts reach it:

```sh
# on the map service's host (192.0.2.10); the cluster file may list no
# storage node at all: they join
sudo target/release/hyperstor-install install --cluster cluster.txt \
    --role meta --map 192.0.2.10:8849
```

Then, for each storage host, make a token on the map service's host and
use it on the new host:

```sh
# on the map service's host; umask 077 keeps the file private
(umask 077; sudo hyperstorctl token create --role node > token)
# copy `token` to the new host, readable by root only, then there:
sudo target/release/hyperstor-install install --join 192.0.2.10:8850 \
    --token-file token --role storage --address 192.0.2.21 --reactors 2
```

The new host pins the cluster by the root fingerprint in the token
before it sends anything, makes its own key (which never leaves it),
and gets a certificate for a new storage node number. The map service
adds the node to the map, down until its first heartbeat, and its PGs
start moving to it once it runs. The node gets one reactor per
`--reactors`, each on a file of `--size-gib` GiB in the data directory
and on its own port from `--node-port`, and its metrics on `--address`
at port 8853.

- A token is used once, expires after an hour (`--ttl SECS`, at most a
  week), and can be revoked with `hyperstorctl token revoke --id ID`;
  `hyperstorctl token list` shows them, never the secrets. Tokens are
  kept in the map service's replicated store, so they survive restarts;
  they are made and used through the leader.
- The token is never saved. Later runs of the installer, such as the
  daily timer's, need none; they renew the host's certificate through
  the API with the certificate itself.
- Installing a host again with the same addresses gets its node back.

**Several map service members.** Install the first member with the
whole member list, then join the others with `meta` tokens; they get
the cluster file, the root's certificate and the intermediate CA, so
every member signs with the same CA (the root's key stays on the first
host):

```sh
M=1=192.0.2.10:8851/192.0.2.10:8849,2=192.0.2.11:8851/192.0.2.11:8849,3=192.0.2.12:8851/192.0.2.12:8849
# first member
sudo hyperstor-install install --cluster cluster.txt --role meta --members $M --member-id 1
(umask 077; sudo hyperstorctl token create --role meta > token-2)
# second member (and likewise the third)
sudo hyperstor-install install --join 192.0.2.10:8850 --token-file token-2 \
    --role meta --members $M --member-id 2
```

A member's host can also run a storage node: list the nodes in the
cluster file, and give each member `--role both --node N` (the first
with `--cluster`, the others with `--join`); a joined member gets the
file, and so its node, from the first:

```sh
sudo hyperstor-install install --cluster cluster.txt --role both --node 0 \
    --members $M --member-id 1
sudo hyperstor-install install --join 192.0.2.10:8850 --token-file token-2 \
    --role both --node 1 --members $M --member-id 2
```

`--node` is checked before the token is sent, so a run refused for it
leaves the token usable.

Tokens are made and used through the leader; the other members refuse
with the leader's address. Before the members have formed a group, the
first member makes the `meta` tokens itself.

**Back up the root CA's key** on the first host, which alone has it
(see [hyperstor-install](../reference/hyperstor-install.md)). The backup
also holds the cluster key, `/etc/hyperstor/pki/cluster.key`: made with
the root, given to members as they join, it encrypts the S3 keys' secrets
the map service keeps, which are lost without it:

```sh
sudo hyperstor-install root-backup --out /media/usb/root.bak --passphrase-file pass
```

## Installing a cluster across hosts from one file

Without tokens, write the [cluster file](../reference/cluster-file.md)
once, copy it to every host, and install each host's part of it:

```sh
# on the map service's host (192.0.2.10)
sudo target/release/hyperstor-install install --cluster cluster.txt \
    --role meta --map 192.0.2.10:8849
# on each storage host, with its node number from the file
sudo target/release/hyperstor-install install --cluster cluster.txt \
    --role storage --node 0 --map 192.0.2.10:8849
```

A storage node gets one reactor for each address the cluster file lists
for it. Adding a host means editing every copy of the file. Storage
hosts installed this way get no certificate; joined ones do, though the
storage network doesn't use them yet: it has no authentication (see
[Running a cluster](index.md)).

## Hosts that run VMs

[`hyperstor-vhost`](../reference/hyperstor-vhost.md) serves volumes to
the VMs on a host, for glidex's `hyperstor` datastores. Add it to a
host's role, or install a host that only runs VMs:

```bash
sudo hyperstor-install install --vhost --vhost-user glidex          # with what the host runs already
sudo hyperstor-install install --role vhost --map 192.0.2.10:8849 --vhost-user glidex
```

`--vhost-user glidex` lets glidex's control plane attach volumes and its
hypervisors open their sockets. Restarting the service (an upgrade, say)
keeps the VMs' disks; `--no-vhost` and `uninstall` detach them first, running VMs too.
See [VM disks](managing.md#vm-disks).

## S3 gateways

[`hyperstor-s3`](../reference/hyperstor-s3.md) serves S3 over HTTPS.
It keeps nothing of its own, so run as many as you like behind a load
balancer. Add one to a map service host, or install a host that only
runs a gateway with an `s3` token:

```bash
sudo hyperstor-install install --s3                 # on a map service host
hyperstorctl token create --role s3 --name gw1      # then, on the gateway's host:
sudo hyperstor-install install --join 192.0.2.10:8850 --token-file FILE --role s3 --address 192.0.2.30
```

On a joined storage host, add it in a later run with a token of its own:
`--s3 --token-file FILE`. The gateway reads the access keys from the
cluster API with its own `s3` certificate and serves HTTPS with it,
unless `--s3-tls DIR` gives the site's certificate (`s3.crt`, `s3.key`).
Before buckets can be made, an administrator names their pools:

```bash
hyperstorctl s3 placement set --name default --data-pool bulk --index-pool flash
hyperstorctl s3 key create --tenant 1 --description backups
```

**Several gateways.** Gateways keep no state, so any of them may take
any request: run one per host and put DNS round-robin or a load
balancer in front. The balancer must leave the `Host` header and the
path as the client sent them, since SigV4 signs both: forward TCP, or
proxy HTTP keeping `Host`. Give every gateway the certificate of the
name clients use (`--s3-tls DIR`), or end TLS at the balancer and have
it reach the gateways over HTTPS, trusting `/etc/hyperstor/pki/root.crt`.
An unsigned `GET /` answers 403 from a gateway that is up.

**Virtual-hosted names.** Most SDKs default to `bucket.s3.example.com`
rather than `s3.example.com/bucket`. Give the gateway its domain with
`--s3-domain s3.example.com`, point a wildcard DNS record
`*.s3.example.com` at the gateways, and serve a certificate for both
`s3.example.com` and `*.s3.example.com` (`--s3-tls DIR`). The bucket is
always the signing key's tenant's, so one name serves every tenant. A
wildcard covers one label: buckets with dots in their names are reached
path-style, as on AWS.

**Behind a Cloudflare Tunnel** (not yet tried on real hardware). Run `cloudflared` on each gateway's
host, routing the public name to the gateway, and leave the `Host`
header alone (no `httpHostHeader`). Clients see Cloudflare's
certificate; `cloudflared` checks the gateway's against the cluster's
root:

```yaml
ingress:
  - hostname: s3.example.com
    service: https://localhost:8848
    originRequest:
      caPool: /etc/hyperstor/pki/root.crt
      originServerName: localhost
  - service: http_status:404
```

Cloudflare limits a request's body (100 MB on its Free and Pro plans),
so larger objects need multipart uploads (SDKs use them for large files
on their own), and it ends requests the gateway is silent on for 100 s.
Cloudflare sees the traffic in the clear at its edge: keep data that
must not leave your hands off a tunnel.

### Realms

A realm is a group of tenants that looks like a separate S3 service:
its own gateways (and so its own name and certificate), keys and pools,
on the same cluster and, if you like, the same drives. Every tenant is
in the realm `default` until you put it in another. Unlike Ceph's
realms, a Hyperstor realm is within one cluster.

```bash
hyperstorctl realm create --name acme --description "a customer"
hyperstorctl pool create --name acme-data --classes hdd    # or share a pool
hyperstorctl realm pool --pool 3 --realm acme              # only acme may use pool 3
hyperstorctl s3 placement set --realm acme --name default --data-pool acme-data --index-pool flash
hyperstorctl realm tenant --tenant 42 --realm acme
hyperstorctl token create --role s3 --name gw-acme1      # a gateway for acme, then
hyperstorctl realm host --host gw-acme1 --realm acme     # bind it to acme
sudo hyperstor-install install --join 192.0.2.10:8850 --token-file FILE --role s3 --s3-realm acme
```

- A gateway serves one realm: it knows only the keys of that realm's
  tenants, so another realm's key is refused as unknown.
- A gateway reads a realm's keys only if it is bound to that realm by
  the name in its certificate: the token's name for one that joined
  with an `s3` token, the member's number for one installed on a map
  service host (`hyperstorctl cert list` shows those issued at a join). A gateway bound
  to nothing reads `default`'s keys alone, so a cluster without realms
  needs no binding. Until it is bound, a gateway for another realm logs
  `isn't bound to realm` and tries again every few seconds.
- A realm's placements may name only pools it may use. A pool with no
  list (`realm pool --realm any`) is open to every realm.
- Bucket names are per tenant: two tenants may both have `photos`, and
  another tenant's bucket is never found.
- A host is unbound until bound: `hyperstorctl realm host --host hv-1
  --realm acme` lets hv-1's `hyperstor-vhost` and `hyperstor-ublk`
  attach only acme's tenants' volumes (add `default` for the others).
  Hosts are named by `--host`, their hostname unless set. Until the data
  path has mutual TLS, a host could give another name: a binding stops
  mistakes, not a hostile host.
- Realms share the drives' time: a realm that must not feel another's
  load needs pools on devices of its own.
- A tenant moves to another realm only if the new realm may use every
  pool its buckets and volumes are in, and a pool is taken from a realm
  only if none of its tenants' buckets or volumes is in it. Volumes
  belong to a tenant when made with `create-volume --tenant N`, and only
  in pools its realm may use; clones take their volume's tenant.

## Running as services by hand

Without the installer, a minimal unit for a storage node:

```ini
[Unit]
Description=Hyperstor storage node
After=network-online.target
Wants=network-online.target

[Service]
User=hyperstor
ExecStart=/usr/local/bin/hyperstord --node 0 --meta 192.0.2.10:9000 \
    --shard 192.0.2.20:9100=/var/lib/hyperstor/dev0.img:100
Restart=on-failure
LimitMEMLOCK=infinity

[Install]
WantedBy=multi-user.target
```

Run the services as an unprivileged user that owns the device files (or
has read and write access to the block devices). The map service needs no
privileges at all. `hyperstor-install install --all-in-one --root DIR
--no-start` writes its units and configuration under DIR without
installing anything, as a starting point.
