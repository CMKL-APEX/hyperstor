# hyperstor-mgmt

The management service: the web dashboard, people's logins, and the
cluster API on their behalf. See
[Managing a cluster](../admin-guide/managing.md#dashboard).

```text
usage: hyperstor-mgmt --listen ADDR[,ADDR...] --api ADDR[,ADDR...] --tls DIR [--metrics ADDR,ADDR...] [--authd SOCKET] [--oidc FILE] [--web-tls DIR] [--tailscale SOCKET] [--hosts NAME,...] [--ui DIR]
```

| Option | Default | Meaning |
|---|---|---|
| `--listen ADDR[,ADDR...]` | required | `IP:PORT`s to serve on (`0.0.0.0:8852,[::]:8852`: every address). Beyond loopback it needs `--web-tls`, and `--authd` or `--oidc`; the host's own names and addresses are then allowed in `Host` |
| `--api ADDR[,ADDR...]` | required | The [cluster API](cluster-api.md), `IP:PORT`: every map service member, separated by commas. A call a member can't serve (it isn't the leader) goes to the next |
| `--tls DIR` | required | Its identity: `DIR/mgmt.crt`, `DIR/mgmt.key` and `DIR/root.crt` |
| `--metrics ADDR,ADDR...` | none | The storage nodes' metrics addresses (`hyperstord --admin`) |
| `--authd SOCKET` | none | People log in with their password, checked by [`hyperstor-authd`](hyperstor-authd.md) on `SOCKET`. Without it, the dashboard is read-only and on loopback only |
| `--oidc FILE` | none | People log in with an identity provider (OIDC), configured in `FILE`; see [Identity providers](#identity-providers) |
| `--web-tls DIR` | none | Serve HTTPS with `DIR/web.crt` (the chain) and `DIR/web.key`, re-read within a minute when they change |
| `--tailscale SOCKET` | none | Also serve the node's Tailscale certificate to clients naming its `ts.net` names, fetched from `tailscaled`'s local API at `SOCKET` (`/run/tailscale/tailscaled.sock`); needs `--web-tls` |
| `--hosts NAME,...` | none | Names people reach it by, allowed in the `Host` header besides `localhost`, `127.0.0.1` and `[::1]` |
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

## Logins

People log in with their user name and password on the host (PAM
service `hyperstor`); they must be in the group `hyperstor-users`. A
first login makes an account with no roles. What a role allows:

| Role | May |
|---|---|
| `viewer` | Read: status, PGs, volumes, snapshots, operations, members, the dashboard |
| `operator` | Also take devices out and in, drain nodes, cancel operations, take snapshots, restore from the trash |
| `storage-admin` | Also create and delete volumes and snapshots, clones, roll volumes back, purge the trash and set its grace periods, add and remove members |
| `security-admin` | People and their roles, join tokens, certificates, map backups; nothing else |

Everyone may make API tokens for themselves. Deleting a volume, a
snapshot or a member, rolling a volume back, purging the trash or
changing its grace period, making a join token or an API token, revoking
a certificate, fetching a map backup, and changing people's roles need a
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
| `allowed_domains` | any | Email domains that may log in |
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
| `GET /auth/methods` | `{"pam": bool, "oidc": bool, "claim_open": bool}`; no login needed |
| `POST /auth/login` | `{"user", "password", "claim"?}` → `{"user", "roles", "csrf"}` and the session cookie `hs_session` |
| `GET /auth/whoami` | `{"user", "roles", "method", "csrf"}` |
| `POST /auth/oidc/start` | `{"return_to"?, "claim"?, "reauth"?}` → `{"redirect"}`: where to send the browser to log in with the provider (`reauth`: a fresh login, for step-up) |
| `GET /auth/oidc/callback` | Where the provider sends the browser back; then on to `return_to` with a session |
| `POST /auth/logout` | Ends the session |
| `GET`, `POST /auth/tokens`, `DELETE /auth/tokens/ID` | Your API tokens: list, make (`{"name", "days"?}`; the token is shown once), revoke |
| `GET /auth/users`, `PUT /auth/users/NAME/roles`, `DELETE /auth/users/NAME` | People (security administrators) |
| `POST /api/hyperstor.v1.SERVICE/METHOD` | The cluster API's call, as it came (Connect, gRPC-Web; JSON or protobuf), if your roles allow it |
| `/api/status` | `GetStatus`, as JSON |
| `/api/pgs?pool=N` | `ListPgs` for pool N |
| `/api/metrics` | Every node's metrics, parsed: `{"nodes": [{"addr", "up", "samples": [[name, labels, value]]}]}` |
| `/health` | `ok` |

Scripts send `Authorization: Bearer hst_…` with an API token instead of
the cookie, or use [`hyperstorctl --mgmt`](hyperstorctl.md). Refusals
are Connect errors, as the cluster API's: `{"code", "message",
"details"}` with the reason as an `ErrorInfo` detail, and also as
`"reason"`: `unauthenticated` (401), `reauth_required` (401: log in
again), `permission_denied` (403), `csrf` (403), `bad_origin` (403),
`login_failed` (401), `rate_limited` (429).

## Protections

- It answers only requests whose `Host` is one of its names on its port
  (against DNS rebinding).
- The session cookie is `HttpOnly; SameSite=Strict` (and `Secure` over
  HTTPS). A write with the cookie must send its CSRF value in
  `X-Hyperstor-CSRF`, and a write from another site's `Origin` is
  refused.
- Pages carry a strict Content-Security-Policy and may not be framed.
- Every login and every change is audited on standard error as a JSON
  line with the person (`user:NAME`) and how they logged in. mgmt names
  the person to the map service (`X-Hyperstor-On-Behalf-Of`, believed
  only from mgmt's certificate), whose audit then reads `…/mgmt/dashboard
  for user:NAME`; a browser's own such header is dropped.

| Exit status | Meaning |
|---|---|
| 1 | The certificates, or `--oidc`'s file or its secret, can't be read (or are wrong), or the address can't be bound |
| 2 | A missing or malformed option, or `--listen` beyond loopback without `--web-tls` and a login method |
