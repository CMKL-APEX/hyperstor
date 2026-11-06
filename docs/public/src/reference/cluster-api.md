# Cluster API

`hyperstor-meta --api ADDR` serves the cluster API, `hyperstor.v1`, which
[`hyperstorctl`](hyperstorctl.md) uses. One service answers the
[Connect protocol](https://connectrpc.com/docs/protocol) (JSON or
protobuf over HTTP/1.1 or HTTP/2), gRPC and gRPC-Web, so any of these
work: `curl`, `grpcurl` (with the proto file), generated clients.

The definitions are
[`cluster.proto`](https://github.com/CMKL-APEX/hyperstor/blob/main/crates/hyperstor-api/proto/hyperstor/v1/cluster.proto)
(`ClusterService`) and
[`admin.proto`](https://github.com/CMKL-APEX/hyperstor/blob/main/crates/hyperstor-api/proto/hyperstor/v1/admin.proto)
(`TokenService`, `CaService`, `NodeService`, `OperationService`,
`S3Service`), in
`crates/hyperstor-api/proto/hyperstor/v1/`.

## Security

With `--api-tls`, the API speaks only TLS. Every call but `Join` needs
a client certificate issued by the cluster's CA, whose identity
(`hyperstor://<cluster>/<role>/<id>`) decides what it may do:

| Role | May |
|---|---|
| `admin` (`hyperstorctl`), `mgmt` | Everything |
| `node`, `meta` | `meta` also `GetClusterKey`. Read (`GetStatus`, `ListPgs`, `ListVolumes`, `ListTokens`, `GetOperation`, `ListOperations`, `DrainNode` with `dry_run`), and renew their own certificate |
| `host` | Renew its own certificate; nothing else |
| `s3` (`hyperstor-s3`) | Read, `GetS3Gateway` (S3's keys with their secrets; no other role may call it), and renew its own certificate |
| `metrics` | Read `GET /metrics` (below) and renew its own certificate; nothing else |

A certificate of another cluster is refused, and so is a revoked one:
`RevokeCertificate` (`hyperstorctl cert revoke`) puts its serial on a
list every map service member checks, on the API and between members,
until the certificate would have expired. `hyperstor-install` makes
the certificates in `/etc/hyperstor/pki`.

**Joining.** `Join` is the only call without a client certificate: a
single-use token from `CreateToken` authorizes it. The joining host
pins the cluster's root by the fingerprint the token carries before it
sends anything, and sends a certificate request for a key it keeps;
the token decides the identity it gets. Tokens are kept in the map
service's replicated store, only their SHA-256, so they survive restarts
and work through any member; the leader makes, uses and revokes them
(another member refuses with `not_leader` and changes nothing). Before
the map service has its first map, only its first member makes tokens,
and only `meta` ones, for the other members to join.

**Local socket.** With `--api-socket`, the API is also served on a Unix
socket (`/run/hyperstor-meta/api.sock` when installed). There the
kernel names the caller: root, the service's user and members of
`hyperstor-admin` may do everything, others nothing. It is the way in
when certificates are broken; its use is audited like any other.

Without `--api-tls` the API is plain HTTP and accepts every request:
only for loopback and tests.

**Metrics.** `GET /metrics` on the same port, beside the calls, serves
the map service's metrics in Prometheus' text format (D234, D235): this
member's `hyperstor_meta_leader`, and each scrubbed pool's
`hyperstor_pool_scrub_oldest_seconds{pool,name,kind}` and
`hyperstor_pool_scrub_overdue_pgs{pool,name}`. It needs a certificate
of role `metrics`, or any role that may read; 401 without one, 403 for
`host`. See [Managing a cluster](../admin-guide/managing.md#the-map-services-metrics).

**Audit.** Every change (`SetDeviceIn`, `SetDeviceWeight`, `SetFullRatios`, `AddDevice`, `RemoveDevice`, `SetDeviceClass`, `SetPortRole`, `CreatePool`, `SetPool`, `SetScrubSettings`, `WeakenScrubSettings`, `StartScrub`, `SetPoolDevice`, `DeletePool`, `CreateVolume`, `ExpandVolume`, `DeleteVolume`, `CopyVolume`, `MoveVolume`, the snapshot calls but `ListSnapshots`, `CreateToken`, `RevokeToken`, `CreateS3Key`, `RevokeS3Key`, `SetS3Placement`, `DeleteS3Placement`, `Join`, `RenewCertificate`, `RevokeCertificate`, `DrainNode`, `CancelOperation`) and every refused call is
written as one JSON line on the map service's standard error, which
systemd keeps in the journal:

```sh
journalctl -u hyperstor-meta | grep '{"audit":1'
```

```json
{"audit":1,"time":1792561403.512,"principal":"hyperstor://7311402/admin/local","op":"SetDeviceIn","target":"device 2 up_in=false","result":"ok, map epoch 5"}
```

**Rate limit.** Each caller may make 200 calls per second, in bursts of
up to 1000; more are refused with `resource_exhausted`.

## Calls

Each call is a `POST` to `/hyperstor.v1.<Service>/<Method>` with a
JSON body. Field names are as in the proto file; fields with zero
values are left out of answers.

| Method | Request | Answer |
|---|---|---|
| `GetStatus` | `{}` | `cluster_id`, `epoch`, `health` (`HEALTH_OK`, `HEALTH_WARN`, `HEALTH_ERROR`), `reasons` (`level`, `text`; scrub's checks add the `pool` and its first ten `pgs`), `nodes` with their `devices` and `last_heard_ms` (time since the node's last heartbeat; absent if none since the map service started, and from a member that isn't the leader, which hears none: its answer then notes where the leader is), `pools` with PG counts, `map_address`, `full_ratios` (`nearfull`, `backfillfull`, `full`, in percent); each device's and pool's `fullness` (`ok`, `nearfull`, `backfillfull` for a device, `full`; empty before the devices report) and a pool's `fullest_device` |
| `ListPgs` | `{"pool": 1}` | `pgs`: `pool`, `pg`, `epoch`, `acting`, `up` |
| `GetPgHeat` | `{"pool": 1}`, optionally `"pg": 7` | Per PG, PG n at index n: `read_ops`, `write_ops`, `read_bytes`, `write_bytes` per second over its primary's last 5-s window (0: none in the last 20 s), and `primaries`, the reporting device; with `pg`, `top`: its busiest volumes (`id`, `name`, `pool`, `ops`, `bytes`). The leader answers; another member `unavailable` (`not_leader`) |
| `ListDevicePerf` | `{}`, or `{"device": 1002}` | `devices`: each device's `node`, `kind`, `samples` (the last 15 minutes at 5 s, oldest first: `read_ops`, `write_ops`, `read_bytes`, `write_bytes` per second as primary, `reads` and `commits` service time with `p50_us`, `p99_us`, `max_us` and 14 `buckets`, `inflight_max`) and `drive` (`model`, `firmware`, `temperature_c`, `link_width`, `link_speed` and their maximums, `numa_node`, `reactor_numa_node`, `write_cache`, `smart` or `smart_note`). The leader answers |
| `ListTopVolumes` | `{"limit": 20}` | `volumes` by requests per second, summed over their PGs: `id`, `name`, `pool`, `ops`, `bytes`. The leader answers |
| `ListFindings` | `{}` | `findings`: the Advisor's (`rule`, `severity`, `subject`, `title`, `evidence`, `action`, `command`), most serious first. The leader answers |
| `SetDeviceIn` | `{"device": 2, "up_in": false}` | `map_epoch` after the change |
| `SetDeviceWeight` | `{"device": 2, "weight": 0.5}`, or `{"device": 2}` to follow the device's capacity again | `map_epoch` after the change, and `weight` now; `invalid_argument` for a weight below 0 |
| `SetFullRatios` | `{"full": 97}`, or any of `nearfull`, `backfillfull` and `full` (percentages); what isn't given stays | `map_epoch` after the change, and `ratios` now; `invalid_argument` unless each is between 50 and 99 and `nearfull` < `backfillfull` < `full` |
| `AddDevice` | `{"node": 0, "address": "192.0.2.20:9102"}`, and `"class"` if not the node's others' | `device`, its id, and `map_epoch`; `not_found` without the node, `failed_precondition` (`device_conflict`) for an address in use, `invalid_argument` for an address a node can't listen on or a bad class |
| `RemoveDevice` | `{"device": 2}`; `"dry_run": true` changes nothing; `"force": true` lets pools keep fewer copies than their size. Needs step-up through hyperstor-mgmt | `moves` (each PG's `before` and `after`), `short_pools` (`pool`, the `copies` it keeps, its `size`), `fill` (each device taking data: `device`, `used_bytes` after, `total_bytes`) and, unless a dry run, the `operation` that removes the device from the map once no PG lists it, with its node if it was the last; `not_found` without the device, `failed_precondition` (`device_conflict`) when a pool would keep fewer copies than its `min` (or, unforced, its size) or a device would pass 95% full |
| `ListPorts` | `{}`, or `{"node": 1}` | `ports`: each node's port, by `node` and `name`, with its `role` (`role_set` when the operator set it), and from its node's last reports `reported`, `up`, `speed_mbps`, `addresses`, `tx_bytes_per_sec`, `rx_bytes_per_sec`, `other_bytes_per_sec` (the larger direction's traffic that isn't hyperstor's); for ports that carry storage, `lent`, `revoking_until_ms` and `weight_mbs`. The leader answers |
| `SetPortRole` | `{"node": 1, "name": "ibp12s0", "role": "compute"}`, or without `role` for the default | `map_epoch`; `not_found` without the node, `invalid_argument` for a bad role or name |
| `SetDeviceClass` | `{"device": 2, "class": "fast"}`, or `{"device": 2}` to follow its kind again | `map_epoch` and `class` now; `not_found` without the device, `failed_precondition` (`class_conflict`) for a device whose kind isn't known or a class of another kind, `invalid_argument` for a bad class |
| `CreatePool` | `{"name": "fast", "kind": "replicated", "size": 3, "classes": ["nvme"]}`; optional `min_size`, `pg_count`, `chunk_size`, `domain`, `force`, `autoscale` (default on), and for a cache pool `cache_grace` (seconds a PG waits for a down node, default 60, up to 3600) | `pool` (its id, never one a pool had), `pg_count`, `map_epoch`, `mixed_kinds`; `failed_precondition` (`pool_conflict`) for a name in use, too few failure domains or an unforced mix; `invalid_argument` for no classes, a bad name, kind, domain, copies, chunk or PG count |
| `SetPool` | `{"pool": 1, "classes": ["nvme", "ssd"]}`, `"every_device": true` for every device, `"domain": "rack"`, `"name": "fast"`; what isn't given stays; `"force": true` to allow hard disks with flash; `"pg_count": 128`, twice the pool's, splits its PGs; `"autoscale": false` stops the autoscaler for it; `"cache_grace": 300` sets a cache pool's wait for a down node, 0 to 3600 seconds | `map_epoch`, and `mixed_kinds` when the pool's devices mix them; `not_found` without the pool, `failed_precondition` (`pool_conflict`) for `.meta`, a name in use, too few failure domains, or an unforced mix; `invalid_argument` for a bad class, name or domain, classes with `every_device`, or nothing to change. Each pool in `GetStatus` gives its `classes` and `mixed_kinds`, `splitting` with `split_done` PGs while it splits, `placement_count`, the PGs placement goes by (below `pg_count` after a split), `autoscale`, and `cache_grace`; a grace on another kind of pool or past 3600 is `failed_precondition` |
| `SetScrubSettings` | `{"pool": 1, "hours": 12}`; any of `hours` (1 to 720, 0 off), `deep_every` (1 to 28), `hole_repair` (`"repair"` or `"report"`) and `deep_rate_mib` (8 to 1024); what isn't given stays | `map_epoch` and `settings` now; `failed_precondition` (`weakens_scrub`) for a change that checks less than the defaults (off, more than 168 hours, `deep_every` above 7, `report`, under 32 MiB/s), naming what; `invalid_argument` for a value out of range, a cache pool or nothing to change. Each pool in `GetStatus` gives its `scrub` settings, with `weakened` listing what is weaker than the defaults |
| `WeakenScrubSettings` | As `SetScrubSettings`, with `"reason": "..."`, required and kept in the audit log. Needs step-up through hyperstor-mgmt | As `SetScrubSettings`; `invalid_argument` without a reason |
| `GetScrubStatus` | `{}` or `{"pool": 1}` | `pools`: per pool but cache pools, its `settings`, `pg_count`, `oldest_light_secs` and `oldest_deep_secs` (a PG never scrubbed counts from when it was made or first recorded), `overdue` with up to ten `overdue_pgs`, `unchecked`, `damaged`, `inconsistent`, and `week`: totals of its PGs' last scrubs of the last 7 days. Up to a minute behind the leader; `not_found` for an unknown pool |
| `ListPgScrubs` | `{"pool": 1}`, `"overdue": true` for only those overdue | `pgs`: each PG's `since`, its last `light` and `deep` report (`at`, `took_ms`, `compared`, `inconsistent`, `repaired`, `unresolved`, `damaged`, `holes`, `holes_repaired`, `holes_skipped`), `overdue` (`light`, `deep` or empty) and `skipped` |
| `StartScrub` | `{"pool": 1}`, with `"pgs": [3, 9]` for only those and `"deep": true` to read every block | `operation`: done once each PG reported a scrub since it started (`done` of `total` PGs meanwhile); `CancelOperation` has the primaries drop the PGs not yet begun. `invalid_argument` for a cache pool or a PG the pool lacks, `not_found` for an unknown pool |
| `SetPoolDevice` | `{"pool": 1, "device": 3001, "member": true}`: the pool uses the device beyond its classes; `"member": false` stops it using one; `"force": true` as for `SetPool` | `map_epoch` and `mixed_kinds`; `not_found` without the pool or the device, `failed_precondition` (`pool_conflict`) as for `SetPool`. Each pool in `GetStatus` gives its `added_devices` and `removed_devices` |
| `DeletePool` | `{"pool": 3}`; `"dry_run": true` lists what it holds and deletes nothing; `"force": true` deletes that first, as an operation. Needs step-up through hyperstor-mgmt | `map_epoch`, or for a dry run `volumes`: the pool's volumes with those in the trash; forced, `volumes` and the `operation` deleting them and then the pool; `not_found` without the pool, `failed_precondition` (`pool_conflict`) for `.meta` and while the pool holds a volume. The pool leaves the map at once, its id is never given again, and devices delete its data |
| `ListVolumes` | `{"pool": 1}` | `volumes`: `id` (32 hex digits), `pool`, `size` (bytes), `name`, `state` (`active` or `deleting`; trashed volumes aren't listed), `version`, `attached_by` (the client instance holding its lease, 16 hex digits; empty if none) |
| `CreateVolume` | `{"pool": 1, "size": "1073741824", "name": "disk-a"}`, optionally `"id"` (32 hex digits; else drawn at random) | `volume` |
| `ExpandVolume` | `{"id": "…", "size": "2147483648"}` (a multiple of 4096, not smaller than now) | `volume`; `invalid_argument` for a smaller size |
| `DeleteVolume` | `{"id": "…"}`, optionally `"force": true` (delete it even if attached: the holder is blocklisted first) | `purge_unix`: it is in the trash until then (`TrashService` below); `failed_precondition` while it is attached, without `force` |
| `ListSnapshots` | `{"id": "…"}` | `snapshots`: `epoch`, `name`, `group` (32 hex digits, or empty), `state` (`active` or `deleting`) |
| `CreateSnapshot` | `{"id": "…", "name": "before-upgrade"}` | `snapshot`; `unavailable` if the volume's client didn't pause within 2 s |
| `DeleteSnapshot` | `{"id": "…", "epoch": "4"}` | `purge_unix`: to the trash; `failed_precondition` (`in_use`) while a clone or rollback depends on it |
| `CloneVolume` | `{"id": "…", "epoch": "4", "name": "copy"}`, optionally `"new_id"` | `volume`: the clone |
| `RollbackVolume` | `{"id": "…", "epoch": "4"}` | `volume`; `failed_precondition` (`attached`) while attached |
| `FlattenVolume` | `{"id": "…"}` | `volume` as it is now, and the `operation` that copies its data (follow it with `WatchOperation`) |
| `CopyVolume` | `{"id": "…", "pool": 3}`, optionally `"epoch": "4"` (a snapshot; otherwise the head, which needs the volume detached), `"name"`, `"new_id"` | `volume` as it is now, `new_id` and the `operation` that copies it; `not_found` without the volume, snapshot or pool, `resource_exhausted` (`pool_full`) for a full pool, `failed_precondition` (`attached`) for an attached volume's head, `already_exists` for an id in use |
| `MoveVolume` | `{"id": "…", "pool": 3}` | `volume` as it is now, and the `operation` that copies its data, switches pools and deletes what is left behind; `invalid_argument` for its own pool, `failed_precondition` (`attached`) while attached, (`in_use`) while it has snapshots, as for `CopyVolume` otherwise |
| `CreateGroupSnapshot` | `{"ids": ["…", "…"], "name": "vm-7"}` | `group` and `members`: each `id` and its `snapshot` |
| `DeleteGroupSnapshot` | `{"group": "…"}` | `purge_unix`: its snapshots are in the trash |

`AuditService` (the audit log in `.meta`, newest first):

| Method | Request | Answer |
|---|---|---|
| `ListAudit` | optionally `since_unix`, `until_unix`, `principal` (contains), `op`, `limit` (default 100, at most 1000), `after` (a previous `next`) | `records` (`time_ms`, `principal`, `op`, `target`, `result`) and `next`, empty when that was all. Administrators only |
| `AppendAudit` | `lines` (JSON audit records) | `{}`. hyperstor-mgmt's identity only |
| `GetAuditSettings` | `{}` | `settings`: `retention_days` (default 365; 0 forever) |
| `SetAuditSettings` | `{"retention_days": 30}` | `settings`. On the leader |

`TrashService` (deleted volumes and snapshots, kept for their pool's grace period):

| Method | Request | Answer |
|---|---|---|
| `ListTrash` | `{"pool": 1}` (0: every pool) | `items`: `kind` (`volume` or `snapshot`), `id`, `epoch`, `name`, `pool`, `size`, `trashed_unix`, `purge_unix`, `group` |
| `RestoreFromTrash` | `{"id": "…"}`, optionally `"epoch"` (a snapshot) and `"name"` | `volume`; a volume whose name was taken comes back as `NAME-restored` |
| `PurgeFromTrash` | `{"id": "…"}`, optionally `"epoch"` | the `operation` that deletes its data; `failed_precondition` if it isn't in the trash |
| `GetTrashSettings` | `{"pool": 1}` | `settings`: `pool`, `grace_seconds` (default 86400), `on_full` (`keep` or `purge`) |
| `SetTrashSettings` | `{"pool": 1, "grace_seconds": "604800", "on_full": "keep"}` | `settings`. On the leader |

`TokenService`, `CaService`, `MapService`, `MemberService`, `NodeService`, `S3Service` and `OperationService`:

| Method | Request | Answer |
|---|---|---|
| `CreateToken` | `{"role": "node"}` (or `"meta"`, or `"mgmt"` with `"name"`: a management client such as glidex, 1–32 of a–z, 0–9 and -, or `"s3"` with `"name"`: an S3 gateway), optionally `"ttl_seconds"` (default 3600, at most a week) | `token` (shown once), `info`, `api_address`. On the leader |
| `ListTokens` | `{}` | `tokens`: `id`, `role`, `expires_unix`, `used`, `revoked`, `created_by`, and a mgmt token's `name` |
| `RevokeToken` | `{"id": "…"}` | `{}` |
| `Join` | `token`, `csr` (PEM); a storage host: `addresses` (one per reactor) and `rack`; a member: `member_id` and `member_ips`; a management client: nothing more; a gateway: `gateway_ips`, which its certificate names | `cluster_id`, `certificate` (chain), `root`, `identity` (the URI the certificate names); a storage host: `node`, `map_addresses`; a management client: `map_addresses` (it renews with `RenewCertificate`); a member: `intermediate`, `intermediate_key`, `cluster_file`, `cluster_key` (empty where the member answering has none); a gateway: `map_addresses` |
| `RenewCertificate` | `csr` (PEM) | `certificate` for the caller's own identity and addresses, `root` |
| `ListCertificates` | `{}` | `certificates` the API issued (`serial`, `identity`, `not_after_unix`, `issued_unix`, `issued_by`, `revoked`) and `revoked` serials (`serial`, `until_unix`, `revoked_unix`, `revoked_by`) |
| `RevokeCertificate` | `{"serial": "…"}` | the `revoked` entry. On the leader |
| `GetClusterKey` | `{}`. Map service members (`meta` certificates) only, audited: `hyperstor-install` asks for it on a member that has none | `cluster_key`; `failed_precondition` (`no_cluster_key`) where the member answering has none |
| `ListMembers` (`MemberService`) | `{}` | `members` (`id`, `raft_address`, `map_address`, `voter`, `leader`, `api_address`: its cluster API as it recorded when it last led, empty if it hasn't), `me` |
| `AddMember` | `id`, `raft_address`, `map_address` | `members`. On the leader; answers once the member votes |
| `RemoveMember` | `id`, optionally `force` | `members`. On the leader; refused below three voters without `force` |
| `GetMapBackup` (`MapService`) | `{}` | `backup` (bytes), `cluster_id`, `epoch`. Administrators only: it holds the token and certificate records |
| `DrainNode` | `{"node": 3}`, optionally `"dry_run": true` | `moves` (each PG's `before` and `after`) and, unless a dry run, the `operation` |
| `CreateS3Key` (`S3Service`) | `{"tenant": 1}`, optionally `"description"` (up to 200 characters). Needs step-up through hyperstor-mgmt | `key` (`id`, `tenant`, `description`, `created_unix`, `created_by`) and `secret`, shown once: the map service keeps it encrypted with the cluster key. On the leader; `failed_precondition` (`no_cluster_key`) on a member without the cluster key, `invalid_argument` for tenant 0 |
| `ListS3Keys` | `{}` | `keys`, without secrets |
| `RevokeS3Key` | `{"id": "HS…"}` | `{}`: gateways refuse the key within seconds. On the leader; `not_found` for an unknown key |
| `SetS3Placement` | `{"placement": {"name": "default", "data_pool": "bulk", "index_pool": "flash"}}` | `{}`. Kept by the pools' ids, so a renamed pool keeps its placements. On the leader; `invalid_argument` for a bad name, a pool that doesn't exist, a cache pool or `.meta` |
| `DeleteS3Placement` | `{"name": "fast"}` | `{}`; `not_found` for an unknown placement |
| `ListS3Placements` | `{}` | `placements`, each with its pools' names now and `data_pool_id`, `index_pool_id` (a pool deleted since has an empty name) |
| `GetS3Gateway` | `{}` | `keys` with their `secret`s, and `placements`: for `s3` certificates only, which gateways fetch every few seconds; any member answers, from its copy |
| `GetOperation` | `{"id": "…"}` | the operation: `id`, `kind`, `target`, `state` (`OPERATION_STATE_RUNNING`, `_DONE`, `_FAILED`, `_CANCELLED`), `done` of `total`, `message`, times, `created_by` |
| `ListOperations` | `{}` | `operations`, newest first: drains, device removals, pool deletes, flattens, copies, moves and purges, kept in the map service's replicated store a week after they finish; a new leader carries on with those still running |
| `WatchOperation` | `{"id": "…"}` | a stream: the operation now, then each change (and again every 2 s), ending once it has finished |
| `CancelOperation` | `{"id": "…"}` | the operation; a cancelled drain or device removal brings the devices back in; a cancelled copy deletes its new volume, and a move stops unless it has switched pools. A flatten or a purge can't be cancelled (`failed_precondition`, `not_cancellable`) |

```sh
cd /etc/hyperstor/pki
curl -s --cacert root.crt --cert admin.crt --key admin.key \
    -X POST -H 'Content-Type: application/json' -d '{}' \
    https://127.0.0.1:8850/hyperstor.v1.ClusterService/GetStatus
```

```json
{"cluster_id":"3","epoch":"2","health":"HEALTH_OK","nodes":[...],"pools":[...],"map_address":"127.0.0.1:8849"}
```

## Errors

Errors are Connect errors, with an HTTP status and a JSON body. Each
carries a `hyperstor.v1.ErrorInfo` detail whose `reason` is a stable
word for scripts (the message is for people, and may change):

```json
{"code":"not_found","message":"no device 77","details":[{"type":"hyperstor.v1.ErrorInfo","value":"…"}]}
```

| Code | Reason | When |
|---|---|---|
| `not_found` | `not_found` | No such device, pool, volume, node, token or operation |
| `already_exists` | `already_exists` | `CreateVolume` with the id of a volume that exists |
| `invalid_argument` | `invalid_argument`, `token_invalid`, `bad_request` | A size that isn't a positive multiple of 4096, a malformed id; a token that isn't one, or is for another cluster; a certificate request that doesn't parse |
| `unauthenticated` | `unauthenticated` | No client certificate with a hyperstor identity |
| `permission_denied` | `permission_denied`, `wrong_cluster`, `revoked`, `token_used`, `token_expired`, `token_revoked` | The certificate's role or cluster doesn't allow the call, or it is revoked; the token was used, has expired or was revoked |
| `failed_precondition` | `attached`, `in_use`, `no_ca`, `node_conflict`, `finished`, `not_cancellable` | `DeleteVolume` of a volume a client has attached (without `force`), or `RollbackVolume` of one; a snapshot something depends on, or an operation refused as things stand; this member can't sign (no `--ca`); a joining host's addresses belong to another node; the operation has finished, or isn't a drain |
| `resource_exhausted` | `rate_limited`, `pool_full` | The caller is over its rate limit; `CreateVolume`, `CloneVolume`, `CreateSnapshot` or `CreateGroupSnapshot` in a pool past `full` |
| `unavailable` | `not_leader`, `not_formed`, `not_published`, `unavailable` | This member isn't the leader (the message names the leader's map service address); the map service has no map yet (it has just started); a change to the map was made but couldn't be committed in time (it is published if the leader keeps the lead: read the map before trying again); the map service is stopping |

## What the answers mean

- **Reads** (`GetStatus`, `ListPgs`) come from the published map: what
  storage nodes and clients see.
- **Calls that change the map** (`SetDeviceIn`, `SetDeviceWeight`,
  `SetFullRatios`, `AddDevice`, pools, classes, endpoints, scrub settings,
  drains and removals) answer once the map with the change is published,
  so the next `GetStatus` shows it. Publishing waits until devices losing
  roles have been fenced, usually under a second. `GetStatus` then shows
  the PGs moving.
- **Health** is `HEALTH_ERROR` when a PG can't take writes (fewer copies
  than the pool's `min`, no primary, a device holding it fenced for a map
  change not yet published, or a primary that has failed to peer with its
  members for over 5 s), `HEALTH_WARN` while a node is
  down, a device is out, or PGs are degraded or backfilling, and
  `HEALTH_OK` otherwise. Each reason is listed. A node the map service
  hasn't heard from since it started is also `HEALTH_WARN`: the map
  counts it up for a short grace period after a restart.
