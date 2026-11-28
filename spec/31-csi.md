# Kubernetes CSI driver

Hyperstor volumes for Kubernetes through the Container Storage Interface:
first on nodes that reach the storage network, through ublk, then on
glidex VMs, through disks glidex hot-plugs. Part of the hyperstor design;
sections keep their numbers across documents, so `§N` in one document is
the same section everywhere (see [README.md](README.md)).

Status: **design.** D390–D404 are settled with the owner (05-decision-log.md
points 354–364, 2026-11-27). Host checks G70–G72 are to run.
Nothing is built. Milestone HS21 (17-milestones.md §21; #454).

## 34. Kubernetes CSI driver

### 34.1 What it is

- **One binary, `hyperstor-csi`,** in Rust (D391), run in two modes:
  `controller` (a Deployment, one replica active, with Kubernetes'
  standard sidecars: external-provisioner, -attacher, -snapshotter and
  -resizer) and `node` (a DaemonSet, with node-driver-registrar). It
  speaks CSI 1.11 over a Unix socket, as gRPC with tonic, on tokio: the
  control path's runtime (D3, D401).
- **Two kinds of Kubernetes cluster** (D390), in this order:
  1. **Nodes on the storage network** (stage C1–C5): the node plugin
     serves each volume as a ublk device itself, with hyperstor's own
     ublk on its reactor (D94), and the controller calls the cluster API.
     For an organization's own clusters: a node holds a `host`
     certificate bound to realms (D259, D375), and its pods' data crosses
     the storage network.
  2. **Kubernetes on glidex VMs** (stage C6): the controller asks glidex
     for a disk on a hyperstor datastore and for it to be hot-plugged into
     the VM as vhost-user-blk; the guest never reaches the storage
     network (11-frontends.md §11.4). For any tenant. Waits for glidex's
     hot-plug of disks into a running VM.
- **Access modes** (D392): `ReadWriteOnce` and `ReadWriteOncePod`, as a
  filesystem (ext4 or xfs) or a raw block device, and `ReadOnlyMany` from
  a snapshot. `ReadWriteMany` waits for the filesystem
  (29-hsfs.md §32).
- **Shipped** as a container image and a Helm chart (D396).

Why ublk first: every piece under it is built (the client library,
`hyperstor-ublk`, leases and forced takeovers, management clients bound
to realms, data-path certificates), and the Kubernetes-facing half
(idempotency, snapshots, expansion, csi-sanity, packaging) is the same
for both kinds of cluster.

### 34.2 Who calls whom (D393)

- **The controller is a management client** (19-administration.md
  §23.12): `hsctl token create --role mgmt --name k8s-<cluster>`,
  joined once, its certificate and key in a Kubernetes Secret, over
  mutual TLS to the cluster API (not mgmt). The operator binds it to the
  realms the Kubernetes cluster may use (`hsctl realm client`,
  D327); bound, it reaches only their tenants and projects
  (`outside_realm` otherwise). Each Kubernetes cluster gets its own.
- **A StorageClass names where volumes go:** `project` (a hyperstor
  project id, or `directoryProject`, glidex's id, as D311) and `pool`, and
  optionally `fsType` (`ext4` default, `xfs`). One cluster can serve
  several projects, one StorageClass each.
- **A node plugin** holds a `host` identity of its own (§34.6), names
  the node with `--host` semantics (D261: the certificate's name), and
  reaches the storage nodes directly. It never calls the cluster API.

### 34.3 Volumes

| CSI call | What it does |
|---|---|
| `CreateVolume` | `ClusterService.CreateVolume` with an id derived from the request's name (D397), the StorageClass's pool and project, the capacity rounded up to 4 KiB, and the name as the volume's name. A retry finds the volume by its id; one of another size is `ALREADY_EXISTS`. From a snapshot or a volume: §34.5. |
| `DeleteVolume` | `DeleteVolume`: to the trash, with the pool's grace (D400). A volume already gone is success. |
| `ControllerPublishVolume` | Checks the volume and returns `{"host": <node>}`; the attach itself happens on the node (§34.4). `attachRequired: true`, so Kubernetes' attach/detach controller keeps an RWO volume on one node at a time. |
| `ControllerUnpublishVolume` | Nothing to undo on the cluster: the node's unstage released the lease. |
| `ControllerExpandVolume` | `ExpandVolume`; `NodeExpansionRequired` for a filesystem. |
| `GetCapacity` | The pool's free space as `GetStatus` gives it, divided by its copies. |
| `ListVolumes` | The project's volumes the client reaches, paged. |

### 34.4 On the node (D394, D399)

- **`NodeStageVolume`:** attach the volume (its `rw` lease, labelled
  `csi:<node>`, D153) and serve it as `/dev/ublkbN` from the plugin's
  own reactor; for a filesystem, make it if the device is blank, then
  mount it at the staging path. A volume held by another node's
  plugin (`csi:<other>`) is preempted with a forced takeover (D24): Kubernetes
  only stages a volume after its attachment elsewhere is gone, or after
  it force-detached a lost node, and the blocklist round fences that
  node before this one writes (D399). A hold by anything else
  (`vhost:`, `hsctl`) is refused, `FAILED_PRECONDITION`, naming
  it.
- **`NodePublishVolume`:** a bind mount of the staging path (filesystem)
  or of the device (raw block), read-only where asked.
- **`NodeUnstageVolume`:** unmount, stop the device, release the lease.
- **`NodeExpandVolume`:** the device's new size from the header, then
  `resize2fs` or `xfs_growfs`.
- **`NodeGetVolumeStats`:** the filesystem's use and inodes, or the
  device's size, and abnormal when the device's server has failed.
- **Restarts keep the devices (D394).** Devices are added with ublk's
  user recovery (`UBLK_F_USER_RECOVERY`): when the plugin stops, the
  kernel holds the devices' I/O; the new plugin finds its devices (a state
  file under its host path, as `hyperstor-vhost`'s, D143), takes each
  lease back from its old client instance, and restarts each device's
  queues. Pods see a pause, not an error. G70.
- **The plugin runs privileged**, with the host's `/dev`, `/sys` and the
  kubelet's directory, as every block CSI node plugin does.

### 34.5 Snapshots, clones and ReadOnlyMany

- **`CreateSnapshot`:** `ClusterService.CreateSnapshot` with the
  request's name, unique per volume (D398); the snapshot's CSI id is
  `<volume id>@<epoch>`. A retry gets the snapshot of that name.
  `DeleteSnapshot` goes to the trash, as volumes do.
- **A volume from a snapshot:** `CloneVolume` of that epoch into the
  StorageClass's project, with the derived id (D397); a clone shares its
  parent's blocks until flattened (10-volumes.md §10.4).
- **A volume from a volume** (`dataSource: PersistentVolumeClaim`): a
  snapshot of the source named after the new volume, then its clone. A
  snapshot with clones can't be deleted (10-volumes.md §10.4), so the driver
  deletes that snapshot when it deletes the clone.
- **`ReadOnlyMany` (D395):** a volume whose content source is a snapshot
  and whose only access mode is `ReadOnlyMany` is not cloned: it *is*
  the snapshot, attached read-only on each node that stages it. A
  read-only attachment of a snapshot takes no lease (a snapshot doesn't
  change), may be served on any number of hosts, and reads through the
  host read cache (D32, D102). It is new in the client and in
  `hyperstor-ublk` (`attach --snapshot EPOCH`). Deleting the
  PersistentVolume detaches it everywhere and leaves the snapshot to its
  VolumeSnapshot.

### 34.6 Node identities (D402)

Each node plugin needs a `host` certificate: the storage nodes serve it by
that role (D377) and check its realm binding on the name in it (D259).
Join tokens are single-use (D57), so a DaemonSet can't share one.

- **The controller makes one per node.** It watches Kubernetes Nodes; for
  a node with no identity yet it makes a single-use `host` token named
  after the node and binds that host to its own realms (D327), then
  writes the token to a Secret named for the node
  (`hyperstor-csi-host-<node>`).
- **The node's plugin joins once** with its Secret's token, making its
  key on the node (D57), and keeps the identity under its host path; the
  controller deletes the Secret once the join is done. A plugin with an
  identity never reads a Secret again; its certificate renews over the
  API as every host's does.
- **A node removed from Kubernetes** has its host's identity revoked by
  the controller (D384), so a node name reused later gets a new one.
- **What it needs:** a `mgmt` client may make `host` tokens and bind the
  hosts they make to realms it is bound to, and no others. The
  DaemonSet's service account can read every node's Secret while it
  exists; a token is single-use and short-lived (1 h), and the Secret
  goes once used.

### 34.7 Packaging (D396)

- **The image:** `ghcr.io/cmkl-apex/hyperstor-csi:<release>`, from CI at
  each release tag; one static binary and the filesystem tools
  (`mkfs.ext4`, `mkfs.xfs`, `resize2fs`, `xfs_growfs`).
- **The chart** (`deploy/helm/hyperstor-csi`): the CSIDriver object
  (`attachRequired: true`, `podInfoOnMount: false`, `fsGroupPolicy:
  File`), the controller Deployment and the node DaemonSet with the
  sidecars at pinned versions, RBAC, and values for the cluster's API
  addresses, the controller's Secret and example StorageClasses and a
  VolumeSnapshotClass.
- **Docs:** a page in the operator guide, from token to first PVC,
  saying C1 is for trusted storage networks (D404).

### 34.8 Tests

- **csi-sanity** (kubernetes-csi/csi-test) against the driver on an
  all-in-one test cluster, in CI.
- **End to end on Kubernetes:** kind, its nodes privileged containers on
  the host's kernel, where `ublk_drv` loads (as the ublk tests,
  `HS_UBLK=1`; in CI only if its runners can load it, G71): a PVC written and read by a pod, the pod moved to another node, a
  snapshot restored, a clone, an expansion, a ReadOnlyMany volume read on
  two nodes, a node plugin restarted under I/O (G70), and a node killed
  and its volume taken over by another.
- **A model test** of the controller's idempotency: random retries and
  lost answers of every call against a model of the volumes and
  snapshots.

### 34.9 Stages (D390)

| Stage | What | Needs |
|---|---|---|
| C1 | Volumes on storage-network nodes: controller and node, filesystem and raw block, RWO and RWOP, expansion, stats, idempotent ids (D397), node identities (D402), no topology (D403), csi-sanity | HS20 T2 for `require` (D404) |
| C2 | Snapshots and clones: unique snapshot names (D398), volumes from snapshots and from volumes | C1 |
| C3 | Restarts: ublk user recovery in `hyperstor-ublk`'s crate and the plugin's state file (D394) | C1, G70 |
| C4 | ReadOnlyMany: read-only snapshot attachments in the client and `hyperstor-ublk` (D395) | C2 |
| C5 | Packaging: image, chart, release CI, the kind end-to-end suite, the operator guide | C1–C4, G71 |
| C6 | Kubernetes on glidex VMs: the controller through glidex's API, disks found by serial in the guest | C5; glidex: hot-plug of disks into a running VM (16-glidex.md §20, glidex#685), a credential for a cluster in a project, the disk's serial; G72 |

### 34.10 Settled after drafting

- **Node identities** (D402): the controller makes a `host` token per
  Kubernetes Node (§34.6).
- **No topology in C1** (D403): every node of a Kubernetes cluster is
  taken to reach every volume of its StorageClasses. A node whose host
  isn't bound to the volume's realm fails at staging,
  `FAILED_PRECONDITION`, naming the realm; the driver reports no
  topology. A `topology.hyperstor.io/realm` key comes when one
  Kubernetes cluster spans realms.
- **C1 doesn't wait for encryption** (D404): it ships with data-path TLS
  in `require` mode, authenticated, its data in plaintext on networks
  marked trusted (D374). The operator guide says to use C1 only where
  the storage network is trusted, and points to HS20's T3 for the rest
  and to C6 for other tenants' clusters.

### 34.11 Host checks

- **G70. ublk user recovery across a server restart** (D394, C3). On the
  hosts' kernel (6.17) and the oldest supported: a device added with
  `UBLK_F_USER_RECOVERY` keeps its block device while its server is
  killed under fio; `START_USER_RECOVERY` and `END_USER_RECOVERY` from a
  new process resume it, with no I/O error, the I/O held in between
  completing after.
- **G71. ublk from a privileged container** (C5). A privileged pod with
  the host's `/dev` adds a ublk device, the kubelet mounts it into
  another pod, and the device survives the pod being replaced, on
  containerd with cgroup v2; and whether CI's runners can load
  `ublk_drv` for the kind suite.
- **G72. Disk hot-plug under Kubernetes in a glidex VM** (C6). A
  vhost-user-blk disk hot-plugged into a running Cloud Hypervisor and
  QEMU guest appears with its serial under `/dev/disk/by-id`, and
  unplugging it after unmount leaves the guest's other disks alone.
