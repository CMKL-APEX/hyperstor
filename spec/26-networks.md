# Networks

How hyperstor uses hosts with several network ports, of different kinds:
which ports carry what, how a reactor and a client pick a path, how a
port's role changes while the cluster runs, and how an orchestrator such
as glidex claims ports and pairs GPUs with storage NICs. Part of the
hyperstor design; sections keep their numbers across documents, so `§N`
in one document is the same section everywhere (see
[README.md](README.md)).

Status: **designed, not yet built** (2026-11-02). D203–D213 are settled
with the owner (points 173–176 of [05-decision-log.md](05-decision-log.md)):
the reconciler runs in the map service's leader, as a module that can
move out (D203); host topology comes from glidex's reader, as a build
dependency (D204); any orchestrator may claim (D205); compute ports are
lent to storage while idle by default (D209); the rest as recommended.
G47–G53 are the host checks they rest on. Built so far (HS19, branch
`feat/fabric`; 01-implementation-notes.md I233–I238): networks and
endpoints in the cluster file and the map, nodes and clients reaching
devices at their endpoints, and in `hyperstor-fabric` the ranking by
closeness, path choice and the reconciler's decisions, not yet wired. It builds on 13-protocol.md §13.2 (networks and transports in
the map) and D31 (port roles), and amends both.

## 29. Networks (D203–D213)

### 29.1 Goal and scope

Today a device in the map has one address (D39), every connection is
TCP, and the map service, the cluster API, client I/O, replication and
backfill share that address. A GPU host has many fast ports, most of
them idle to hyperstor, while a small host has one slow port.

- **Goal:** use every port a host may give to storage, close to the
  data, without pinning the choice at install: roles and paths follow
  the workload, and a host with less degrades rather than failing.
- **In:**
  - networks in the cluster file, and endpoints per reactor in the map
    (§29.3, §29.5);
  - control traffic apart from data traffic (§29.4);
  - choosing a path per I/O among the eligible endpoints (§29.6);
  - health per network (§29.7);
  - the fabric reconciler: port roles as leases, weights, lending
    (§29.8);
  - claims by orchestrators, and GPU-direct pairing through them
    (§29.9, §29.10).
- **Later:** live re-pairing of a running VM's devices (D212).
- **Out:**
  - changing a host's network configuration (VF counts, bonds, OVS
    bridges, addresses, routes, the subnet manager). That belongs to
    the host's owner: glidex's `netd`, or the operator and
    `hyperstor-install`. Why: one owner per setting, and hyperstor needs
    no privileges beyond `hyperstord`'s;
  - placing data by network speed automatically (D213).

### 29.2 Reference hosts

Two hosts in the pilot bound the design.

| | GPU host (8-GPU HGX A100) | Small host |
|---|---|---|
| Sockets, NUMA nodes | 2 sockets, 8 NUMA nodes (4 per socket) | 1 socket, 1 NUMA node |
| PCIe | 4 Gen4 switches per host, each with 2 GPUs, 2 InfiniBand ports and 1–2 NVMe drives; each GPU and one port under the same sub-switch | each GPU behind its card's own switch |
| InfiniBand | 8 × HDR 200 Gb/s (ConnectX-6), one beside each GPU; IPoIB on all 8 in one subnet, datagram mode (MTU 2044) | none |
| Ethernet | 2 × 100 GbE (ConnectX-6) in an LACP bond owned by the VM switch, with no host address, on the CPU's own lanes of socket 1; 1 GbE management | 1 GbE only |
| Other | BlueField-3 (link down) | — |
| What hyperstor uses today | the 1 GbE port, for everything | the 1 GbE port |

What they show:
- **The fast ports are GPU rails.** On the GPU host every InfiniBand
  port sits beside a GPU, which makes them D31's `compute` ports, and
  the only NICs a GPU can reach peer to peer within its switch.
- **The "storage" NICs aren't free.** The 100 GbE ports are glidex's VM
  uplink, on one socket only.
- **One subnet for eight ports.** Without per-source routing, Linux
  sends from all eight addresses through one port (G47).
- **A host may have nothing to choose.** On the small host every rule
  here must reduce to "use the one port", with no warning noise.

### 29.3 Model

**Host topology (D204)** comes from glidex's reader
(`host-topology.md`), used by `hyperstord` and `hyperstor-install` as a
build dependency. It gives, per host:
- NUMA nodes and their CPUs, and which socket each belongs to;
- the PCIe tree: for each device its root port, the switch above it
  and its NUMA node;
- SR-IOV physical and virtual functions;
- **network ports** (added by this design, in glidex): netdev name,
  PCI function, link layer (`ethernet` or `infiniband`), speed, state,
  MTU, RDMA device and port, addresses, and for InfiniBand the subnet
  prefix and the subnet manager's identity.

**Closeness** of two devices, best first, from the tree: same PCIe
switch, same NUMA node, same socket, other socket. Why these levels:
within a switch, peer-to-peer traffic needs no root complex; within a
NUMA node, memory is local; across sockets, every byte crosses one
shared link (G49).

**Networks** are named in the cluster file and the map:

| Field | Meaning |
|---|---|
| `name` | e.g. `mgmt`, `ib`, `eth100` |
| `kind` | `control` or `data` (D206) |
| `trusted` | D25: plaintext after the handshake, or TLS throughout |
| `transports` | `tcp`, `roce`, `ib` (13-protocol.md §13.2) |
| `match` | how a port is recognised as on it: a subnet, or an InfiniBand subnet prefix |
| `traffic_class` | DSCP and PFC priority on RoCE, service level on InfiniBand (D31) |

A **fabric** is what two ports must share to reach each other directly:
the same InfiniBand subnet prefix and subnet manager, or the same IP
subnet. Hosts discover it from their ports; the map records it. Why:
two hosts on networks with the same name but no shared fabric must not
pick each other's endpoints.

**Ports and roles (D209).** Each port has a role, held as a lease from
the reconciler (§29.8):

| Role | Carries | Example |
|---|---|---|
| `host` | the host's own traffic; hyperstor's control traffic | 1 GbE management |
| `uplink` | a VM switch's uplink; hyperstor doesn't use it unless the host owner gives it a host port | the 100 GbE bond |
| `storage` | hyperstor's data traffic | a dedicated storage NIC |
| `compute` | GPU rails for NCCL or RCCL; hyperstor only by lending or for GPU-direct (D31) | the InfiniBand ports |
| `converged` | both, hyperstor in its own traffic class | a single fast NIC |
| `vm` | a pool of VFs handed to VMs; never used by hyperstor itself | VFs on a rail |

### 29.4 Control and data (D206)

- **Control traffic** (the map protocol, Raft, the cluster API, mgmt,
  heartbeats to the map service) uses `control` networks.
- **Data traffic** (client I/O, replication, recovery and backfill, NFS
  data servers) uses `data` networks.
- **Replication isn't split from client I/O.** Backfill keeps its
  per-target budget (I187) and, on RDMA, its own traffic class.
- **One network may be both** (a cluster with one port per host, like
  the pilot today). Then nothing changes from now.

Why: the pilot's heartbeats starved behind backfill on one connection
(I184, I187); moving bulk data off the control network removes that
class of failure. Splitting replication from client traffic, as Ceph's
`cluster_network` does, halves the bandwidth each can use and adds a
failure where one network is up and the other down.

### 29.5 Endpoints and closeness (D207)

Amends 13-protocol.md §13.2 ("Several ports"), where a reactor uses the
port on its own NUMA node.

- **In the map,** each reactor lists **endpoints**: network, address and
  port, transport, the host port it is on, closeness to the reactor's
  device, weight and state. D39 stays: one listening port per reactor,
  now on each of its endpoints.
- **Ranking.** A reactor's eligible endpoints are the ports with a role
  hyperstor may use, ordered by closeness to its device (§29.3). It
  binds the closest few (by default 2 per network) and lists the rest
  as eligible but unbound. Why a few: each bound endpoint costs a
  listener, and on RDMA queue pairs; two per network survives a port's
  loss.
- **Data stays with the device.** A reactor's buffers live on its
  device's NUMA node (host-topology's carve-out, D7). When no port is on
  that socket, the NIC's DMA makes the one crossing, and the readiness
  page warns. Going NVMe to NIC directly, peer to peer, isn't done:
  the reactor checks checksums and runs the engine on every chunk.
- **Source addresses.** Outgoing connections bind the endpoint's address
  as their source. With several ports in one subnet, the host must also
  route by source (G47); the installer checks it and refuses to bind
  endpoints on a host where it doesn't hold.
- **Clients** rank their own ports the same way: by closeness to the
  client reactor's CPU, and for `libhyperstor-gpu` to the GPU whose
  buffer is read or written (12-gpu.md §12.1).

### 29.6 Choosing a path (D208, D213)

Two loops, on different time scales. The fast one is in the data path
and never waits on the control plane.

- **Eligible paths.** A sender may use a peer's endpoint when both have
  a port on the same fabric of a `data` network, with transports as in
  13-protocol.md §13.2 (RDMA when both have it, TCP otherwise).
- **Per I/O,** a client or reactor picks among the eligible paths to a
  peer by **two random choices, weighted** by the reconciler's weights,
  taking the one with fewer bytes in flight (then lower RTT, then more
  RDMA credits). Why not "least loaded": every sender would pick the same
  path at once and herd onto it.
- **Warm paths.** Each sender keeps connections open on a small set of
  paths per peer (by default 2), since RDMA connection setup takes
  milliseconds (G53). DC queue pairs (13-protocol.md §13.2) make the
  set cheap.
- **Moving.** A request in flight finishes on its path. Retries and new
  requests may take another. Why it is safe: every message carries its
  epoch, PG and request id and never relies on order between messages
  (13-protocol.md §13.2), so which path carried it doesn't matter.
- **Different hosts (D213).** A host with only a slow network reaches
  the others over it, and its PGs are as slow as that link. Placement
  doesn't weigh network speed: the operator keeps such hosts out of
  fast pools with device classes (24-pools-and-devices.md §27.3), and
  health warns when a pool's PG spans a link under a tenth of its
  others' speed. Why not automatic: speed alone doesn't say whether a
  slow host is acceptable for a pool, and moving PGs on a link change
  would cause backfill storms.

### 29.7 Health per network (D210)

- **Each data network is watched.** Peer reports (D40) are per network:
  a node reports a peer silent on a network, not just silent.
- **An endpoint fails, not the node.** When a port or network fails,
  the map marks its endpoints down, and senders move to other paths.
- **A node is down** only when the map service loses its heartbeats on
  the control network, or when no data network reaches it. Why: a
  failed NIC would otherwise turn into a node flap and a backfill, the
  failure the pilot already saw from other causes (D149).
- **Damping** as D149 for endpoints too: an endpoint that flaps is held
  down for doubling periods.

### 29.8 The fabric reconciler (D203, D208, D209)

**Where (D203).** A module, `hyperstor-fabric`, run by the map service's
leader. Its state (port roles and leases, endpoint weights, claims) is
replicated map state, so it survives a change of leader.
- **Kept separable:** the module is a pure function from inputs to
  changes, `reconcile(observed, policy, claims, current) -> changes`,
  behind two traits: one for its inputs and one for writing its state.
  In the leader they read the map and write map changes; as its own
  service later, they would use the cluster API. Why: it needs the
  map's facts and the leader's single writer now, and a busy fabric
  shouldn't be able to slow the map service later.

**Inputs:**
- topology and port state, from each `hyperstord` with its heartbeats;
- port counters every 10 s: bytes each way and, for InfiniBand,
  `PortXmitWait`, and for RoCE congestion notifications;
- claims (§29.9) and the policy below.

**Outputs:** port roles as leases, each reactor's eligible endpoints,
and their weights.

**Timing.** Every 10 s. A role or eligibility change waits out a minimum
dwell (60 s) since the last one on that port. Weights move only when the
new weight differs by more than 20%. These are starting values for G52
to tune. Why: the pilot has flapped for other reasons (D149); a loop
that reacts to its own effect on the counters would flap the same way.

**Leases and revocation.** A role lent to storage (a `compute` port, say)
is a lease. When its owner claims it back, the reconciler revokes it with
a deadline (30 s): senders stop opening requests on its endpoints,
in-flight ones finish, and the endpoints are removed.

**Lending policy (D209)**, per cluster and per port:

| Policy | `compute` ports used by storage |
|---|---|
| `off` | never, except GPU-direct on hosts with `gpu_nic: rail` (D211) |
| `idle` (default) | while the counters show them idle for a dwell and no claim holds them |
| `always` | always, in hyperstor's traffic class |

Why `idle` by default: on a GPU host most of the fast ports are rails,
idle much of the time, and storage would otherwise stay on far less
bandwidth. A lent rail is revoked as soon as its counters show traffic
or a claim names it, and a claim always wins. What counts as idle, and
how fast a rail is handed back, is set from G52 before stage 3 ships.

**Never:** it never changes host network configuration (§29.1), never
uses a port or VF a claim holds, and never moves data onto a network
that isn't `trusted` without TLS (§29.11).

**Standalone.** With no orchestrator there are no claims: the loop
weights storage ports, and lends compute ports only under `idle` or
`always`. On a host with one port it changes nothing.

### 29.9 Claims (D205)

Any orchestrator (glidex, a Slurm prolog, a Kubernetes device plugin)
tells the reconciler what it holds and asks for what it needs, through
the cluster API, as the role `orchestrator` (beside `node`, `meta` and
`mgmt`; 19-administration.md §23).

| Call | Meaning |
|---|---|
| `Claim{host, target, mode, holder, ttl}` | `target` is a port, a VF, or a request such as "a storage VF peer-close to GPU `0000:07:00.0`"; `mode` is `exclusive` or `shared` (in a traffic class) |
| `Renew`, `Release` | keep or end a claim |
| `Watch` | the orchestrator's claims and grants, as they change |

- **A claim is a lease**, renewed by its holder. A claim on a port lapses
  when not renewed, and the port returns to the reconciler. A claim on a
  VF or device held by a running VM doesn't lapse into reuse: it is
  reported as stale, and released only by its holder or an operator.
  Why: reusing a VF still attached to a VM would hand one device to two
  users.
- **A request is answered with a grant:** the VF's address, its network,
  and the traffic class to set in the guest. The reconciler picks a free
  VF on a `storage` or `converged` port (or a `compute` port under D211)
  peer-close to the named device, else the closest by §29.3.
- **Audited** in the map service's audit log, with the holder.
- **glidex** is one such client: its scheduler claims rails its GPU VMs
  use, and asks for storage VFs for GPU-direct VMs (§29.10). glidex
  without hyperstor needs none of this.

### 29.10 GPU-direct pairing (D211, D212)

- **D31's default is per host (D211).** Where every GPU has a storage-
  capable port under its own switch (the GPU host), `gpu_nic: rail` is
  the default: GPU-direct uses that port, in hyperstor's traffic class.
  Elsewhere it stays `gpu_nic: storage`. Why: on the GPU host the
  storage NIC is a CPU hop and, for half the GPUs, a socket away, and
  across sockets peer-to-peer falls back to bounce buffers (G50).
- **Bare metal:** `libhyperstor-gpu` picks the port peer-close to the
  buffer's GPU, among its eligible endpoints (§29.5).
- **VMs (D212):** at VM start, the orchestrator claims a storage VF
  peer-close to each GPU (§29.9) and passes both to the VM, with vCPUs
  and memory on that NUMA node (12-gpu.md §12.3). While the VM runs,
  if a better pairing appears, the VM is marked as able to improve at
  its next restart. Live re-pairing through VFIO hot-plug is later: the
  guest's GPU-direct library would have to rediscover its NIC.
- **What placement can't promise:** the tree shows a pairing is
  possible, not that peer-to-peer works. Under an IOMMU, ACS may send
  every peer-to-peer transfer up to the root complex unless ATS lets it
  stay in the switch, and the guest's virtual PCIe tree decides whether
  cuFile sees the two as close (G2, G51). The reconciler prefers pairs
  that `hyperstor-install --check` has verified, and the grant says
  whether its pair was.
- **Hosts without a pairing** (the small host): GPU VMs run, and read
  through guest memory; a `strict` request is refused there, a
  `preferred` one placed without GPU-direct.

### 29.11 Security

- **Per network, as D25.** A path change never moves data onto a network
  that isn't `trusted` in plaintext: such a path is TLS from its first
  byte, and a lent role doesn't change a network's `trusted`.
- **Fencing doesn't depend on the path.** The blocklist and lease
  identities (D150) name client instances and leases, not addresses,
  so a fenced client can't get back in over another network. A test
  must show it: a blocklisted client reconnecting over a second network
  is refused.
- **Claims need the `orchestrator` role,** whose certificates are
  issued like the others (19-administration.md §23). A tenant's VF is
  never lent: `vm` ports and claimed VFs are outside the reconciler's
  reach.

### 29.12 Stages (HS19)

| Stage | Content | Needs |
|---|---|---|
| 1 | glidex's topology crate with port facts (a glidex change); networks in the cluster file; endpoints in the map; control apart from data; reactors bind close endpoints; source routing checked (G47); TCP only | HS2 |
| 2 | Path choice per I/O and warm paths (§29.6); health per network (§29.7); the fencing test (§29.11) | 1 |
| 3 | The reconciler in the leader: roles as leases, weights from counters, dwell and revocation (§29.8) | 2 |
| 4 | Claims and the `orchestrator` role; lending policies; glidex's scheduler as the first client (§29.9) | 3 |
| 5 | RDMA endpoints with HS6; GPU-direct pairing with HS8 and HS9 (§29.10) | 4, HS6 |

Stage 1 alone moves the GPU host's data off 1 GbE, onto IPoIB or a host
port on the 100 GbE bond, with no RDMA work.

### 29.13 Host checks

- **G47. Several ports in one subnet** (D207). Seen on the pilot's GPU
  hosts (01-implementation-notes.md I237): no rules by source, so the
  risk is real; the measurement with and without rules is open. On a host with several
  IPoIB ports in one subnet: with `arp_ignore=1`, `arp_announce=2` and a
  routing rule per source address, does each reactor's traffic leave by
  its own port? Measure per-port counters while 8 reactors send, with
  and without the rules.
- **G48. TCP over IPoIB and 100 GbE** (D206, stage 1). First part
  measured 2026-11-02 (02-benchmarks.md): one IPoIB port in datagram mode
  gives 6–7 times 1 GbE for 1 MiB I/O and 3–4 times lower latency at
  QD1, but one connection reaches about 0.8 GB/s; the rest is open. With
  `hyperstor-bench remote`: throughput and latency for 4 KiB and 1 MiB
  I/O over one HDR port in datagram and connected mode, over 8 ports,
  and over a host port on the 100 GbE bond, against 1 GbE.
- **G49. Crossing sockets** (D207). Serve one NVMe drive through a port
  on the same switch, the same socket and the other socket: bandwidth,
  p99 latency, and the inter-socket link's load with 1, 4 and 8 ports
  busy.
- **G50. GPU to NIC peer-to-peer on bare metal** (D211). `ib_write_bw`
  with GPU memory (perftest built with CUDA) between a GPU and the port
  under its switch, a port on the same socket, and one on the other
  socket.
- **G51. Peer-to-peer in a VM** (D212; extends G2). With the GPU and a
  VF passed through and the IOMMU on: does traffic stay in the switch
  (ACS and ATS settings), at what bandwidth against G50, and does cuFile
  in the guest see the pair as close, under QEMU with a virtual PCIe
  switch and under Cloud Hypervisor?
- **G52. Counters as a sign of idleness** (D209). Sample InfiniBand and
  RoCE port counters every 10 s through an NCCL all-reduce with pauses:
  do they tell an idle rail from a busy one, how fast, and at what CPU
  cost? Tune §29.8's dwell and hysteresis from it.
- **G53. The cost of warm paths** (D208). Time to set up an RC queue
  pair and a DC target; memory per warm path; how many warm paths per
  reactor a host with 8 ports affords.

### 29.14 Alternatives considered

- **Bonding, LACP or MPTCP for the data path.** Bonds hash flows over
  ports, so one flow gets one port anyway; MPTCP adds a kernel layer for
  what sharding already gives (many PGs, many reactors, many
  connections). Neither helps RDMA. Hosts may still bond for their own
  reasons.
- **A separate network for replication** (Ceph's `cluster_network`):
  §29.4.
- **Pinning roles and paths at install.** Simple, but a rail lent to
  storage could never be returned to a training job, and a new port or
  a failed one would need a reinstall.
- **A separate fabric daemon from the start.** Another replicated store
  to run, before load shows the leader can't carry the loop; D203 keeps
  the door open instead.
- **The reconciler in glidex.** Hyperstor must run without glidex, and
  ports used by non-glidex jobs need the same arbitration (D205).
- **UCX's automatic transport and rail selection.** D30 keeps transport
  choice in hyperstor; UCX's selection doesn't know about claims, roles
  or the map's endpoints.

### 29.15 Changes elsewhere

- **glidex:**
  - move `topology.rs` out of `glidex-control-plane` into its own crate,
    as `glidex-raftstore` was, and add network ports (§29.3). Hyperstor
    pins it as a git dependency (D20, D204);
  - its scheduler claims rails its GPU VMs use and asks for storage VFs
    for GPU-direct VMs (§29.9, §29.10), in `host-topology.md` and its
    GPU VM spec.
- **hyperstor:** 13-protocol.md §13.2 ("Several ports", port roles) and
  D31 point here; the cluster API gains the `orchestrator` role.
