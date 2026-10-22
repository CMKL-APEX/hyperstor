# hyperstor-mgmt

The management service: the web dashboard, people's logins, and the
cluster API on their behalf. See
[Managing a cluster](../admin-guide/managing.md#dashboard).

```text
usage: hyperstor-mgmt --listen ADDR --api ADDR[,ADDR...] --tls DIR [--metrics ADDR,ADDR...] [--authd SOCKET] [--web-tls DIR] [--hosts NAME,...] [--ui DIR]
```

| Option | Default | Meaning |
|---|---|---|
| `--listen ADDR` | required | `IP:PORT` to serve on. Beyond loopback it needs `--authd` and `--web-tls` |
| `--api ADDR[,ADDR...]` | required | The [cluster API](cluster-api.md), `IP:PORT`: every map service member, separated by commas. A call a member can't serve (it isn't the leader) goes to the next |
| `--tls DIR` | required | Its identity: `DIR/mgmt.crt`, `DIR/mgmt.key` and `DIR/root.crt` |
| `--metrics ADDR,ADDR...` | none | The storage nodes' metrics addresses (`hyperstord --admin`) |
| `--authd SOCKET` | none | People log in with their password, checked by [`hyperstor-authd`](hyperstor-authd.md) on `SOCKET`. Without it, the dashboard is read-only and on loopback only |
| `--web-tls DIR` | none | Serve HTTPS with `DIR/web.crt` (the chain) and `DIR/web.key` |
| `--hosts NAME,...` | none | Names people reach it by, allowed in the `Host` header besides `localhost`, `127.0.0.1` and `[::1]` |
| `--ui DIR` | the built-in dashboard | Serve the web UI's files from `DIR` |

The installer runs it with `--authd /run/hyperstor-authd/auth.sock` on
`127.0.0.1`; from another machine, use an SSH tunnel
(`ssh -L 8852:127.0.0.1:8852 host`).

## Logins

People log in with their user name and password on the host (PAM
service `hyperstor`); they must be in the group `hyperstor-users`. A
first login makes an account with no roles. What a role allows:

| Role | May |
|---|---|
| `viewer` | Read: status, PGs, volumes, snapshots, operations, members, the dashboard |
| `operator` | Also take devices out and in, drain nodes, cancel operations |
| `storage-admin` | Also create and delete volumes, snapshots and clones, roll volumes back, add and remove members |
| `security-admin` | People and their roles, join tokens, certificates, map backups; nothing else |

Everyone may make API tokens for themselves. Deleting a volume, a
snapshot or a member, rolling a volume back, making a join token or an
API token, revoking a certificate, fetching a map backup, and changing
people's roles need a login of the last 10 minutes; an API token is
never fresh enough for these.

**The first security administrator.** While no one is one, the service
prints a claim code to its log (`journalctl -u hyperstor-mgmt`), valid
for an hour: whoever logs in with it becomes the first. Restarting the
service prints a new one while the cluster is unclaimed.

Sessions last 12 hours, or 30 minutes idle. They are kept, with
accounts and tokens, in the map service's store, so they outlive a
restart of `hyperstor-mgmt`.

## Paths

| Path | Answers |
|---|---|
| `/` | The dashboard (or `--ui`'s `index.html`) |
| `GET /auth/methods` | `{"pam": true, "claim_open": bool}`; no login needed |
| `POST /auth/login` | `{"user", "password", "claim"?}` → `{"user", "roles", "csrf"}` and the session cookie `hs_session` |
| `GET /auth/whoami` | `{"user", "roles", "method", "csrf"}` |
| `POST /auth/logout` | Ends the session |
| `GET`, `POST /auth/tokens`, `DELETE /auth/tokens/ID` | Your API tokens: list, make (`{"name", "days"?}`; the token is shown once), revoke |
| `GET /auth/users`, `PUT /auth/users/NAME/roles`, `DELETE /auth/users/NAME` | People (security administrators) |
| `POST /api/hyperstor.v1.SERVICE/METHOD` | The cluster API's call, as it came (Connect, gRPC-Web; JSON or protobuf), if your roles allow it |
| `/api/status` | `GetStatus`, as JSON |
| `/api/pgs?pool=N` | `ListPgs` for pool N |
| `/api/metrics` | Every node's metrics, parsed: `{"nodes": [{"addr", "up", "samples": [[name, labels, value]]}]}` |
| `/health` | `ok` |

Scripts send `Authorization: Bearer hst_…` with an API token instead of
the cookie. Errors are `{"code", "message"}`: `unauthenticated` (401),
`reauth_required` (401: log in again), `permission_denied` (403), `csrf`
(403), `login_failed` (401), `rate_limited` (429).

## Protections

- It answers only requests whose `Host` is one of its names on its port
  (against DNS rebinding).
- The session cookie is `HttpOnly; SameSite=Strict` (and `Secure` over
  HTTPS). A write with the cookie must send its CSRF value in
  `X-Hyperstor-CSRF`, and a write from another site's `Origin` is
  refused.
- Pages carry a strict Content-Security-Policy and may not be framed.
- Every login and every change is audited on standard error as a JSON
  line with the person (`user:NAME`); the map service audits the same
  calls as `mgmt`.

| Exit status | Meaning |
|---|---|
| 1 | The certificates can't be read, or the address can't be bound |
| 2 | A missing or malformed option, or `--listen` beyond loopback without `--authd` and `--web-tls` |
