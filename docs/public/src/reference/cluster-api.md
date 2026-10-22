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
(`TokenService`, `CaService`, `NodeService`, `OperationService`), in
`crates/hyperstor-api/proto/hyperstor/v1/`.

## Security

With `--api-tls`, the API speaks only TLS. Every call but `Join` needs
a client certificate issued by the cluster's CA, whose identity
(`hyperstor://<cluster>/<role>/<id>`) decides what it may do:

| Role | May |
|---|---|
| `admin` (`hyperstorctl`), `mgmt` | Everything |
| `node`, `meta` | Read (`GetStatus`, `ListPgs`, `ListVolumes`, `ListTokens`, `GetOperation`, `ListOperations`, `DrainNode` with `dry_run`), and renew their own certificate |
| `host` | Renew its own certificate; nothing else |

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

**Audit.** Every change (`SetDeviceIn`, `CreateVolume`, `DeleteVolume`, `CreateToken`, `RevokeToken`, `Join`, `RenewCertificate`, `RevokeCertificate`, `DrainNode`, `CancelOperation`) and every refused call is
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
| `GetStatus` | `{}` | `cluster_id`, `epoch`, `health` (`HEALTH_OK`, `HEALTH_WARN`, `HEALTH_ERROR`), `reasons`, `nodes` with their `devices` and `last_heard_ms` (time since the node's last heartbeat; absent if none since the map service started), `pools` with PG counts, `map_address` |
| `ListPgs` | `{"pool": 1}` | `pgs`: `pool`, `pg`, `epoch`, `acting`, `up` |
| `SetDeviceIn` | `{"device": 2, "up_in": false}` | `map_epoch` after the change |
| `ListVolumes` | `{"pool": 1}` | `volumes`: `id` (32 hex digits), `pool`, `size` (bytes), `name`, `state` (`active` or `deleting`), `version`, `attached_by` (the client instance holding its lease, 16 hex digits; empty if none) |
| `CreateVolume` | `{"pool": 1, "size": "1073741824", "name": "disk-a"}`, optionally `"id"` (32 hex digits; else drawn at random) | `volume` |
| `DeleteVolume` | `{"id": "…"}`, optionally `"force": true` (delete it even if attached: the holder is blocklisted first) | `{}`, once the volume and its data are gone; `failed_precondition` while it is attached, without `force` |

`TokenService`, `CaService`, `MapService`, `NodeService` and `OperationService`:

| Method | Request | Answer |
|---|---|---|
| `CreateToken` | `{"role": "node"}` (or `"meta"`), optionally `"ttl_seconds"` (default 3600, at most a week) | `token` (shown once), `info`, `api_address`. On the leader |
| `ListTokens` | `{}` | `tokens`: `id`, `role`, `expires_unix`, `used`, `revoked`, `created_by` |
| `RevokeToken` | `{"id": "…"}` | `{}` |
| `Join` | `token`, `csr` (PEM); a storage host: `addresses` (one per reactor) and `rack`; a member: `member_id` and `member_ips` | `cluster_id`, `certificate` (chain), `root`; a storage host: `node`, `map_addresses`; a member: `intermediate`, `intermediate_key`, `cluster_file` |
| `RenewCertificate` | `csr` (PEM) | `certificate` for the caller's own identity and addresses, `root` |
| `ListCertificates` | `{}` | `certificates` the API issued (`serial`, `identity`, `not_after_unix`, `issued_unix`, `issued_by`, `revoked`) and `revoked` serials (`serial`, `until_unix`, `revoked_unix`, `revoked_by`) |
| `RevokeCertificate` | `{"serial": "…"}` | the `revoked` entry. On the leader |
| `GetMapBackup` (`MapService`) | `{}` | `backup` (bytes), `cluster_id`, `epoch`. Administrators only: it holds the token and certificate records |
| `DrainNode` | `{"node": 3}`, optionally `"dry_run": true` | `moves` (each PG's `before` and `after`) and, unless a dry run, the `operation` |
| `GetOperation` | `{"id": "…"}` | the operation: `id`, `kind`, `target`, `state` (`OPERATION_STATE_RUNNING`, `_DONE`, `_FAILED`, `_CANCELLED`), `done` of `total`, `message`, times, `created_by` |
| `ListOperations` | `{}` | `operations`, newest first (the last 100, kept in memory) |
| `CancelOperation` | `{"id": "…"}` | the operation; a cancelled drain brings the node's devices back in |

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
| `failed_precondition` | `attached`, `no_ca`, `node_conflict`, `finished` | `DeleteVolume` of a volume a client has attached (without `force`); this member can't sign (no `--ca`); a joining host's addresses belong to another node; the operation has finished |
| `resource_exhausted` | `rate_limited` | The caller is over its rate limit |
| `unavailable` | `not_leader`, `not_formed`, `unavailable` | This member isn't the leader (the message names the leader's map service address); the map service has no map yet (it has just started); the map service is stopping |

## What the answers mean

- **Reads** (`GetStatus`, `ListPgs`) come from the published map: what
  storage nodes and clients see.
- **`SetDeviceIn`** changes the map service's working map. The change is
  published once the devices losing roles have been fenced, usually
  within a second, and `GetStatus` then shows the PGs moving.
- **Health** is `HEALTH_ERROR` when a PG can't take writes (fewer copies
  than the pool's `min`, or no primary), `HEALTH_WARN` while a node is
  down, a device is out, or PGs are degraded or backfilling, and
  `HEALTH_OK` otherwise. Each reason is listed. A node the map service
  hasn't heard from since it started is also `HEALTH_WARN`: the map
  counts it up for a short grace period after a restart.
