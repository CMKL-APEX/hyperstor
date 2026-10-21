# Administration: installer, API, CLI, UI and security

How hyperstor is installed, administered and secured: the installer, the cluster API, `hyperstorctl`, the management service and web UI, and the identities and checks behind them. Part of the hyperstor design; sections keep their numbers across documents, so `§N` in one document is the same section everywhere (see [README.md](README.md)).

Status: **draft, partly built.** D50–D55 were settled with the owner on 2026-10-21
([decision-log.md](decision-log.md)); D56–D60 were made while drafting
and are open for review. Host checks G17 and G18 are open. Built so far
(implementation-notes.md I41–I52): the cluster API on the map service,
over mutual TLS with identities and roles, audited and rate-limited, with a break-glass local socket; the root and intermediate
CAs and leaf certificates; `hyperstorctl` (status, nodes, pools, PGs,
devices out and in, an I/O test); metrics on storage nodes' admin
ports; and `hyperstor-install` with host checks, the all-in-one trial
cluster, multi-host installs from a shared cluster file (before join
tokens) and a daily renewal timer. This
section refines operations.md §16, §18 and §19, which point here.

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
   certificate for the role the token allows (node, meta member, mgmt).
4. `hyperstorctl token create --role …` makes more tokens;
   `token list` and `token revoke` manage them.

Why tokens rather than copying certificates (D57): the operator moves a
short, expiring, single-use string, never a key; this matches glidex's
join (clustering.md §5.2), so operators learn one model.

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
  (glidex.md §17), so Cedar applies once. When glidex manages the
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
- **Browser protections** as glidex security.md §5.6: `Host`
  allowlist, Origin check, CSRF on cookie calls, `SameSite=Strict`
  cookies, CSP without inline scripts, HSTS with a configured
  certificate, self-signed certificate by default with first-use
  pinning in `hyperstorctl`.
- **First boot:** no default password. `cluster init` prints a claim
  URL with a one-time code (valid 1 h); opening it creates the first
  `security-admin` (a PAM user or an OIDC subject).

**Pages:**

| Page | Shows | Actions |
|---|---|---|
| Overview | health with reasons (§16), capacity, IOPS and latency | — |
| Nodes and devices | role, reactors, NUMA, device wear and capacity, peer reports (D40) | drain, out, in, add device |
| Pools | kind (replicated, EC, cache, D45), fullness against D27's thresholds, PG states | create, thresholds, delete when empty |
| PGs | a state map by pool and failure domain | — |
| Volumes | size, allocated, lease holder, QoS and class (D46), snapshots and clones | create, expand, snapshot, clone, flatten, QoS, delete to trash |
| Hosts and exports | bare-metal identities, NVMe-oF exports, allowlists | export, revoke, rotate CHAP |
| S3 (later) | buckets, access keys | create, revoke |
| Operations | running and recent operations with progress | cancel |
| Trash | deleted volumes and snapshots with their purge time | restore, purge now |
| Security | users, roles, tokens, site policies, certificates | grant, revoke, rotate intermediate |
| Audit | every mutation (§23.9.4) | filter, export |

- **Every action shows its `hyperstorctl` command**, so the UI teaches
  the CLI.
- **Charts** come from mgmt's sampled counters (1 s to 5 s, kept 1 h).
  Grafana dashboards ship as JSON for longer history.
- **Under glidex**, there is no hyperstor UI. glidex's UI gets a
  Storage section (pools, health, nodes) for a new `storage-admin`
  role; tenants see their volumes as glidex disks (glidex.md §17, §20).

### 23.8 Identities and certificates

**Two-level CA** (D53):

| CA | Key lives | Lifetime | Signs |
|---|---|---|---|
| Root | offline: exported at `cluster init` as an encrypted file for the operator to store; optionally kept as the systemd credential `hyperstor-root-ca` on the first meta host (small sites) | 10 years | intermediates only |
| Intermediate | every meta member, as the encrypted systemd credential `hyperstor-ca` (TPM-sealed where the host has one, G18) | 1 year, rotated at 9 months | node, meta, mgmt, admin and host certificates |

- **Everyone trusts the root.** Rotating the intermediate needs no
  change on any node: the new one is signed by the root, pushed with
  the map, and leaves renew under it. That is the simpler rotation the
  owner asked for: the frequent operation never touches trust stores,
  and the rare one (root rotation) is a planned procedure.
- **When a meta member leaves**, the intermediate is rotated, as glidex
  rotates its cluster CA (clustering.md D20): the departed member's
  copy becomes worthless once its certificates expire.
- **Under glidex** the root is still hyperstor's own (D60), not glidex's
  cluster CA. glidex rotates its CA whenever a server leaves; chaining
  under it would make every such rotation re-sign hyperstor's
  intermediate. glidex's control plane holds an admin certificate
  issued by hyperstor, stored as a glidex credential (glidex.md §17).

**Leaf certificates** (D56):

- Identity in a URI SAN: `hyperstor://<cluster id>/<role>/<id>`, with
  role `node`, `meta`, `mgmt`, `admin` or `host`. The API authorizes
  by role (§16): `host` may only attach volumes exported to it; `node`
  may report and read the map; `mgmt` and `admin` may administer.
- P-256, 30 days, renewed automatically at two thirds of their life
  over the existing mTLS session. Short lives keep revocation small.
- **Revocation:** a serial denylist in the map, pushed with map updates
  like the blocklist (D24). Entries drop once the certificate would
  have expired.

### 23.9 People: authentication and authorization

#### 23.9.1 Authentication (standalone)

Reused from glidex (D52), from a crate extracted into the glidex
repository (`glidex-auth`) and taken as a pinned git dependency, as
`glidex-raftstore` is (D20):

- **PAM** through `glidex-authd`, the small root helper (glidex
  security.md §5.3); users created on first login when they pass the
  allowed groups.
- **OIDC** with an external IdP.
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
  purge from trash, a delete that removes snapshots, blocklisting, CA
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
- Written to journald and to an append-only audit object in `.meta`
  (D28), readable by `security-admin` and by site policies that grant
  `readAudit`.
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
| AD4 | glidex: the Storage section and `storage-admin` role in glidex's UI | AD3, HS3 |

## Decisions and host checks

D50–D60 are in [decisions.md](decisions.md); host checks G17 (admin
load against I/O latency) and G18 (sealing the intermediate key) are in
milestones.md §22.
