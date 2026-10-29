# The client library

`hyperstor-client` is a Rust crate. It holds a copy of the cluster map,
keeps connections to storage nodes, splits each request at chunk
boundaries, sends the pieces in parallel to their primaries, and retries
when the map changes.

## Adding the dependency

The crate is not on crates.io yet. Depend on it from git, with the
reactor it runs on:

```toml
[dependencies]
hyperstor-client = { git = "https://github.com/CMKL-APEX/hyperstor" }
hyperstor-reactor = { git = "https://github.com/CMKL-APEX/hyperstor" }
```

Pin a revision (`rev = "..."`) while the API is changing.

## The reactor

The client runs on Hyperstor's own io_uring reactor, not on tokio or
another async runtime. `hyperstor_reactor::block_on` runs a future, and
everything it spawns, on the current thread. A `Client` belongs to the
thread that created it (it is not `Send`); use one client per thread.

## Writing and reading

```rust
use hyperstor_client::Client;
use hyperstor_reactor::block_on;

fn main() {
    let meta = "127.0.0.1:19000".parse().unwrap();
    block_on(async move {
        // The second argument identifies this client to the cluster.
        let client = Client::connect(meta, 1).await.expect("connect");

        let pool = 1;
        let volume: u128 = 0x1234;
        let data = vec![0xab; 64 * 1024];

        // Offsets and lengths are multiples of 4096.
        client.write(pool, volume, 0, data.clone()).await.expect("write");
        let back = client.read(pool, volume, 0, 64 * 1024).await.expect("read");
        assert_eq!(back, data);
    });
}
```

- `Client::connect(meta, id)` fetches the map from the map service at
  `meta`. With several map service members, use
  `Client::connect_members(vec![a, b, c], id)`: any member serves the
  map, and requests only the leader answers follow its redirects.
- `write(pool, volume, offset, data)` returns once every reachable copy
  of every chunk touched has the data (see
  [guarantees](guarantees.md)).
- `read(pool, volume, offset, len)` returns `len` bytes; blocks never
  written are zeros.
- `map()` returns the client's current copy of the cluster map, for
  example to list pools.

The full API is in the [client API reference](../reference/client-api.md).

## Volumes

`write` and `read` above address a *lineage* directly: nothing records
that it exists, and nothing stops two programs from writing to it at
once. Programs that share a cluster use **volumes**, which the cluster
keeps a record of, and which one client at a time attaches:

```rust
// Create a 1 GiB volume in pool 1, with a random id (or Some(id)).
let v = client.create_volume(1, 1 << 30, None, "scratch").await?;

// Attach it: take its lease. I/O goes through the handle.
let disk = client.attach(v.id).await?;
disk.write(0, vec![0xab; 4096]).await?;
let back = disk.read(0, 4096).await?;
disk.detach().await?;
```

- `create_volume(pool, size, id, name)` makes the volume's header; the
  size is a multiple of 4096. A retried create is recognised, not
  doubled. `volume(id)`, `list_volumes(pool)`, `expand_volume(id, size)`
  (volumes don't shrink) and `delete_volume(id)` manage them;
  `hyperstorctl` does the same for administrators.
- Ids are 128-bit, chosen by you (for example a disk's UUID) or drawn at
  random. Names are shown, not unique.

## Attaching a volume

`attach(id)` takes the volume's **lease**. The client renews it every
second in the background, and a lease not renewed for 4 s runs out.
While one client holds it, another's `attach` fails with `LeaseHeld`.

- `detach()` gives the lease up. A handle dropped without it stops
  renewing, and its lease runs out.
- `take_over(id)` attaches a volume whose holder has died or is cut off:
  it waits for the holder's lease to run out, **blocklists** that lease,
  then takes it. From then on every storage node refuses requests made
  under the old lease, so none of the old holder's writes can land after
  the new holder's. A holder that is still renewing is left alone
  (`LeaseHeld`).
- `preempt(id)` takes a volume from a live holder, for example one an
  operator moves: it blocklists the holder's lease first. Only that
  volume's lease is cut off: the old holder's other volumes go on, and it
  can attach the volume again later.
- `lease()` is the handle's lease identity, which every request through
  it carries.
- `set_lease_label(label)` sets a short label (up to 64 bytes) recorded
  with every lease the client takes, and `attach_as(id, how, label)` sets
  one lease's. `LeaseHeld` and a volume's `holder_label` give it, so
  whoever finds a volume held knows what holds it.
- **Delegates** act under a holder's lease from another client, such as
  a library inside a VM: `add_delegate()` on the holder's handle returns
  a new identity, and `Client::delegate(id, identity)` opens the volume
  with it there. A delegate renews nothing. It is cut off when the holder
  removes it (`remove_delegate`), detaches, or loses the lease to a
  take-over, and a snapshot waits for it to pause as for the holder.
- Once a handle's lease is lost, its I/O fails with `LeaseLost`, and
  `lost()` returns true.
- An attached volume can't be deleted (`VolumeAttached`); detach it
  first, or use `force_delete_volume(id)`, which preempts the holder.
- Read-only attachments (of snapshots, by several clients at once) are
  planned.

## Snapshots and clones

```rust
let snap = client.snapshot(v.id, "before-upgrade").await?;   // crash-consistent
let copy = client.clone_snapshot(v.id, snap.epoch, "copy", None).await?;
client.rollback(v.id, snap.epoch).await?;                    // detached volumes only
client.delete_snapshot(v.id, snap.epoch).await?;
```

- `snapshot(id, name)` takes a **crash-consistent** snapshot: the volume
  as it would be after a power cut at one instant. If a client has the
  volume attached, its writes are paused while the snapshot is cut, at
  most 2 s; past that the snapshot is abandoned (`Server` with code
  `Busy`), and you can try again. A snapshot is identified by its
  **epoch**; `volume(id)` lists them in `snapshots`.
- `snapshot_group(ids, name)` takes a snapshot of several volumes at one
  instant (a **consistency group**), for example a database's data and
  log volumes; `delete_group` removes them all.
- `clone_snapshot(id, epoch, name, new_id)` makes a new volume from a
  snapshot at once: it shares the snapshot's data, and writes to the
  clone go to its own copies.
- `rollback(id, epoch)` returns a detached volume to a snapshot. To the
  newest snapshot it is done in place and is instant.
- `flatten(id)` copies into a clone what it still reads from its parents,
  so they can be deleted.
- `delete_snapshot(id, epoch)` merges the snapshot's data into what
  still needs it, without copying blocks, and is refused
  (`SnapshotInUse`) while a clone or a rollback depends on it.
