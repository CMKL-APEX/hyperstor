# hyperstor-mgmt

The management service: for now a read-only web dashboard. See
[Managing a cluster](../admin-guide/managing.md#dashboard).

```text
usage: hyperstor-mgmt --listen ADDR --api ADDR --tls DIR [--metrics ADDR,ADDR...]
```

| Option | Default | Meaning |
|---|---|---|
| `--listen ADDR` | required | `IP:PORT` to serve the dashboard on; must be a loopback address |
| `--api ADDR` | required | The [cluster API](cluster-api.md), `IP:PORT` |
| `--tls DIR` | required | Its identity: `DIR/mgmt.crt`, `DIR/mgmt.key` and `DIR/root.crt` |
| `--metrics ADDR,ADDR...` | none | The storage nodes' metrics addresses (`hyperstord --admin`) |

It only reads: the cluster API's status and PGs, and each node's
metrics. People don't log in yet, so it refuses to listen on anything
but loopback; from another machine, use an SSH tunnel
(`ssh -L 8852:127.0.0.1:8852 host`). It answers only requests whose
`Host` is `localhost`, `127.0.0.1` or `[::1]` on its port (against DNS
rebinding), and only `GET`.

| Path | Answers |
|---|---|
| `/` | The dashboard |
| `/api/status` | The cluster API's `GetStatus`, as JSON |
| `/api/pgs?pool=N` | `ListPgs` for pool N |
| `/api/metrics` | Every node's metrics, parsed: `{"nodes": [{"addr", "up", "samples": [[name, labels, value]]}]}` |
| `/health` | `ok` |

| Exit status | Meaning |
|---|---|
| 1 | The certificates can't be read, or the address can't be bound |
| 2 | A missing or malformed option, or a non-loopback `--listen` |
