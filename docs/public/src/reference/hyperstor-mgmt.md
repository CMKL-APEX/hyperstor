# hyperstor-mgmt

The management service: the web dashboard, people's logins, and the
cluster API on their behalf. See
[Managing a cluster](../admin-guide/managing.md#dashboard).

```text
usage: hyperstor-mgmt --listen ADDR[,ADDR...] --api ADDR[,ADDR...] --tls DIR [--metrics ADDR,ADDR...] [--authd SOCKET] [--oidc FILE] [--web-tls DIR] [--tailscale SOCKET] [--hosts NAME,...] [--proxy-ca FILE] [--trusted-proxies CIDR,...] [--proxy-only] [--ui DIR]
```

| Option | Default | Meaning |
|---|---|---|
| `--listen ADDR[,ADDR...]` | required | `IP:PORT`s to serve on (`0.0.0.0:8852,[::]:8852`: every address). Beyond loopback it needs `--web-tls`, and `--authd` or `--oidc`; the host's own names and addresses are then allowed in `Host` |
| `--api ADDR[,ADDR...]` | required | The [cluster API](cluster-api.md), `IP:PORT`: one map service member or more, separated by commas. mgmt asks them for the member list every 5 seconds and sends every call to the leader first, then to the others, so one address is enough and every host's mgmt serves changes |
| `--tls DIR` | required | Its identity: `DIR/mgmt.crt`, `DIR/mgmt.key` and `DIR/root.crt` |
| `--metrics ADDR,ADDR...` | none | The storage nodes' metrics addresses (`hyperstord --admin`) |
| `--authd SOCKET` | none | People log in with their password, checked by [`hyperstor-authd`](hyperstor-authd.md) on `SOCKET`. Without it, the dashboard is read-only and on loopback only |
| `--oidc FILE` | none | People log in with an identity provider (OIDC), configured in `FILE`; see [Identity providers](#identity-providers) |
| `--web-tls DIR` | none | Serve HTTPS with `DIR/web.crt` (the chain) and `DIR/web.key`, re-read within a minute when they change |
| `--tailscale SOCKET` | none | Also serve the node's Tailscale certificate to clients naming its `ts.net` names, fetched from `tailscaled`'s local API at `SOCKET` (`/run/tailscale/tailscaled.sock`); needs `--web-tls` |
| `--hosts NAME,...` | none | Names people reach it by, allowed in the `Host` header besides `localhost`, `127.0.0.1` and `[::1]`, the host's own (beyond loopback) and the DNS names on `--web-tls`'s certificate |
| `--proxy-ca FILE` | none | A client certificate from a CA in `FILE` (PEM) marks a proxy's connection; see [Behind a proxy](#behind-a-proxy). Needs `--web-tls` |
| `--trusted-proxies CIDR,...` | none | Proxies by address, `ADDR` or `ADDR/BITS` |
| `--proxy-only` | off | Serve only proxies, and loopback; needs `--proxy-ca` or `--trusted-proxies` |
| `--ui DIR` | the built-in dashboard | Serve the web UI's files from `DIR` |

The installer runs it on every address over HTTPS, with logins
(`--authd /run/hyperstor-authd/auth.sock`); see [Certificates](#certificates).

## Certificates

| Certificate | How |
|---|---|
| The cluster CA's (default) | The installer issues `web.crt` for the host's names and addresses from the cluster's intermediate CA, and renews it. Browsers trust it once they trust the cluster's root, `/etc/hyperstor/pki/root.crt`; `hyperstorctl` on the host trusts it already |
| Your own | `hyperstor-install install --web-tls DIR`: `DIR/web.crt` (full chain) and `DIR/web.key`, readable by the `hyperstor` group. Replace them in place (say, an ACME client's deploy hook) and mgmt serves the new pair within a minute, without a restart |
| The node's Tailscale certificate | `hyperstor-install install --tailscale-cert`: on a host on Tailscale with HTTPS certificates on for the tailnet, browsers reaching `https://NODE.TAILNET.ts.net:8852` get the node's Let's Encrypt certificate; other names keep the one above. The name is published in Certificate Transparency logs |

- A plain-HTTP request on the port is answered `308` to the same URL over
  HTTPS.
- HSTS (`max-age=86400`) is sent only with the Tailscale certificate,
  which browsers trust: with the cluster CA's, a browser that hasn't
  imported the root would be locked out.

An ACME client's deploy hook, for example certbot's:

```sh
#!/bin/sh
# /etc/letsencrypt/renewal-hooks/deploy/hyperstor
install -m 0644 "$RENEWED_LINEAGE/fullchain.pem" /etc/hyperstor/web/web.crt
install -m 0640 -g hyperstor "$RENEWED_LINEAGE/privkey.pem" /etc/hyperstor/web/web.key
```

with `hyperstor-install install --web-tls /etc/hyperstor/web`.

## Behind a proxy

mgmt can sit behind a reverse proxy, a load balancer or a CDN such as
Cloudflare, on one host or several: every host's mgmt sends its calls to
the map service's leader, and sessions and tokens are kept in the map
service, so any host's mgmt serves any request.

| What | How |
|---|---|
| The public name | A certificate whose DNS names include it (`--web-tls`), or `hyperstor-install install --mgmt-hosts NAME`. Other names are refused (`421`) |
| The certificate | Any CA's: Cloudflare's Origin CA, or your organization's. `web.crt` holds the chain, leaf first; `web.key` the key (PKCS#8, RSA or EC) |
| Which connections are the proxy's | A client certificate from `--mgmt-proxy-ca FILE` (Cloudflare's Authenticated Origin Pulls CA, or your proxies' CA), or an address in `--mgmt-trusted-proxies` (`127.0.0.1,::1` for a tunnel on the host) |
| The person's address | From a proxy's connection only: `CF-Connecting-IP`, else the last `X-Forwarded-For` entry. Others' such headers are ignored. It is the address in mgmt's log and the audit |
| Only through the proxy | `--mgmt-proxy-only on`: other connections get `403`, except from loopback |
| Request bodies | mgmt reads a request's whole body before answering, so a proxy can reuse the connection. Anyone may send 256 KiB; up to 4 MiB needs a valid session or API token. Larger bodies get `413`, and bodies that take longer than 10 s (60 s for the part past 256 KiB) get `408`, both with `Connection: close` |
| Connections | A connection has 10 s to send each request's headers (the first one's TLS handshake included). A kept connection with no request closes after 2 minutes, longer than proxies keep theirs. At most 1024 are open at once, from every address together; past that, new ones wait until one closes |

With a Cloudflare Tunnel on the host, one tunnel can serve several
names: add a public hostname whose service is
`https://localhost:8852`, with **Origin Server Name** a name on mgmt's
certificate and **CA Pool** the CA that issued it (Cloudflare's Origin
CA root for an Origin CA certificate, `/etc/hyperstor/pki/root.crt` for
the cluster's own). Then:

```sh
hyperstor-install install --web-tls /etc/hyperstor/web-origin \
    --mgmt-trusted-proxies 127.0.0.1,::1
```

where `/etc/hyperstor/web-origin` holds the Origin CA certificate and
key as `web.crt` and `web.key` (key mode 0640, group `hyperstor`). With
the cluster's own certificate instead, leave `--web-tls` as it is and
add `--mgmt-hosts` with the public name.

Any host's mgmt serves any request, so a load balancer or tunnel over
several hosts needs no session affinity: sessions, and the state of a
login with an identity provider until the provider sends the browser
back, are kept in the map service.

## Logins

People log in with their user name and password on the host (PAM
service `hyperstor`); they must be in the group `hyperstor-users`. A
first login makes an account with no roles. What a role allows:

| Role | May |
|---|---|
| `viewer` | Read: status, PGs, volumes, snapshots, operations, members, the dashboard |
| `operator` | Also take devices out and in, drain nodes, cancel operations, take snapshots, restore from the trash |
| `storage-admin` | Also create and delete volumes and snapshots, clones, roll volumes back, purge the trash and set its grace periods, add and remove members |
| `security-admin` | People and their roles, join tokens, certificates, map backups, the audit log; nothing else |

**Roles in a tenant or a project.** The same way, `role@tenant:NAME`
(`storage-admin@tenant:lab`) holds in one tenant: its projects, volumes,
snapshots, trashed volumes, buckets and keys. `role@project:TENANT/NAME`
(`viewer@project:lab/web`) holds in one project: its volumes and
buckets alone. A tenant or project named in a grant must exist.

**Roles in a realm.** A role may also be granted in one realm, as
`role@realm` (`storage-admin@acme`), beside cluster-wide roles. It then
reaches only that realm's tenants' volumes, snapshots and trashed
volumes, their buckets and keys, the realm's placements, the realm
itself and the pools it may use, its people's grants, and the
operations on its tenants' volumes, whoever started them; nothing of
the cluster as a whole (status,
PGs, devices, nodes, pools, members, hosts, the audit). A
`security-admin@acme` manages acme's S3 keys and grants roles in acme
to anyone who has logged in; their other grants stay. A volume, bucket
or key of another realm answers as one that doesn't exist. The UI's
realm picker and `hyperstorctl --in-realm R` act in one realm, which
sends `X-Hyperstor-Realm: R`; a cluster administrator may use it to see
what a realm's administrators see. Without it, someone with roles in
several realms gets one list across them, each row naming its realm;
creating a volume or a placement there must name its tenant or realm
(`tenant_required`, `realm_required`). A grant names a realm that exists.

Everyone may make API tokens for themselves. Deleting a volume, a
snapshot or a member, rolling a volume back, purging the trash or
changing its grace period, changing how long the audit log is kept, making a join token or an API token, revoking
a certificate, fetching a map backup, removing or converting a device
(`hyperstorctl device upgrade`), and changing people's roles need a
login of the last 10 minutes; an API token is never fresh enough for
these.

**The first security administrator.** While no one is one, the service
prints a claim code to its log (`journalctl -u hyperstor-mgmt`), valid
for an hour: whoever logs in with it becomes the first. Restarting the
service prints a new one while the cluster is unclaimed.

Sessions last 12 hours, or 30 minutes idle. They are kept, with
accounts and tokens, in the map service's store, so they outlive a
restart of `hyperstor-mgmt`.

## Identity providers

With `--oidc FILE`, people can log in with an OIDC identity provider
(Authorization Code flow with PKCE). `FILE` is JSON:

```json
{
  "issuer": "https://idp.example.com",
  "client_id": "hyperstor",
  "client_secret_file": "/etc/hyperstor/oidc-secret"
}
```

| Key | Default | Meaning |
|---|---|---|
| `issuer`, `client_id` | required | The provider, and this client as registered with it |
| `client_secret_file` | required | A file holding the client's secret (mode 0640, group `hyperstor`), read at each use |
| `scopes` | `["openid", "email", "profile"]` | Asked for; `openid` is required |
| `allowed_domains` | any | Email domains that may log in; the address must be one the provider verified (`email_verified`) |
| `required_groups`, `groups_claim` | none, `groups` | Groups (in the ID token's `groups_claim`) of which a person must be in one |
| `redirect_uri` | `<scheme>://<host>/auth/oidc/callback` | Where the provider sends the browser back; register it with the provider |

A person is named by the email the provider vouches for
(`email_verified`), lowercased, and tied to the provider's subject: a
changed email keeps the account. A first login makes an account with no
roles, as with passwords; the claim code works here too. The secret
never goes in `FILE` itself.

## Paths

| Path | Answers |
|---|---|
| `/` | The dashboard (or `--ui`'s `index.html`) |
| `GET /auth/methods` | `{"pam": bool, "oidc": bool, "claim_open": bool, "directories": [{"id", "issuer", "realm"}], "cluster_id"}`; no login needed |
| `POST /auth/login` | `{"user", "password", "claim"?}` → `{"user", "roles", "csrf"}` and the session cookie (`__Host-hs_session` over HTTPS, `hs_session` over HTTP) |
| `GET /auth/whoami` | `{"user", "roles", "method", "csrf", "directory"}` (`directory`: the issuer of a directory's person) |
| `POST /auth/oidc/start` | `{"return_to"?, "claim"?, "reauth"?}` → `{"redirect"}`: where to send the browser to log in with the provider (`reauth`: a fresh login, for step-up) |
| `GET /auth/oidc/callback` | Where the provider sends the browser back; then on to `return_to` with a session |
| `POST /auth/directory/start` | `{"directory", "return_to"?, "reauth"?}` → `{"redirect"}`: where to send the browser to sign in at a trusted glidex directory (its issuer or id) |
| `GET /auth/directory/callback` | Where the directory sends the browser back; then on to `return_to` with a session |
| `POST /auth/logout` | Ends the session |
| `GET`, `POST /auth/tokens`, `DELETE /auth/tokens/ID` | Your API tokens: list, make (`{"name", "days"?}`; the token is shown once), revoke |
| `GET /auth/users`, `PUT /auth/users/NAME/roles`, `DELETE /auth/users/NAME` | People (security administrators). Roles are `role`, `role@realm`, `role@tenant:NAME` or `role@project:TENANT/NAME`; a security administrator in realms sees and sets only those realms' grants, and the rest stay |
| `POST /api/hyperstor.v1.SERVICE/METHOD` | The cluster API's call, as it came (Connect, gRPC-Web; JSON or protobuf), if your roles allow it |
| `/api/status` | `GetStatus`, as JSON |
| `/api/pgs?pool=N` | `ListPgs` for pool N |
| `/api/metrics` | Every node's metrics, parsed: `{"nodes": [{"addr", "up", "samples": [[name, labels, value]]}]}` |
| `/health` | `ok` |

Scripts send `Authorization: Bearer hst_…` with an API token instead of
the cookie, or use [`hyperstorctl --mgmt`](hyperstorctl.md); a trusted
directory's person sends a glidex API token (`gxt_…`) for this cluster,
or the directory's access token for it. Refusals
are Connect errors, as the cluster API's: `{"code", "message",
"details"}` with the reason as an `ErrorInfo` detail, and also as
`"reason"`: `unauthenticated` (401), `reauth_required` (401: log in
again), `permission_denied` (403), `csrf` (403), `bad_origin` (403),
`login_failed` (401), `rate_limited` (429), `directory_stale` (503: the
person's directory hasn't been heard from for longer than its
`max_staleness`), `not_operator` and `from_directory` (403: roles a
directory's person can't be given here). After 10 failed logins
from one address in 5 minutes, logins from it get `rate_limited`, with
`Retry-After`, until the window passes; other addresses aren't affected.

## Protections

- It answers only requests whose `Host` is one of its names on its port
  (against DNS rebinding).
- The session cookie is `HttpOnly; SameSite=Strict; Path=/`. Over
  HTTPS it is named `__Host-hs_session` and is `Secure`, with no
  `Domain`, so the browser sends it only to this host and no other site
  can set it; over plain HTTP on loopback it is `hs_session`. A cookie
  under the other name is ignored: after an upgrade that changes it,
  people log in once more. A write with the cookie must send its CSRF value in
  `X-Hyperstor-CSRF`, and a write from another site's `Origin` is
  refused.
- Pages carry a strict Content-Security-Policy and may not be framed.
- Every login and every change is audited on standard error as a JSON
  line with the person (`user:NAME`) and how they logged in, and sent to
  the cluster's audit log in `.meta`. mgmt names
  the person to the map service (`X-Hyperstor-On-Behalf-Of`, believed
  only from mgmt's certificate), whose audit then reads `…/mgmt/dashboard
  for user:NAME`; a browser's own such header is dropped.

| Exit status | Meaning |
|---|---|
| 1 | The certificates, or `--oidc`'s file or its secret, can't be read (or are wrong), or the address can't be bound |
| 2 | A missing or malformed option, or `--listen` beyond loopback without `--web-tls` and a login method |
