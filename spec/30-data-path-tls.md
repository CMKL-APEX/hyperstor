# Data-path mutual TLS

Every connection on the storage network proves who it is to the other
end, with certificates of the cluster CA. Part of the hyperstor design;
sections keep their numbers across documents, so `§N` in one document is
the same section everywhere (see [README.md](README.md)).

Status: **design.** D372–D380 and D382–D386 are settled with the owner
(05-decision-log.md points 335–347, 349 and 350, 2026-11-25 to 27). Host checks G67–G69 are
to run on real hosts; G67 holds on one host (I445). Stages T0 and T1 are
built (01-implementation-notes.md I444, I445): connections authenticate
in mode `allow`. Of T2, revoked certificates are refused (D384, I450),
connections are served by their certificate's role and `require` can be
set (D377, I451), pings are keyed (D379, I453), and `data-tls status`
lists who connects without a certificate (I455). It delivers D25, which stays the rule.

## 33. Data-path mutual TLS

### 33.1 Why now

- **The data path tells no one apart** (01-implementation-notes.md I440).
  A storage node serves replication messages (`Repl`, pushes, fetches) on
  any connection, and a client names its own host (D261). A pool that
  requires capabilities (D371) therefore stops mistakes and honest
  clients, not a host that speaks a node's messages.
- **Realm bindings wait on it** (D259): a host's binding is checked on
  the name it gives.
- **hsfs H2 puts tenants with root on the storage network.** That is the
  agreed order: H0, then this, then H1 and H2 (29-hsfs.md).

### 33.2 What it is

- **A TLS 1.3 handshake with certificates on both sides**, against the
  cluster CA (19-administration.md §23.8), on every TCP connection of
  the wire protocol (13-protocol.md §13): client to node, node to node,
  nodes and clients to the map service, and the map service to nodes.
- **After the handshake, frames as today** on every network in the first
  stage (D372), and for good on networks marked `trusted` (D374, D25):
  plaintext, each frame and block under CRC32C.
- **Encryption on networks not marked trusted** is the last stage
  (§33.9): the session stays TLS there.
- **The cluster API is unchanged**: it has had mutual TLS since AD0.

### 33.3 Starting TLS on a connection (D373, D376)

Same ports, so a cluster upgrades node by node.

1. `Hello` and `HelloOk` in plaintext, as now. They agree a protocol
   version.
2. If both speak version 8 and the caller has an identity, it sends
   `StartTls`. The peer answers `StartTlsOk`, or `Error` if it has no
   identity or the mode is `off`.
3. From the next byte the stream is TLS. Both sides run the handshake;
   each checks the other's chain against the cluster's root and its
   revocation list (§23.8), and keeps the other's identity (role and id)
   with the connection.
4. On a connection that carries plaintext afterwards, the caller sends
   `close_notify`; the peer answers with its own and sends nothing more
   until the caller's next frame. The caller sends frames once it has
   read the peer's `close_notify`. Frames then go on as before.

- **Why after `Hello`:** an older peer answers a version below 8 and is
  never sent `StartTls`, which it could not decode. A second port per
  reactor would need new addresses in the map and new firewall rules.
- **Why the handshake on the socket, then plaintext:** it is the standard
  shape (FTP's `CCC` does the same), and the encrypted stage keeps the same
  handshake and simply doesn't close the session. G67 checks the turn
  back to plaintext loses or swallows no byte.
- **Who runs it:** rustls, which does no I/O of its own, fed from the
  reactor's sockets (D376). No tokio on the data path (D3) and no second
  TLS library: the cluster API already uses rustls.

### 33.4 Modes (D373)

A cluster setting in the map, set with `hyperstorctl data-tls set`:

| Mode | A caller with an identity | A node or the map service |
|---|---|---|
| `off` | doesn't ask | refuses `StartTls`; serves as today |
| `allow` | asks whenever the peer speaks version 8 | authenticates who asks; still serves who doesn't |
| `require` | asks, and drops a peer that refuses or speaks less | serves only `Hello`, `Ping` and `StartTls` before the handshake |

`require` came with authorization by role (T2, I451): without it the
mode would promise a boundary that isn't there.

- An upgraded cluster starts `off`; a new install starts `allow`: the
  cluster file's `data-tls allow` line, which `hyperstor-install
  --all-in-one` writes, read when the first map is made.
- **Before `require`**, the map service asks every node that is up, and
  refuses the change while one holds no identity or runs a build that
  doesn't refuse by it (I451). Clients can't be asked, so each node
  counts the connections it serves unauthenticated
  (`hyperstor_unauthenticated_connections_total`) and tells the map
  service their addresses; `hyperstorctl data-tls status` lists them:
  who would be cut off (I455).
- `require` to `allow` is always possible: the way back if a host was
  missed.
- **Why three:** `allow` lets every host get its certificate and prove
  the handshake works before anything is refused.

### 33.5 Who may do what (D377)

Once a connection is authenticated, and for every connection in
`require`:

| Identity | May send |
|---|---|
| `node` N | node-to-node messages (replication, pushes, fetches, digests, peering) and reports to the map service, as node N: the id in `Hello` and in each report must be N |
| `meta` | what the map service sends nodes (`MapPush`, `Fence`, scrub requests) and members send each other |
| `host`, `s3`, `admin` | clients' messages only: object reads and writes, leases, snapshots, map requests |
| `mgmt` | clients' messages, until management clients show a `host` certificate here (D385); then nothing |

- A client's message on a `node` connection is served: primaries act as
  clients of other PGs (copies, flattens).
- **A `host` certificate's holder is that host, whether it says so or
  not** (I454): a storage node takes the name from the certificate.
- **The host a client names (`Host`, D261) must be its certificate's.**
  Realm bindings (D259) and a capability's host (D363) are then checked
  against a name the client can't choose.
- **Revoked certificates** (§23.8) are refused at the handshake, and a
  connection whose certificate is revoked later is dropped at its next
  request. The list reaches nodes and clients in the cluster map (D384,
  I450), so a revocation answers once every node that is up holds it.
  - The map carries it only while the mode isn't `off` and every node
    that is up reads the format (protocol version 9). Why: a node from
    before it can't decode such a map, and would stop.
  - `RevokeCertificate` says which holds (`data_path`): `nodes`, `off`
    or `old_nodes`. The map service adds the list by itself once the
    reason is gone.
  - Callers check too: a client, a node or the map service drops a
    connection whose peer shows a revoked certificate. So a revoked
    storage node is cut off, and goes down by its lease.
- In `allow`, an unauthenticated connection is served as today, and its
  node-to-node messages too: `allow` is a migration state, not a
  boundary.
- **Why by role:** it is the one fact the storage path lacks today
  (I440), and the roles exist (§23.8).

### 33.6 Certificates on every host (D375, D378)

| Host | Identity | How it gets it |
|---|---|---|
| Storage host joined with a token | `node` N | has it (AD1) |
| Map service member that runs a storage node | `node` N beside its `meta`, in `node-N.crt` | the installer issues one at its next run, from the member's CA; until then its node shows the member's (D370) and is served as `meta` |
| Storage host that serves VM disks | `host` NAME beside its `node` N | the installer asks the cluster API with the node's certificate (D386, I456) |
| Map service member's host, as a client | `host` NAME, in `host.crt` | the installer issues one from the member's CA (I454): `hyperstor-vhost` and `hyperstorctl --identity` there show it, not the member's |
| VM host (`--role vhost`), `hyperstor-ublk` host | `host` NAME, the name it gives as `--host` | `hyperstor-install --role vhost --join API --token-file F`, with a `host` token (D375) |
| S3 gateway | `s3` | has it (D233) |
| `hyperstorctl`'s data commands | the administrator's | has it |

- Renewed by the daily timer, as node certificates are (D56).
- `hyperstord`, `hyperstor-vhost`, `hyperstor-ublk` and `hyperstor-s3`
  take `--identity DIR` (`hyperstord` has it since D370).
- **glidex's driver** installs VM hosts: its flow gains the token step
  (glidex #665). Its servers make each node's `host` token with their
  `mgmt` identity: an unbound one may already; one bound to realms (D311)
  only once an administrator lets it, with
  `hyperstorctl realm client-tokens --client C --role host` (D382, I447).
- **Why a grant, not every bound client:** the client chooses the name
  in the token, and a `host` certificate opens volumes under that name
  (D377). An administrator gives that power to one client knowingly.
- **A host such a client joins is bound to the client's realms** (D383,
  I449): the token carries them, and the join binds the name before the
  certificate is signed. A name bound already keeps its binding, and
  gets a token only if all its realms are the client's; a name that
  joined before and is bound to no realm gets none.
- **Why bound:** an unbound host attaches volumes of any realm (D251),
  so a certificate under a new name would reach past the client's
  realms, whatever limited the names it may ask for.
- **Why a token for VM hosts:** the same path storage hosts use, with no
  dependency on glidex for a standalone host.

### 33.7 Pings (D379)

Nodes ping each other over UDP to find dead peers (D40). A forged
answer could make a dead node look alive.

- From stage T2, each ping and each answer carries a counter and a short
  MAC over its contents, under the cluster's sealing key (D133), which
  every node holds (D370). Either valid key opens it, so a rotation
  breaks nothing.
- In `require`, a ping or answer without a valid MAC is ignored. In
  `allow` it is accepted, so nodes from before it keep their peers.
- A node that hasn't fetched the keys yet answers no keyed ping. Its
  peers still find it over the TCP fallback, on an authenticated
  connection (I453); in `require` it reports nobody silent until it
  has them: seconds after its start.
- **Why the sealing key and not a key per pair:** a key from two nodes'
  TLS session would make pings depend on the TCP connection they exist
  to notice the loss of. The sealing key needs no state per peer.

### 33.8 What it doesn't cover

- **RDMA queue pairs** are set up only over an authenticated TCP session
  (D25), when RDMA is built (13-protocol.md §13.2).
- **A trusted network's wire.** After the handshake a trusted network
  carries plaintext (D374): someone who can inject packets there can
  still take over an authenticated connection. Mark a network trusted
  only if that can't happen.

### 33.9 Stages (D372)

| Stage | What | Needs |
|---|---|---|
| T0 | Certificates everywhere: `node` certificates on member hosts, `host` tokens and `--role vhost --join` | — |
| T1 | `--identity` on every data-path binary, the handshake, modes `off` and `allow`, identities kept with connections, the unauthenticated-connection count | T0, G67 |
| T2 | `require`, authorization by role (§33.5), the host name from the certificate, keyed pings (§33.7) | T1 |
| T3 | Encryption on networks not marked trusted: TLS records from rustls first, kernel TLS where G68 allows | T2, G68, G69 |

Milestone HS20 (17-milestones.md §21).

### 33.10 Host checks

- **G67. The turn back to plaintext** (D373). With rustls 0.23 on both
  ends: after each side's `close_notify`, the next bytes read from the
  socket are the first plaintext frame, whole, with none consumed into
  the TLS reader's buffer, at 1 and at 64 concurrent connections.
- **G68. Kernel TLS from a rustls handshake** (T3). On the hosts' kernel
  (6.17): the session's secrets extracted from rustls set up `TCP_ULP
  tls` for TLS 1.3 with AES-GCM in both directions, and io_uring sends
  and receives then work unchanged, zero-copy sends included.
- **G69. The cost of encryption on a reactor** (T3). 4 KiB and 1 MiB
  reads and writes on an untrusted network, userspace records against
  kernel TLS against plaintext, on the pilot's hosts.
