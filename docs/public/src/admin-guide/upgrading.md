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
