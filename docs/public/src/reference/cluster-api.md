# Cluster API

`hyperstor-meta --api ADDR` serves the cluster API, `hyperstor.v1`, which
[`hyperstorctl`](hyperstorctl.md) uses. One service answers the
[Connect protocol](https://connectrpc.com/docs/protocol) (JSON or
protobuf over HTTP/1.1 or HTTP/2), gRPC and gRPC-Web, so any of these
work: `curl`, `grpcurl` (with the proto file), generated clients.

The definition is
[`crates/hyperstor-api/proto/hyperstor/v1/cluster.proto`](https://github.com/CMKL-APEX/hyperstor/blob/main/crates/hyperstor-api/proto/hyperstor/v1/cluster.proto).

## Security

With `--api-tls`, the API speaks only mutual TLS. Every client presents
a certificate issued by the cluster's CA, whose identity
(`hyperstor://<cluster>/<role>/<id>`) decides what it may do:

| Role | May |
|---|---|
| `admin` (`hyperstorctl`), `mgmt` | Everything |
| `node`, `meta` | Read (`GetStatus`, `ListPgs`, `ListVolumes`) |
| `host` | Nothing |

A certificate of another cluster is refused. `hyperstor-install` makes
the certificates in `/etc/hyperstor/pki`.

**Local socket.** With `--api-socket`, the API is also served on a Unix
socket (`/run/hyperstor-meta/api.sock` when installed). There the
kernel names the caller: root, the service's user and members of
`hyperstor-admin` may do everything, others nothing. It is the way in
when certificates are broken; its use is audited like any other.

Without `--api-tls` the API is plain HTTP and accepts every request:
only for loopback and tests.

**Audit.** Every change (`SetDeviceIn`, `CreateVolume`, `DeleteVolume`) and every refused call is
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

Each call is a `POST` to `/hyperstor.v1.ClusterService/<Method>` with a
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

```sh
cd /etc/hyperstor/pki
curl -s --cacert root.crt --cert admin.crt --key admin.key \
    -X POST -H 'Content-Type: application/json' -d '{}' \
    https://127.0.0.1:8850/hyperstor.v1.ClusterService/GetStatus
```

```json
{"cluster_id":"3","epoch":"2","health":"HEALTH_OK","nodes":[...],"pools":[...],"map_address":"127.0.0.1:8849"}
```

Errors are Connect errors, with an HTTP status and a JSON body:

```json
{"code":"not_found","message":"no device 77"}
```

| Code | When |
|---|---|
| `not_found` | No such device, pool or volume |
| `already_exists` | `CreateVolume` with the id of a volume that exists |
| `failed_precondition` | `DeleteVolume` of a volume a client has attached |
| `invalid_argument` | A size that isn't a positive multiple of 4096, a malformed id |
| `unauthenticated` | No client certificate with a hyperstor identity |
| `permission_denied` | The certificate's cluster or role doesn't allow the call |
| `resource_exhausted` | The caller is over its rate limit |
| `unavailable` | The map service is stopping |

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
