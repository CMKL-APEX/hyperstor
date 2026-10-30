# glidex integration

The `hyperstor` datastore driver and the changes glidex needs. Part of the hyperstor design; sections keep their numbers across documents, so `§N` in one document is the same section everywhere (see [README.md](README.md)).

## 17. glidex integration

This builds on storage.md S6 (`glidex-stord` and `VolumeBinding::Block`,
PR #114) and adds a `hyperstor` driver behind the `StorageDriver` trait
(storage.md §7).

| glidex | hyperstor |
|---|---|
| `Datastore{driver: hyperstor}` | parameters: the cluster API's endpoints, the pool, and the map service's addresses (from the join). The identity: a `mgmt` certificate glidex enrolls with a token and keeps sealed in its store, renewed through the API (D122). Topology `shared`. |
| `capabilities` | block, linked clones, expand, shared, snapshots (new). No shrink. |
| `probe` | the node runs `hyperstor-vhost` and reaches the metadata service: the node reaches the datastore. |
| `create_volume` | blank: create. From an image copy: clone of the image's golden snapshot. From a volume: snapshot, clone, then flatten in the background. |
| `delete_volume`, `expand_volume`, `capacity` | delete (to the trash), `ExpandVolume`; capacity: the pool's `total_bytes` and `free_bytes` in `GetStatus` (D156), usable bytes after redundancy, from a server with glidex's identity |
| `stage` / `unstage` | `attach` through the node's `hyperstor-vhost` control socket (D117), `preempt` only for a VM re-placed from a fenced node (D119, D124); its socket is `VolumeBinding::VhostUserBlk{socket}` (new). Stopping the VM detaches. |
| image copies (S9) | one per datastore: a volume holding the image (converted to raw, written with `hyperstorctl write-volume --sparse`, D123), plus its snapshot `golden`, named `glidex image <image> on <datastore>`. A deleted disk's clone in the trash still needs it, so glidex's leader deletes it once the clone is purged (glidex storage.md S24). |
| linked clones (S8) | clones of `golden` (D21), instant |
| extend-root | on the worker, without root (D123): read the partition table's region with `read-volume`, edit it as a file with `images::partition`'s shadow-file code, write it back with `write-volume` |
| disk move (S5) | not yet: glidex refuses moves onto and off hyperstor. Later through two bindings on the worker, as S6c does for `lvm` |
| volume lock (S16) | replaced by the lease (D11); the shim takes it at start through `hyperstor-vhost` |
| metering `disk.stored` | volume allocated bytes, head plus snapshots (not reported by hyperstor yet: unknown until then) |

- **Hypervisor arguments.** `VhostUserBlk` gives Cloud Hypervisor
  `--disk vhost_user=true,socket=…` and QEMU a `vhost-user-blk-pci`
  device, with the VM's memory backend shared (11-frontends.md §11.1). Command-line pin
  tests cover both; glidex's ignored boot tests boot a cloud image from
  a linked disk under each (glidex storage.md §11.2), which needed
  512-byte sectors in `hyperstor-vhost` (D126).
- **Disk snapshots in glidex** (API, Cedar, UI) are a separate glidex
  spec change. storage.md lists disk snapshots as out of scope today, and
  hyperstor is the first datastore that can do them.
- **Bare-metal GPU hosts** aren't glidex nodes. An operator exports a
  volume or snapshot to a host identity (15-operations.md §16) through `gxctl datastore`
  sub-commands that call the management API.

## 20. Edits to other documents (when implemented)

- **storage.md §11:** the `hyperstor` driver, `VolumeBinding::VhostUserBlk`,
  and the snapshot capability.
- **host-topology.md:** hyperstor's reserved cores, namespaces and NICs
  (15-operations.md §18.3).
- **clustering.md §18:** the `--fenced` question for VMs whose disks are
  all on hyperstor (06-metadata.md §9).
- **security.md:** hyperstor's identities and networks.
- **architecture.md:** hyperstor's processes.
- **New crate `glidex-auth`:** sessions, tokens, PAM through
  `glidex-authd`, and Cedar, extracted for hyperstor-mgmt (D52); and the
  UI's shared components.
- **web-ui.md, security.md §7.3:** a Storage section and a
  `storage-admin` role (19-administration.md §23.7, AD4).
- **tenancy.md:** the project-to-tenant-id mapping and tenant weights
  (D61, D62, 20-tenants.md §24); sites or regions of a glidex deployment
  as the federation (D68, 21-federation.md §25.6).
- **tenancy.md, credentials.md:** S3 access keys per project, and
  Cedar actions for buckets and objects (14-s3.md §14.2).
- **hypervisors.md:** a vhost-user-fs device for VMs mounting the
  hyperstor filesystem (00-filesystem.md); a virtio-vsock device for VMs with GPU-direct
  storage (D33); vhost-user-blk hot-plug, for the later CSI driver
  (11-frontends.md §11.4).
