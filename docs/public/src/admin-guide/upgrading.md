# Upgrading

A cluster is upgraded one host at a time while it keeps serving: each
host's services restart on the new build, and the cluster recovers
before the next host. This page covers an upgrade to v0.0.8 from v0.0.7
or a later build of `main`.

## Before you start

- **Health is `ok`.** `hyperstorctl status` shows `health ok`, every
  node up and every device in. An upgrade restarts nodes, so don't start
  one while PGs are still degraded or backfilling.
- **A backup of the map service's store.** `hyperstorctl map backup
  --out FILE`, as an administrator, writes the map, join tokens and the
  certificates issued and revoked; see
  [Backups and restoring](map-service.md#backups-and-restoring).
- **The new build.** Build the release binaries and the web UI as in
  [Building](installation.md#building), on a host whose glibc is no newer
  than the target hosts'. Copy the binaries to one directory on each
  host, with the UI's built files beside them as `hyperstor-ui`:
  `hyperstor-install` installs the binaries and the UI it finds next to
  itself, and reads every one before writing any, so a missing file
  changes nothing.
- **The previous build kept.** Keep a copy of `/usr/local/bin` from each
  host: going back is installing it the same way (see
  [Going back](#going-back)).

## The order of hosts

Upgrade, in turn:

1. hosts that run only storage nodes, `hyperstor-vhost` or
   `hyperstor-s3`;
2. map service members that don't lead;
3. the leader, last (`hyperstorctl member list` names it).

The leader goes last so that the group changes leader once, at the end,
rather than at each member. New builds read older builds' frames and
send frames older builds read, so nodes and members of both builds work
together while the upgrade goes on.

## On each host

```sh
# in the directory holding the new binaries and hyperstor-ui
sudo ./hyperstor-install install
# then, from any host with hyperstorctl, before the next host:
hyperstorctl status --wait 300
```

`install` with no options reuses the options saved by the host's last
run (`/etc/hyperstor/install.conf`), copies the binaries that changed,
and restarts only the services whose binaries or configuration changed:
the storage node, the map service member, `hyperstor-mgmt`,
`hyperstor-vhost` and `hyperstor-s3`, as the host runs them.

- **Storage nodes.** A node restarting is down for a few seconds. Its
  PGs go on with their other copies and it receives only what it missed
  when it comes back. Data moves only if it stays down past the
  map service's down-out interval.
- **Map service members.** A restarting member rejoins and catches up
  from the leader; while one of three is down the others keep a
  majority. Wait until `member list` on the leader shows it `in step`
  before the next.
- **VM disks.** `hyperstor-vhost` keeps its volumes' leases across a
  restart; hypervisors reconnect and their requests in flight run
  again. VMs see a pause, not an error.
- **The web UI** names the installed build in its footer.

## What v0.0.8 changes on a host

The installer takes care of these; nothing needs doing by hand:

- The udev rules that give the `hyperstor` user its block devices move
  to `/etc/udev/rules.d/61-hyperstor.rules`, after the system's
  `60-persistent-storage` rules, so they still match after a reboot.
  The old `60-hyperstor.rules` is removed.
- A storage-only host keeps the devices added to it with `--device`
  across runs.
- Hosts follow the map service's current members: storage hosts take
  the member list from the cluster API on every run, so a member added
  or removed since they were installed is picked up.
- `hyperstor-vhost` runs a reactor on each NUMA node, on the CPUs
  glidex reserves for the host; where glidex reserves none, on each
  node's first CPU, and the installer says so. `--vhost-cpus` names
  other CPUs, and is kept. The service restarts once; VMs keep their
  disks.
- Keys are written with their final modes from the start, and
  `/etc/hyperstor/pki` is set to `0751`, so other users can't list it.
  Each service reaches its own files by name, as before.

## What clients and operators notice

- **S3 clients.** As in S3, every `x-amz-*` header a request carries
  must be signed, except `x-amz-content-sha256`; otherwise the answer is
  403 `AccessDenied`. The AWS SDKs and CLI sign theirs. A tool that adds
  `x-amz-*` headers to a presigned URL's request, beyond those the URL
  signs, gets 403.
- **Logins to the web UI.** After 10 failed logins from one address in
  5 minutes, logins from that address are refused for the rest of the
  window (`rate_limited`). Behind a proxy or a tunnel, install mgmt with
  `--mgmt-trusted-proxies` or `--mgmt-proxy-ca` so that it sees each
  person's address; otherwise everyone behind the proxy shares the
  proxy's (see [Behind a proxy](../reference/hyperstor-mgmt.md#behind-a-proxy)).
- **Connections to mgmt and the S3 gateway.** Each has 10 s to send a
  request's headers, and a kept connection with no request closes after
  2 minutes. Browsers and SDKs open a new one. mgmt takes request
  bodies up to 256 KiB from anyone and up to 4 MiB with a session or API
  token; larger ones get `413`.
- **VM disks.** Requests of up to 1 GiB are served, 8 MiB at a time, so
  a VM's firmware can load large files (such as a unified kernel image)
  in one request.
- **Volume clients** wait up to 30 s for a PG that can't serve yet (a
  split, a primary moving) instead of failing after about two seconds;
  see [Guarantees](../user-guide/guarantees.md#errors).
- **S3 gateways** get only the keys of their realm. A gateway serves a
  realm other than `default` once bound to it: the installer binds a
  map service host's gateway itself, and a gateway that joined with an
  `s3` token needs `hyperstorctl realm host --host NAME --realm R` once
  (its install prints the command). Gateways of `default` need nothing.
- **Map service members** added with `hyperstorctl member add` vote
  only once they have caught up with the leader's log (see
  [Adding and removing members](map-service.md#adding-and-removing-members)).

## Converting devices to v0.0.8's format

v0.0.8 formats new devices in a new shard format and opens devices of
the old one as they are. Once **every** node runs v0.0.8 and the
cluster is healthy, convert the old ones:

```sh
hyperstorctl nodes                  # FORMAT: 1 for the old format, 2 for the new
hyperstorctl device upgrade --all   # or --device ID
```

Each node converts its devices the next time it starts: restart the
storage nodes one at a time, as for the upgrade. Converting is
**one-way**: builds before v0.0.8 refuse a converted device. Convert
only once you won't go back; the details are in
[Pools and devices](pools-and-devices.md#converting-devices-to-v008s-format).

## Going back

Until devices are converted, going back to the previous build is the
same procedure with the old binaries: install them on each host in the
same order. After converting, a node of an earlier build refuses the
converted devices.

## When something goes wrong

- **A node doesn't come back up.** `journalctl -u hyperstord@N` on its
  host says why. Install the previous build on that host to bring it
  back, then look into it before going on.
- **A node refuses a device** with `the device has format version N,
  newer than this build reads`: an older build met a converted device.
  Install v0.0.8 or later on that host.
- **Health stays degraded** after a host: wait for recovery to end
  (`hyperstorctl status --wait`) before the next host; with two copies
  down at once, PGs of pools whose `min` is 2 stop taking writes.

A rolling upgrade was rehearsed on a three-host test cluster from a
build made just after v0.0.7, and v0.0.8's changes were rolled out this
way to a four-host pilot; CI doesn't test upgrades between releases
yet.
