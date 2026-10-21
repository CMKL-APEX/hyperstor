# Cluster API

`hyperstor-meta --api ADDR` serves the cluster API, `hyperstor.v1`, which
[`hyperstorctl`](hyperstorctl.md) uses. One service answers the
[Connect protocol](https://connectrpc.com/docs/protocol) (JSON or
protobuf over HTTP/1.1 or HTTP/2), gRPC and gRPC-Web, so any of these
work: `curl`, `grpcurl` (with the proto file), generated clients.

The definition is
[`crates/hyperstor-api/proto/hyperstor/v1/cluster.proto`](https://github.com/CMKL-APEX/hyperstor/blob/main/crates/hyperstor-api/proto/hyperstor/v1/cluster.proto).

> **No authentication yet.** The API is plain HTTP and accepts every
> request; certificates and roles are planned. `hyperstor-install` binds
> it to `127.0.0.1`. Never expose it to a network others can reach.

## Calls

Each call is a `POST` to `/hyperstor.v1.ClusterService/<Method>` with a
JSON body. Field names are as in the proto file; fields with zero
values are left out of answers.

| Method | Request | Answer |
|---|---|---|
| `GetStatus` | `{}` | `cluster_id`, `epoch`, `health` (`HEALTH_OK`, `HEALTH_WARN`, `HEALTH_ERROR`), `reasons`, `nodes` with their `devices`, `pools` with PG counts, `map_address` |
| `ListPgs` | `{"pool": 1}` | `pgs`: `pool`, `pg`, `epoch`, `acting`, `up` |
| `SetDeviceIn` | `{"device": 2, "up_in": false}` | `map_epoch` after the change |

```sh
curl -s -X POST -H 'Content-Type: application/json' -d '{}' \
    http://127.0.0.1:8850/hyperstor.v1.ClusterService/GetStatus
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
| `not_found` | No such device or pool |
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
  `HEALTH_OK` otherwise. Each reason is listed.
