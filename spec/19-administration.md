# Administration: installer, API, CLI, UI and security

How hyperstor is installed, administered and secured: the installer, the cluster API, `hyperstorctl`, the management service and web UI, and the identities and checks behind them. Part of the hyperstor design; sections keep their numbers across documents, so `§N` in one document is the same section everywhere (see [README.md](README.md)).

Status: **draft, partly built.** D50–D55 were settled with the owner on 2026-10-21
([05-decision-log.md](05-decision-log.md)); D56–D60 were made while drafting
and are open for review. Host checks G17 and G18 are open. Built so far
(01-implementation-notes.md I41–I56): the cluster API on the map service,
over mutual TLS with identities and roles, audited and rate-limited, with a break-glass local socket; the root and intermediate
CAs and leaf certificates; `hyperstorctl` (status, nodes, pools, PGs,
devices out and in, an I/O test); metrics on storage nodes' admin
ports; and `hyperstor-install` with host checks, the all-in-one trial
cluster, multi-host installs from a shared cluster file (before join
tokens) and a daily renewal timer; an interim read-only dashboard on
loopback, and Grafana and Prometheus files. This
section refines 15-operations.md §16, §18 and §19, which point here.

## 23. Administration

### 23.1 Goals

- **Administration never slows the data path.** No control-path code
  runs on a reactor core, and no admin request waits on, or adds work
  to, an I/O.
- **Easy to reach.** One command installs a node or a trial cluster;
  the UI works from first boot; every operation is in the UI, the CLI
  and the API alike.
- **One policy point per mode.** Standalone, `hyperstor-mgmt`
  authenticates and authorizes people. Under glidex, glidex's control
  plane does. The cluster API only checks machine identities.
- **Reuse glidex** where it already solved the problem: the installer's
  shape, connectrpc, sessions and tokens, Cedar, `glidex-authd`, and
  the UI stack.

### 23.2 Layers

```
  browser          laptop / script          glidex control plane
     │ HTTPS            │ HTTPS or gRPC           │ gRPC, mTLS
     ▼                  ▼                         │ (admin identity)
 ┌──────────────────────────────┐                 │
 │ hyperstor-mgmt (standalone)  │                 │
 │ UI · sessions · tokens ·     │                 │
 │ PAM (glidex-authd) · OIDC ·  │                 │
 │ Cedar · audit · watch fan-out│                 │
 └──────────────┬───────────────┘                 │
                │ gRPC, mTLS (mgmt identity)      │
                ▼                                 ▼
 ┌───────────────────────────────────────────────────────────────┐
 │ hyperstor-meta ×3/×5: cluster API hyperstor.v1 (:8850)        │
 │ machine identities only; break-glass on a local Unix socket   │
 └───────────────────────────────────────────────────────────────┘
                ▲ map, pushes            ▲ metrics scrape (:8853)
          hyperstord · -vhost · -gw · -s3 (data path, D25)
```

| Layer | Processes | Callers | Credential |
|---|---|---|---|
| Data | `hyperstord`, `hyperstor-vhost`, `hyperstor-gw`, `hyperstor-s3` | clients, peers | mTLS (D25); RDMA only after an authenticated TCP session (§19) |
| Cluster API | `hyperstor-meta` members | `hyperstor-mgmt`, the glidex control plane, `hyperstorctl`, nodes, hosts | client certificate (§23.8); peer uid on the local socket |
| Human edge | `hyperstor-mgmt` (standalone only) | browsers, `hyperstorctl` with a token | session cookie or `hst_` token (§23.9) |

Why three layers (D50): the Raft members stay small and machine-only,
with no browser, PAM or IdP code in them; the human edge is stateless
and can be restarted, scaled or upgraded without touching the map.

### 23.3 Performance rules

These rules are D58.

| Rule | Why |
|---|---|
| Control-path threads (tokio, D3) run on the node's non-reactor cores: the units set `CPUAffinity=` to the complement of the reactor cores (H7). | A reactor owns its core (D7, D49); an admin request that lands there adds tail latency to every I/O on it. |
| Reactors count into per-core counters with plain atomic adds; the daemon's admin thread reads them and serves Prometheus on its admin port. Metrics never pass through `hyperstor-meta`. | No syscall, lock or allocation on the I/O path for observability; meta load doesn't grow with the number of volumes. |
| Per-volume series only for the top-k volumes by IOPS or bytes (default 50) and volumes an operator pins; the rest aggregate per pool. | Thousands of volumes × histograms would make scrapes and the admin thread heavy. |
| Reads of the map (status, nodes, pools, PGs, health) are served from the member's local copy, tagged with its epoch; map writes go to the leader. Volume reads come from their header objects in `.meta` (D28), never through Raft. Each operation declares read or write (as glidex grpc.md G-D6). | Most admin traffic is reads; serving them locally needs no quorum round and spreads the load over the members. A caller that needs read-your-writes passes the epoch it last saw. |
| `hyperstor-mgmt` holds one watch stream per member it uses and fans it out to every browser and CLI watcher. | A hundred open tabs cost meta one stream, not a hundred. |
| Long operations (drain, rebalance, flatten, pool delete, CA rotation) are `Operation` resources with progress, watched rather than waited on. | No request holds a connection or a task for minutes; a dropped client reattaches by id. |
| The API port has a per-identity request rate limit (default 200/s, burst 1000) and a bound on concurrent streams. | A runaway script can't starve glidex's driver or the nodes' map traffic. |

G17 checks the first rule under load.

### 23.4 Installer

`hyperstor-install` follows the shape of glidex's installer
(glidex installer.md, *Philosophy*): one linear file, non-interactive and
convergent. Every run detects what is missing or outdated and fixes just
that; flags are saved to `/etc/hyperstor/install.conf` and reused.

| Flag | Effect |
|---|---|
| `--role storage\|compute\|both` | Units and devices for the role (D7, §18.3) |
| `--devices LIST` | Devices for the engine: NVMe namespaces, SAS and SATA disks, hard disks with `:log=<flash partition>` (D29) |
| `--meta` | Also run a `hyperstor-meta` member |
| `--mgmt` | Also run `hyperstor-mgmt` and the UI (standalone only) |
| `--join URL --token T` | Join an existing cluster (§23.4.2) |
| `--all-in-one` | One-node trial: meta, `hyperstord` on the given devices, mgmt and the UI, a pool with one copy |
| `--prebuilt` | Use release binaries already built in this checkout (as glidex's `--prebuilt`) |
| `--vhost`, `--role vhost` | Also run `hyperstor-vhost`, or only it, on a host that runs VMs (D142); `--vhost-user NAME` names who may use it (D144) |

- **Under glidex**, `glidex-install --hyperstor …` runs
  `hyperstor-install` with the matching flags, and `glidex-stord`
  hands over the devices (§18.2). Without glidex, the installer writes
  udev rules and `DeviceAllow=` drop-ins itself.
- **Units** are as §18.2, plus `hyperstor-mgmt.service`. All run as
  the `hyperstor` user with systemd sandboxing (§23.10).

#### 23.4.1 Preflight

`hyperstor-install check` runs every check without changing anything,
and the install runs it first. Each line is pass, warn or fail, with
the reason and the fix:

- kernel version, io_uring with `uring_cmd`, NVMe generic character
  devices (`/dev/ng*`) for the given namespaces (D29);
- the IOMMU, `memlock` limits, RDMA devices and their GIDs, the
  storage NIC's NUMA node against the devices' (H7);
- free cores for reactors after the host's reservations;
- clock sync (leases depend on bounded drift, D48);
- that no device given is mounted, in use by LVM or another datastore.

A check that rests on a host claim cites its host check (G…).

#### 23.4.2 Bootstrap and join

1. `hyperstorctl cluster init` on the first meta host creates the root
   and intermediate CAs (§23.8), the first meta member, and prints the
   UI's one-time claim URL (§23.7) and a join command.
2. A **join token** is single-use, expires in 1 h by default, and is
   stored only as its SHA-256. It carries the root's SHA-256, so the
   joining node pins the cluster before it sends anything.
3. The joining node generates its own key and sends a CSR with the
   token. The key never leaves the node. The intermediate signs a node
   certificate for the role the token allows (node, meta member, mgmt,
   s3 gateway).
4. `hyperstorctl token create --role …` makes more tokens;
   `token list` and `token revoke` manage them.
5. Tokens are rows of the map service's replicated store (D81), made,
   used and revoked by the leader (D82). Before the group has a map, the
   first member keeps the `meta` tokens for the other members itself.

Why tokens rather than copying certificates (D57): the operator moves a
short, expiring, single-use string, never a key; this matches glidex's
join (clustering.md §5.2), so operators learn one model.

#### 23.4.3 Changing a host's roles

D288 (settled 2026-11-10; point 248). Built (I348): the member id
defaults to the next free one, and `--raft-port` (8851) is where the
promoted member talks Raft; a demote is also refused where an S3
gateway runs on the member's certificate, and for a node whose devices
aren't on consecutive ports of one address, as a joined node's are.

A host runs a **map service member** (`hyperstor-meta`, with mgmt, the
web UI and authd beside it, the `api`, `mgmt`, `web` and `admin`
certificates, the intermediate CA's key and the cluster key, §23.8), a
**storage node**, or both; and maybe a gateway or `hyperstor-vhost`.
Members change one at a time at run time (D87), but a host's role
couldn't: a storage host that joined with a token can't also become a
member (`--join --role both` takes its node from the cluster file),
and a member can't go back to storage only. The installer changes
them:

| Change | On the host | What happens |
|---|---|---|
| **Promote**: storage host → member and storage | `hyperstor-install install --role both --token-file F --member-id N` with a `meta` token, on a host joined with `--role storage` | Joins as member N (the member certificates, the intermediate, the cluster key and the member list, as a `meta` join does, I86); keeps its node, its node certificate and its devices; starts `hyperstor-meta`, then adds itself (`AddMember`, a learner until it has the log, then a voter), mgmt and authd |
| **Demote**: member and storage → storage | `hyperstor-install install --role storage` on a member host | Removes itself (`RemoveMember`) and waits until it is out of the group; gets its node a certificate of its own (a `Join` with its node's addresses, which gives that node back, D167); revokes and deletes its member certificates; deletes the intermediate's key, the cluster key and the store (`meta/`), after a backup (`map backup`); stops and removes `hyperstor-meta`, mgmt and authd; keeps its node and devices, now joined through another member's API |
| **Demote** a member without storage | `hyperstor-install uninstall` | As above, then the rest goes as an uninstall does |
| **Replace** member N on host A by host B | Promote B (new id), then demote A | One change at a time keeps a majority throughout (D87) |

- **Refused:**
  - a demote that would leave fewer than three voters, unless
    `--force`, as `member remove` is (D87);
  - a demote of the host holding the root CA's key, until the key has a
    backup and has left the host (`root-backup --remove-key`, then
    `root-restore` on another member). Why: the root is how the
    intermediate is rotated and new members trust the cluster (D70,
    D84);
  - a promote that would make an even count, unless `--force`: four
    members survive one failure, as three do, and commit slower.
- **The leader** may be demoted: removing it ends its term once the
  change commits, and the others elect a leader (openraft's joint
  consensus, D87). The installer waits for a new leader before it stops
  anything.
- **Member lists follow.** Storage hosts, gateways, `hyperstor-vhost`
  and glidex hold the members' map addresses and reach the leader by
  redirect (D72). Every installer run, the daily renewal included,
  takes the list from `ListMembers` through the API, rewrites the
  services' options where it changed and restarts them, one at a time.
  A member's own list (`hyperstor-meta --member`) stays as installed: it
  only forms a new group, and rewriting it would restart members.
  Until a host's next run, it reaches a member it still lists. So a
  replacement keeps the old member running until every host has run
  the installer once, or the operator runs it on each.
- **Keys after a demote.** The demoted host deletes its copies, but a
  copy taken before is still good: the intermediate is rotated, as
  §23.8 has for a member that leaves (not built yet), which needs the
  root's key on a member; until then, a demote is as safe as the trust
  put in that host. The cluster key can't be rotated (I268).
- **mgmt and the UI** run on every member: a demote takes them off
  that host, and a proxy or tunnel pointed at it must be pointed at
  another member's (§23.7).

Why the installer, not `hyperstorctl`: each change starts, stops and
deletes services, keys and data on the host itself, which only the
installer does there; the map service's part (`AddMember`,
`RemoveMember`, a node's certificate) goes through the cluster API, so
every step is audited. Why `--role`: the installer is convergent, and a
role is what it already saves in `install.conf`; a change of role is a
run with another one.

Settled with the owner (05-decision-log.md point 248):

1. The installer runs `AddMember` and `RemoveMember` itself: one command
   per host, and a demote can't stop the member before the group has
   let it go.
2. A demoted member's store is deleted after a `map backup`: a stale
   copy could only mislead.
3. The intermediate's rotation on a demote is a follow-up of its own:
   it needs the root's key on a member, and rotation is designed but not
   built.

On the pilot: replacing member 3 (on a host whose map address sits on
glidex's bridge) by the new storage-only host is a promote of that host
with id 4, then a demote of member 3's host to storage only.

### 23.5 Cluster API

- **`hyperstor.v1`** on port 8850, served by every meta member with
  **connectrpc** (D51), pinned to the same exact version glidex uses
  (glidex grpc.md G-D10). One service answers gRPC, gRPC-Web and
  Connect JSON, so curl works without a separate REST layer.
- **Protos are the source of truth**, as glidex grpc.md G-D3, G-D9 and
  G-D11: `buf lint` and `buf breaking` in CI, JSON names equal the proto
  names, Rust code generated in `build.rs` without `protoc`.
- **Services:** those of §16, plus `OperationService` (get, list,
  watch, cancel), `TrashService` (§23.9.3), `TokenService` (join
  tokens), `CaService` (intermediate rotation, certificate list and
  revoke) and `AuditService` (read).
- **Mutations take an idempotency key.** A retried create returns the
  first result instead of a second volume.
- **Errors** carry stable reasons in `ErrorInfo.reason` (`pool_full`,
  `lease_held`, `not_empty`, `reauth_required`), the same on every
  transport (glidex grpc.md G-D5).
- **Versioning:** `hyperstor.v1` changes only compatibly; a breaking
  change is a `v2` package served beside `v1`. Rolling upgrades (§18.4)
  run mixed versions.
- **Listeners:** TCP 8850 on the management network only (D59), and
  `/run/hyperstor-meta/api.sock` on each meta host for break-glass
  (§23.6).

### 23.6 CLI

`hyperstorctl` covers every operation of the cluster API (D55).

- **Generated from the protos**: one command per RPC. A CI check fails
  when an RPC has no command, so the CLI and the UI never drift apart.
- **One binary, three ways in:**

  | Where | Transport | Credential |
  |---|---|---|
  | On a meta host | `api.sock` | peer uid (`SO_PEERCRED`); members of `hyperstor-admin` only |
  | Operator's machine, standalone | HTTPS to `hyperstor-mgmt` | `hst_` token from `hyperstorctl auth login` (OIDC device flow or PAM), server certificate pinned on first use as `gxctl` (glidex security.md decision 18) |
  | Node or automation | gRPC to meta | client certificate (§23.8) |

- **Break-glass:** the local socket works with mgmt, OIDC and the
  network all down. Its use is audited like any other call.
- **Output:** tables for people; `-o json|yaml` for scripts; stable
  exit codes and the API's error reasons.
- **Long operations** print an operation id; `--wait` follows progress
  from the watch stream, and `hyperstorctl op watch <id>` reattaches.
- **Safety:** destructive commands confirm or take `--yes`; they need
  step-up (§23.9.2); drain, rebalance and pool delete support
  `--dry-run`, which prints what would move or be removed.
- Shell completion and `--help` come from the proto comments.
- **Under glidex**, the everyday path is `gxctl datastore …`
  (16-glidex.md §17), so Cedar applies once. When glidex manages the
  cluster, `hyperstorctl` on a glidex node reaches only the local
  socket (break-glass) and read-only status; other admin calls are
  refused with a reason pointing to `gxctl`.

### 23.7 Management service and web UI

`hyperstor-mgmt` runs only when hyperstor is standalone. It is
stateless: its sessions, tokens, users, role links and site policies
are rows in `hyperstor-kv` when that exists (D35), and until then in
meta's ReDB through a dedicated table written only by mgmt's identity.

- **Serves** the built UI and the human API on 8852 (HTTPS), proxying
  RPCs to meta as its own mgmt identity after checking the person.
- **Stack** as glidex web-ui.md: React, Vite, TypeScript, Tailwind,
  bun, Connect-ES over gRPC-Web. Shared components come from a package
  extracted with the auth code (D52).
- **Footer** as glidex web-ui.md's: under every signed-in page, the
  build of the mgmt serving it (its release tag, or the release before
  it and the commits since, its branch and commit), linked to the code
  on GitHub. mgmt takes it from git when it is built and returns it in
  `/auth/whoami`, so the footer names the installed binaries, not where
  the UI's files were built.
- **Browser protections** as glidex security.md §5.6: `Host`
  allowlist, Origin check, CSRF on cookie calls, `SameSite=Strict`
  cookies, CSP without inline scripts, HSTS with a configured
  certificate, self-signed certificate by default with first-use
  pinning in `hyperstorctl`.
- **Reaches the leader itself** (D276): mgmt asks the member list
  every 5 s, and as soon as a call meets a member that doesn't lead,
  and sends every call to the leader first, then to the others. Every
  host's mgmt serves changes, logins and audit lines, with no browser
  redirect; `--api` needs one member.
- **Behind a proxy or a CDN** (D277): the operator's certificate from
  any CA (Cloudflare's Origin CA, the organization's) with its chain;
  its DNS names are allowed `Host` names, and `--hosts` adds others.
  A proxy is known by a client certificate from `--proxy-ca`
  (Cloudflare's Authenticated Origin Pulls, the organization's) or by
  address (`--trusted-proxies`); only a proxy's `CF-Connecting-IP` or
  last `X-Forwarded-For` entry names the person's address.
  `--proxy-only` serves proxies and loopback alone. Any host's mgmt
  serves any request, an OIDC login's return included: the login's
  state is in the map service (D278).
- **First boot:** no default password. `cluster init` prints a claim
  URL with a one-time code (valid 1 h); opening it creates the first
  `security-admin` (a PAM user or an OIDC subject).

**Pages:**

| Page | Shows | Actions |
|---|---|---|
| Overview | health with reasons (§16), capacity, IOPS and latency | — |
| Nodes and devices | the map service member on the node's host (D282), role, reactors, NUMA, device wear and capacity (free and total space, D156, I193), peer reports (D40) | drain, out, in, add device, weight, class, remove device (with its plan first) |
| Pools | kind (replicated, EC, cache, D45), fullness against D27's thresholds (usable free and total space, D156, I193), PG states | create, change (classes, name, domain, a split, autoscale, a cache pool's grace), the devices it uses, thresholds, delete (forced: with its volumes, after a dry run) |
| PGs | a state map by pool (D154: a cell per PG, sent again only when the map changes; by failure domain later), each cell shaded by the bytes on its primary (D155) | pick a PG for its acting and up sets |
| Volumes | size, allocated, lease holder, QoS and class (D46), snapshots and clones | create, expand, snapshot, clone, flatten, QoS, delete to trash |
| Hosts and exports | bare-metal identities, NVMe-oF exports, allowlists | export, revoke, rotate CHAP |
| S3 (later) | buckets, access keys | create, revoke |
| Operations | running and recent operations with progress | cancel |
| Networks | the cluster's networks (kind, trust, match, endpoints on each; 26-networks.md, D284), each device's endpoints and whether peers reach them (D210, D215), the nodes' ports with their role, state, speed, traffic and weight (D237) | add, change and remove a network, set a device's endpoints, set a port's role |
| Members | the map service's members: voter or learner, the leader, the term and each member's Raft state in the leader's view (D281: in step, behind, no reply), the storage nodes on its host (D282) | add, remove |
| Trash | deleted volumes and snapshots with their purge time | restore, purge now |
| Security | users, roles, tokens, site policies, certificates | grant, revoke, rotate intermediate |
| Audit | every mutation (§23.9.4) | filter, export |

- **Every action shows its `hyperstorctl` command**, so the UI teaches
  the CLI.
- **Charts** come from mgmt's sampled counters (1 s to 5 s, kept 1 h).
  Grafana dashboards ship as JSON for longer history.
- **Under glidex**, mgmt runs as well (D305, amending D50): people sign
  in at their glidex directory, with its passkeys or IdP, and their
  grants come from its feed (§23.12). glidex's UI shows a tenant's
  volumes, buckets, keys and usage inline through mgmt's API, and links
  each hyperstor datastore here for the cluster's administration
  (D310); tenants see their volumes as glidex disks too (16-glidex.md
  §17, §20).

### 23.8 Identities and certificates

**Two-level CA** (D53):

| CA | Key lives | Lifetime | Signs |
|---|---|---|---|
| Root | on a trusted host, readable by root only (`/etc/hyperstor/pki` on the map service's host, D70); a site may move it offline instead | 10 years | intermediates only |
| Intermediate | every meta member, as the encrypted systemd credential `hyperstor-ca` (TPM-sealed where the host has one, G18) | 1 year, rotated at 9 months | node, meta, mgmt, admin, host and s3 certificates |

- **The root's key is backed up** encrypted with a passphrase, and may
  then leave the host (D84): `hyperstor-install root-backup`, and
  `root-restore` to bring it back when the intermediate is due. Installs
  warn until it is.
- **Everyone trusts the root.** Rotating the intermediate needs no
  change on any node: the new one is signed by the root, pushed with
  the map, and leaves renew under it. That is the simpler rotation the
  owner asked for: the frequent operation never touches trust stores,
  and the rare one (root rotation) is a planned procedure.
- **The cluster key** (D162) sits beside the intermediate's on every
  meta member, handed to the service as a credential as the
  intermediate's is. The installer makes it with the root, a member that
  joins gets it in the join's answer, and `root-backup` keeps it with the
  root. It seals the S3 keys' secrets in the replicated store (D231);
  volumes' keys come later. It isn't rotated with the intermediate: what
  it sealed would no longer open (I268).
- **When a meta member leaves**, the intermediate is rotated, as glidex
  rotates its cluster CA (clustering.md D20): the departed member's
  copy becomes worthless once its certificates expire.
- **Under glidex** the root is still hyperstor's own (D60), not glidex's
  cluster CA. glidex rotates its CA whenever a server leaves; chaining
  under it would make every such rotation re-sign hyperstor's
  intermediate. glidex's control plane holds an admin certificate
  issued by hyperstor, stored as a glidex credential (16-glidex.md §17).

**Leaf certificates** (D56):

- Identity in a URI SAN: `hyperstor://<cluster id>/<role>/<id>`, with
  role `node`, `meta`, `mgmt`, `admin`, `host`, `s3` or `metrics`. The
  API authorizes by role (§16): `host` may only attach volumes exported
  to it; `node` may report and read the map; `mgmt` and `admin` may
  administer; `s3`, a gateway, may read S3's keys, which no other role
  may (D233); `metrics` may only read the map service's `/metrics`
  (04-decisions.md D234).
- P-256, 30 days, renewed automatically at two thirds of their life
  over the existing mTLS session. Short lives keep revocation small.
- **Revocation:** a serial denylist in the map, pushed with map updates
  like the blocklist (D24). Entries drop once the certificate would
  have expired.

### 23.9 People: authentication and authorization

#### 23.9.1 Authentication (standalone; from glidex directories, §23.12)

Reused from glidex (D52), from a crate extracted into the glidex
repository (`glidex-auth`) and taken as a pinned git dependency, as
`glidex-raftstore` is (D20):

- **PAM** through `glidex-authd`, the small root helper (glidex
  security.md §5.3); users created on first login when they pass the
  allowed groups.
- **OIDC** with an external IdP; a login's state until the IdP sends the
  browser back is in the map service, so any mgmt finishes it (D278).
- **Sessions** (browser): 256-bit ids, only hashes stored, idle 30 min,
  absolute 12 h, `HttpOnly; Secure; SameSite=Strict` (glidex
  security.md §5.5).
- **Tokens** (CLI, automation): `hst_` prefix, only the SHA-256 stored,
  shown once, required expiry (default 90 days, at most 365); personal
  or service account.

#### 23.9.2 Authorization

- **Cedar**, evaluated in `hyperstor-mgmt`, deny by default: every RPC
  maps to one action, and an RPC without one fails a test.
- **Shipped roles:**

  | Role | May |
  |---|---|
  | `viewer` | read everything except secrets and audit |
  | `operator` | viewer, plus drain, out, in, snapshots, operations, restore from trash |
  | `storage-admin` | operator, plus pools, volumes, exports, QoS, purge |
  | `security-admin` | users, roles, tokens, site policies, certificates, audit |

  Storage and security are separate roles so no one administers data
  and covers their own tracks by default; a site may grant both.
- **Step-up** (fresh login within 10 min, as glidex): pool delete,
  a pool's copies changed (24-pools-and-devices.md §27.3.5, D287), purge from trash, a delete that removes snapshots, blocklisting, CA
  rotation, token and key creation, policy writes.
- **Site policies** in Cedar, stored with history, as
  glidex security.md §7.6; they can add restrictions but not relax the shipped `forbid`s.

#### 23.9.3 Trash

Deletes are soft (D54):

- A deleted volume or snapshot goes to the trash: its lease is
  dropped, it disappears from lists and can't be attached, its space
  still counts against the pool.
- It is purged after the grace period (default 24 h, per pool, 0
  allowed), or at once by `purge` (step-up).
- `restore` brings it back under its name, or a new one if the name
  was reused.
- **A full pool** (D27) purges the oldest trash first only when the
  pool policy allows it (`trash_on_full = purge|keep`, default `keep`):
  keeping it never loses data an operator may still want; purging keeps
  writes flowing.

Why: a mistyped delete of a volume is the most common unrecoverable
admin error; a day's grace costs only space, and the space is visible.

#### 23.9.4 Audit

- Every mutation, every denied request and every break-glass call is
  recorded: time, principal, source address, operation, target,
  result, and the operation id.
- Written to journald and to append-only audit objects in `.meta`
  (D28, D112), readable by `security-admin` and by site policies that
  grant `readAudit`. Kept a year by default, pruned a day at a time
  (D113).
- **As built** (D147, D148, I185):
  - each map service member writes its own records to numbered parts,
    `audit/YYYY-MM-DD/<member>-<N>`, starting the next at 16 MiB and
    after a restart; `audit/days` lists the days and `audit/parts`
    their parts;
  - a read merges a day's parts by key (the time counted down, so
    newest first); the cursor is the day and the last key, opaque to
    hyperstorctl and the UI;
  - a day written before parts is one object, `audit/YYYY-MM-DD`,
    read and pruned with the rest;
  - refusals of one principal's operation with one error code are
    recorded ten a minute; the rest are counted into one `Refused`
    record when the minute ends ("refused N more … calls"); journald
    has every one.
  - a record is one line of JSON whose `time` is a number of Unix
    seconds with milliseconds; one whose `time` is a string holding
    that number, as hyperstor-mgmt wrote until I190, reads the same.

Why: a client retrying refused calls once grew a day's single object
past the 64 MiB frame and stalled its PG's backfill (I177). Parts bound
every object, and summaries bound what a storm adds without losing the
evidence.
- Under glidex, glidex's audit (glidex security.md §10) records the
  person; hyperstor's records glidex's admin identity and the request
  id glidex sends, so the two can be joined.

### 23.10 Hosts and secrets

- **Networks:** the cluster API listens on the management network only
  (D59); mgmt's 8852 on the management or an operator network; the
  daemons' admin port 8853 (metrics and health) on the management
  network. Data networks carry no admin port.
- **Units:** the `hyperstor` user, `NoNewPrivileges=`,
  `ProtectSystem=strict`, `PrivateTmp=`, `CPUAffinity=` per §23.3,
  and no capability beyond `LimitMEMLOCK` (§18.2).
- **Secrets** (CHAP keys, S3 secrets, the intermediate key): encrypted
  at rest with a key from a systemd credential, shown once, never
  logged, redacted from audit records and errors.
- **No secret in a URL or a command line:** tokens are read from the
  environment, a file or a prompt; join tokens from `--token-file` as
  well as `--token`.

### 23.11 Order of work

| Step | Delivers | Depends on |
|---|---|---|
| AD0 | connectrpc on meta, `hyperstor.v1` protos with `buf` checks, leaf identities and the two-level CA, `hyperstorctl` generated from the protos, the local socket | HS1 |
| AD1 | `hyperstor-install` with `check`, join tokens, `--all-in-one` | AD0 |
| AD2 | `hyperstor-mgmt` read-only: health, map, metrics, watch fan-out; UI pages without actions | AD0; `glidex-auth` extracted (a glidex change) |
| AD3 | UI and CLI writes, Cedar roles, step-up, trash, audit, operations | AD2 |
| AD4 | glidex: a tenant's volumes, buckets, keys and usage per project inline in glidex's UI through mgmt's API, and links from each hyperstor datastore to its mgmt (D310; was a Storage section and `storage-admin` role in glidex's UI) | AD5 |
| AD5 | People from glidex directories (§23.12): `glidex-trust` extracted, offers, redeem and the feed, logins through a directory, grants from the feed, `hyperstorctl` logins, the account picker, reports | AD3, HS15's records (20-tenants.md §24.9) |

### 23.12 People from glidex directories (D305–D311)

Settled 2026-11-12 (05-decision-log.md points 267–273). Built: the
trust, offers, redeems and ends, the feed and what it makes, and reports
(I380, #360 stage 4), with these choices open for review: the records
and the replica in the map service's settings table, which checks the
handshake (I381); the cluster key made on first use; tenants and
projects gone from the feed kept unlinked (I382); names and clashes
(I383); a directory's realm (I384). Settled: reports send used bytes
from the space counters, 0 until they exist (D313, #364); a directory's
pin of mgmt's certificate follows renewals through the reports, and an
offer names mgmt's direct address, not a proxy's (D312, glidex#501). Built
too: logins through a directory in a browser and by `hyperstorctl auth
login`, its access and API tokens, roles from its links and, for the
operator tenant's members, from here, and partitions past `max_staleness`
(I387, #360 stage 5), with these choices open for review: a person's
account here (I388), local and directory accounts apart (I389), links to
roles (I390), an access token's age (I391), the CLI's device grant (I392),
`max_staleness` (I393), and where logins come back (I394). Built too,
hyperstor's half of AD4 and the driver's binding (I395, #360 stage 6),
with these choices open for review: only hyperstor-mgmt's own identity
acts for people (I396), management clients bound to realms and naming
glidex's projects (I397), accounts in one browser (I399), and the pickers
as views (I400); glidex's UI reaches mgmt through glidex's server (D314,
settled 2026-11-14, which replaced I398's cross-origin calls). Not yet: the
email field, and glidex's half (below). With realms as
directories, tenants as tenants and projects as projects (20-tenants.md
§24.9, D299–D304), `hyperstor-mgmt` takes people from glidex's
directories the way a glidex cluster does (glidex 19-tenancy.md §7,
§12): a person signs in once at their organization's directory, with
its passkeys, IdP or password, and reaches glidex and hyperstor alike.
Standalone logins (§23.9.1) stay.

**mgmt runs under glidex too (D305; amends D50 and §23.7).** It is
hyperstor's human edge whether or not glidex is installed. Why: one
hyperstor cluster may serve several glidex clusters (glidex storage.md
S24), S3 clients and bare-metal hosts, so no one glidex UI can be its
administration; D50's "glidex's control plane is the human edge"
assumed one glidex per hyperstor.

**The trust (D306).** To a directory, a hyperstor cluster is one more
cluster, through glidex's own handshake and feed:

| Step | glidex (19-tenancy.md) | hyperstor |
|---|---|---|
| Cluster key | a cluster's ES256 key, sealed by the host, signing every request to a directory (D63) | the same, made by the map service's leader on first use (I380), kept in the map service sealed with the cluster key (D162); any mgmt signs with it |
| Offer | `gxctl admission offer --tenant --directory --cap` prints a `gxo_` token (D64) | `hyperstorctl admission offer --tenant SLUG --directory ISSUER [--realm R] --cap POOL=BYTES,…`, the same token, naming mgmt's address and certificate |
| Redeem | the tenant's admin runs `gxctl tenant opt-in --offer` at the directory (D65) | unchanged: the directory redeems at mgmt, which pins the directory's keys; the first redeemed offer of a directory makes its realm (or links the one named) |
| Feed | the cluster pulls the filtered snapshot every 30 s (D67) and keeps it as a read-only replica (D68) | mgmt with the map service's leader on its host pulls it (D276 finds it) and writes the replica as rows of the map service's store, where people's records are now (D98); every mgmt reads them |
| Reports | admission, cap, counts of VMs, disks, networks (D70) | admission, caps per pool, counts of volumes, buckets and bytes per project; glidex shows them on the tenant's page (a glidex change) |
| Ending | either side ends it (D66) | the same; the realm and its tenants stay, unlinked, until an operator removes them |

The operator's own directory is trusted the same way when the operator
takes the cluster over (`hyperstorctl cluster set-operator`, as glidex's
`gxctl cluster set-operator`, §12.1), so operator staff sign in with
their organization's IdP. The cluster side of all this is in
`glidex-control-plane` today; it moves into a crate, `glidex-trust`
(relying party: the cluster key, signed peer assertions, redeem, feed
client, replica tables' shape, token checks), as `glidex-auth` did
(D52). Why one handshake: organizations already trust their glidex
clusters this way, and a second mechanism would be a second thing to
audit and get wrong.

**Logins (D307).**

- **Browser:** mgmt is the directory's OIDC client `cluster:<hyperstor
  cluster id>` (glidex D69). The login page offers "Sign in with
  `<directory>`" for each trusted directory, and an email field that
  finds the directory by its tenants' verified domains from the feed
  (glidex D61), then the local methods. Authorization Code with PKCE;
  the login's state in the map service as now (D278). mgmt checks the
  ID token against the JWKS of the directory's last feed, `aud`, the
  user in the replica and enabled, and makes a session as now (§23.9.1),
  bound to the directory and its `sid`.
- **Passkeys and MFA** are the directory's: mgmt never handles WebAuthn.
  `amr` says whether the login was multi-factor; step-up asks the
  directory for a fresh login (`max_age`, `prompt=login`), so a passkey
  answers it as a password does (glidex passkeys P5).
- **CLI:** `hyperstorctl --mgmt URL` takes a directory's access token
  (`aud` = this cluster, glidex D55), checked offline as for a session.
  `hyperstorctl auth login --directory URL` gets one by the directory's
  device grant and keeps its refresh token, as `gxctl auth login`;
  `gxctl auth token --cluster ID` hands one over too. A glidex API token
  (`gxt_`) that lists this cluster is accepted from its hash in the
  feed (glidex §7.4).
- **Logout** ends mgmt's session and tells the directory (glidex D72).
- **Local logins stay** (PAM through `hyperstor-authd`, mgmt's own OIDC,
  `hst_` tokens, the claim code): for a cluster without glidex, and for
  break-glass when a directory is unreachable, as glidex keeps PAM and
  host sockets on their cluster (glidex D23, §7.5).

**Who may do what (D308).**

| From | Grants | Kept |
|---|---|---|
| Members of the operator tenant | cluster-wide roles (`viewer` … `security-admin`) and `role@realm` | in mgmt, granted by a `security-admin`, to directory users and teams of the operator's directory only (a Cedar `forbid` for anyone else, as glidex D4) |
| A glidex tenant's links | `tenant-admin` → `storage-admin@tenant` and `security-admin@tenant`; `tenant-auditor` → `viewer@tenant` with its tenant's audit | the feed: written at the directory, never in mgmt (glidex D12) |
| A glidex project's links | `owner` and `editor` → `storage-admin@project`; `operator` → `operator@project`; `viewer` → `viewer@project` | the feed |
| Local accounts | as now (§23.9.2) | mgmt |

A directory user's grants come from the feed, so a person removed or
disabled at their IdP loses them as the feed arrives (glidex D20). Step-up
actions (§23.9.2) stay the same list.

**Partitions (D309)**, as glidex D13: cut off from a directory, mgmt
keeps that directory's sessions until they end and its access tokens
until they expire; past the directory's `max_staleness` (default 1 h,
the operator's) it refuses them, since a revocation may be missing.
Local logins and other directories' people keep working; storage never
depends on it.

**One sign-on, two UIs (D310).** mgmt's UI already uses glidex's
components (D97). It gains glidex's account picker (one account per
directory in one browser, glidex D71) and a tenant and project picker
beside the realm picker; people, teams, projects and their roles link to
the directory's own pages, since those are written there.

glidex's UI shows a tenant's storage inline as well: its volumes,
buckets and S3 keys, and usage per project, through mgmt's API, on the
tenant's and each project's pages (AD4). The page asks glidex's own
server, which relays a fixed set of calls to mgmt (D314): it asks its
directory for an access token addressed to the hyperstor cluster for the
person's chain (glidex D69: the directory gives one to members of tenants
opted in to it), and calls mgmt at the trusted cluster's `api`, pinned as
the offer said (D312), with it as a bearer token, server to server; mgmt
answers under the person's grants as for any other caller. The browser
never holds the token, glidex's Content-Security-Policy needs no change,
and mgmt sends no CORS headers. Administering the cluster (pools,
devices, nodes, members) stays in mgmt's UI, linked from each datastore.
Why: tenants work where their VMs are, in glidex, and operators where
the hardware is; both read the same API under the same grants, so the
two UIs can't disagree on what a person may see. The shared pages are
components of the glidex UI kit, written once.

**glidex's datastore driver (D311).** A glidex cluster still acts as its
own `mgmt` identity (glidex storage.md S20), now naming each disk's
project. The operator binds that identity to the realms it may act in,
as hosts and gateways are bound (D251, I370); the map service refuses a
project outside them. Why: one glidex cluster can't make volumes in
another directory's projects, whatever it names.

**The contract with glidex (#360 stage 6; I395–I400).** What glidex's
half builds on:

| What | How |
|---|---|
| The driver's identity | as now: `hyperstorctl token create --role mgmt --name NAME`, joined by `TokenService.Join`, `hyperstor://<cluster>/mgmt/NAME`, over mutual TLS to the cluster API (not mgmt). It may not be named `dashboard` or `web`, and never reads mgmt's rows or acts for a person (I396). |
| Binding it | `hyperstorctl realm client --client NAME --realm R[,R...]` (`RealmService.SetClientRealms`); `--realm any` unbinds. The directories' realms, and `default` for its shared image volumes. Unbound, it acts in the whole cluster as before. |
| Calls it makes | `ClusterService.ListVolumes`, `CreateVolume`, `CloneVolume`, `CreateSnapshot`, `ListSnapshots`, `DeleteVolume`, `ExpandVolume`, `GetStatus` (still allowed bound, for its pools' space), `TrashService.ListTrash` and `PurgeFromTrash`: bound, each reaches only its realms' tenants and projects. |
| A disk's project | `CreateVolume.directory_project` and `CloneVolume.directory_project` (strings): glidex's project id, resolved to the project linked to it among those the call reaches. Not with `project`. The shared image volume names no project (it lands in `default`, which the client must be bound to); each disk is a clone of its snapshot in its own project (D300). |
| Refusal reasons (`ErrorInfo.reason`) | `outside_realm` (a project, volume or call outside its realms), `tenant_required` (no project, and `default` not bound), `not_found` (no project linked to that id within reach, or a volume outside it), `ambiguous_project` (an unbound client trusted by two directories using the same id), `invalid_argument` (`project` and `directory_project` both). |
| Pages in glidex's UI (D314) | glidex's server relays them, server to server: it calls mgmt at the trusted cluster record's `api`, pinned to the offer's pin (D312), with `Authorization: Bearer <access token>`, the directory's `at+jwt` with `aud` = this cluster's id minted for the person's chain (glidex D69), checked as in stage 5. No `Origin` and no cookie, so no CSRF value: a bearer call needs none. The fixed set: `POST /api/hyperstor.v1.ClusterService/ListVolumes {"pool":N}` (each volume's `tenant`, `project`, `project_name`, `size`), `S3Service/ListS3Buckets {}` (`objects`, `bytes`, `project`), `S3Service/ListS3Keys {}` (`project`), `TenantService/ListTenants {}` (projects, `glidex_id`, `default_project`), and `GET /auth/whoami`; Connect's JSON (`Content-Type: application/json`). Usage per project is summed from these: provisioned bytes from volumes' `size`, buckets' `bytes`; used bytes come with HS15's counters (#364). Answers hold only what the person's links reach (D308): a cluster-wide call such as `GetStatus` is refused (`permission_denied`). No token: `401 unauthenticated`; a stale directory: `503 directory_stale`. Browsers never call mgmt across origins: mgmt sends no CORS headers, and refuses a write from another site's `Origin` (`403 bad_origin`). |
| Links to administration | a datastore links to its mgmt: an optional URL on the datastore record, else the trusted cluster's `api` from the redeem (glidex's choice). mgmt's own pages link back: tenants and projects to `<issuer origin>/tenants/<glidex id>` and `/projects/<glidex id>`, a person to `<issuer origin>/account`. |

**Changes in glidex** (glidex pull requests): the `glidex-trust` crate;
reports with storage counts; the tenant's storage pages, read through
glidex's server, which relays them to mgmt with an access token for the
hyperstor cluster (D314); links from a
hyperstor datastore to its mgmt; `gxctl auth token --cluster` if it
isn't there. Nothing changes in directories themselves: a hyperstor
cluster is a cluster to them.

**Tests:** `glidex-trust` gets a fake directory (issuer, JWKS, token
endpoint, feed), as `glidex-tls` has a fake tailscaled; `hyperstor-test`
signs in through it in a browser test and with `hyperstorctl`, checks
grants from the feed, a disabled user, a partition past
`max_staleness`, and a local login while the directory is down. One
ignored test runs against a real glidex directory on a pilot host.

**Order of work (AD5):** `glidex-trust` extracted (a glidex change);
the cluster key, offers, redeem and the feed replica; browser logins
through a directory; grants from the feed with `role@tenant` and
`role@project` (with HS15's records); `hyperstorctl` logins; the
account picker; reports; the driver's projects and identity binding.

## Decisions and host checks

D50–D60 are in [04-decisions.md](04-decisions.md); host checks G17 (admin
load against I/O latency) and G18 (sealing the intermediate key) are in
17-milestones.md §22.
