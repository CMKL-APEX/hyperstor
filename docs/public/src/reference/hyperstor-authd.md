# hyperstor-authd

The root helper that checks people's passwords with PAM for
[hyperstor-mgmt](hyperstor-mgmt.md), so the management service never
runs as root. It is glidex's `glidex-authd` with hyperstor's defaults,
and `hyperstor-install` sets it up on the map service's host.

```text
usage: hyperstor-authd --config FILE
```

| Option | Default | Meaning |
|---|---|---|
| `--config FILE` | required | Settings, as JSON (installed as `/etc/hyperstor/authd.json`); if the file doesn't exist, hyperstor's defaults |

| Setting | Default | Meaning |
|---|---|---|
| `service_user` | `hyperstor` | The only user it answers, by the caller's uid |
| `allowed_groups` | `["hyperstor-users"]` | A person must be in one of these groups to log in |
| `allow_root` | `false` | Also answer root (for debugging only) |
| `failure_delay_ms` | 1000 | Delay before every refusal |
| `max_connections` | 8 | Connections served at once |

It listens on `/run/hyperstor-authd/auth.sock`, which systemd's
`hyperstor-authd.socket` creates (mode 0660, group `hyperstor`), and
checks passwords against the PAM service the caller names: `hyperstor`
(`/etc/pam.d/hyperstor`). It allows five failures per user in 15
minutes and 30 a minute in all, as glidex's does.

To let someone log in: `sudo usermod -aG hyperstor-users NAME`.

| Exit status | Meaning |
|---|---|
| 1 | The settings or the socket can't be used |
| 2 | A malformed option |
