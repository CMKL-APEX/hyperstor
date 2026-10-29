# Internal protocol and transports

Messages, and TCP, RoCEv2 and InfiniBand transports. Part of the hyperstor design; sections keep their numbers across documents, so `§N` in one document is the same section everywhere (see [README.md](README.md)).

## 13. Internal protocol (D5)

### 13.1 Messages

- **Framing.** Every message is a fixed header:
  - magic, version, message type and flags;
  - request id, header length, payload length;
  - CRC32C of the header.

  Then a typed header body, encoded little-endian with a fixed layout
  and no general-purpose serializer, then the payload.
- **Every request to a PG names its target**: pool, PG, map epoch, PG
  epoch, and the lease identity it acts under (D150; 0 for none), 32
  bytes.
- **Messages:**

| Group | Messages |
|---|---|
| Session | `Hello` (version range, identity, reactor wanted), `Ping` |
| I/O | `Read`, `Write`, `WriteZeroes`, `Discard` |
| Replication | `Repl`, `ReplAck` (with a read-lease grant) |
| Volume metadata | `ObjGet`, `ObjCas`, `LeaseAcquire` (with the holder's label, D153), `LeaseRenew`, `LeaseRelease`, `LeaseDelegates` (D151), `Quiesce`, `Resume`, `Notify` |
| Peering and recovery | `PeerQuery`, `PeerInfo`, `PushOps`, `BackfillScan`, `BackfillObjects` |
| EC | `LogAppend`, `FragmentWrite`, `FragmentRead` |
| Errors | `StaleMap{current}`, `NotPrimary{epoch}`, `Blocklisted` (the client instance), `Fenced` (the lease identity, D150), `CasConflict{version}`, `NoSpace`, `Checksum`, `Busy{retry_after}` |

- **Versions.** `Hello` agrees on the highest version both sides speak.
  Each release speaks its own version and the one before, so rolling
  upgrades work node by node (operations.md §18.4). The exception is
  version 2 (lease identities), which replaced 1 at once (D152).
- **Security** per D25.

### 13.2 Transports (D30)

- **One transport layer.** The reactor talks to a `Transport` trait with
  two implementations, TCP (io_uring sockets) and verbs (rdma-core).
  The messages of §13.1 are the same on both. A libfabric
  implementation can be added behind the same trait (D30).
- **Networks and transports in the map.** Each network a node is on
  lists its address and the transports it offers there: `tcp`, `roce`
  (RoCEv2, with GID index and traffic class) or `ib` (with GID, LID and
  partition key). A sender uses RDMA to a peer when both have it on a
  common network and TCP otherwise, so a cluster can mix InfiniBand
  storage nodes with Ethernet-only compute nodes.
- **Connection setup.** Every connection starts with the authenticated
  TCP session of D25 (over IPoIB on an InfiniBand-only fabric). The two
  sides exchange queue pair numbers, GIDs and receive-buffer credits over
  it, then move to the queue pair. The TCP session stays open, idle, to
  carry a clean close and to detect a dead peer.
- **Queue pairs** are reliable connected (RC). A reactor posts receive
  buffers to one **shared receive queue** for all its queue pairs, so
  memory doesn't grow with the number of peers. **Later:** on mlx5
  devices, dynamically connected (DC) queue pairs let one queue pair
  reach every peer, which removes per-peer queue pairs altogether.
- **No reliance on order.** Every message carries what its receiver
  needs to place it: request ids for replies, PG sequence numbers for
  `Repl` (replication.md §8.1). A reply that covers an RDMA write is sent only after
  that write has completed at the target (on libfabric, with
  delivery-complete semantics). The protocol then also runs on
  transports that deliver out of order, such as UET's unordered mode
  (D30).
- **Payloads:**
  - messages up to the inline limit (typically 256 bytes) go as inline
    `SEND`s;
  - other headers go as `SEND`s from the reactor's registered buffers;
  - data moves by RDMA operations that only the server starts: for a
    read, the server writes into the client's buffer; for a write, the
    server reads from it. The client names its buffer by address,
    length and rkey. No server rkey ever leaves the server (D13).
- **Flow control** is by credits. Each side grants the other a number of
  receive buffers at setup and returns credits in every reply. A sender
  without credits queues, so receivers never see receiver-not-ready
  retries.
- **Several ports.** A node with more than one RDMA port (or several
  NICs, one per NUMA node) lists each as its own address. A reactor
  uses the port on its own NUMA node.
- **Port roles and traffic classes** (D31):
  - each port has a role, `storage`, `compute` or `converged`. The
    installer proposes `compute` for ports that sit under the same PCIe
    switch as a GPU, and `storage` for the others; the operator
    confirms;
  - each network has a traffic class for hyperstor: a DSCP value and PFC
    priority on RoCE, a service level on InfiniBand. Hyperstor sets it on
    every queue pair;
  - hyperstor uses `storage` ports. It uses `converged` ports with its
    traffic class. It uses a `compute` port only for GPU-direct on a host
    whose policy is `gpu_nic: rail`, and then with its traffic class.
    The default policy is `gpu_nic: storage`: GPU-direct goes through the
    storage NIC, at the cost of a hop through the CPU's root complex
    instead of the GPU's PCIe switch;
  - the readiness check warns when a `converged` or `compute` port has
    no separate traffic class for hyperstor, or when the class's PFC
    priority isn't lossless on the NIC.
- **NVIDIA SHARP.** Hyperstor doesn't use in-network reduction:
  - SHARP's operations (sums, minimum and maximum, bitwise operations)
    can't compute Reed–Solomon parity, which needs Galois-field
    arithmetic; only single-parity XOR would fit, and EC pools need
    `m ≥ 2`;
  - replication needs reliable point-to-point transfers, which SHARP
    doesn't provide;
  - SHARP trees are set up per job for a fixed group of members, and
    switches hold few of them at a time.

  On an InfiniBand fabric where the aggregation manager (`sharp_am`)
  runs, hyperstor's service level must differ from the ones SHARP and
  NCCL use. The subnet manager should then map it to its own virtual
  lane, so collectives and storage don't queue behind each other. The
  host can't read the switches' SL-to-VL tables, so this is an
  installation step, with a readiness warning when hyperstor's service
  level equals the one in the host's NCCL or SHARP configuration
  (`NCCL_IB_SL`, `SHARP_COLL_*`).
- **Fabric requirements** (RoCEv2): a lossless traffic class (PFC) or
  ECN with DCQCN congestion control, consistently on NICs and switches.
  The node's readiness check (as in host-topology §5) reports, per RDMA
  device: the link layer, MTU, the RoCE mode and GID table, the PFC and
  trust settings of the NIC, and whether ECN is on. A mismatch is a
  warning, not a refusal, since switches can't be checked from the host.
  On InfiniBand it checks the port state, the subnet manager and the
  partition key.
