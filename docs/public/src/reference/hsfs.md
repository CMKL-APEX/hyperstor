# hsfs

Datasets and their properties, in the order `zfs` users type them:
[hsctl](hsctl.md) run by the name `hsfs` (the installer
puts `/usr/local/bin/hsfs` as a link to it), whose commands are
`hsctl dataset`'s. See [datasets and their
properties](../admin-guide/managing.md#datasets-and-their-properties).
Run with no command, it prints the usage line, on standard error, with
status 2.

```text
usage: hsfs [--api ADDR] [--tls DIR] [--socket PATH] [--mgmt URL [--token-file F] [--mgmt-ca FILE] [--in-realm R]] [--timeout-secs N] [--json | -o json|yaml] list [-t volume|filesystem|bucket] [TENANT[/PROJECT]] | get all|PROPERTY[,PROPERTY...] DATASET | set PROPERTY=VALUE... DATASET | inherit PROPERTY... DATASET
```

| Command | Does |
|---|---|
| `list [-t KIND] [TENANT[/PROJECT]]` | `hsctl dataset list`: every dataset reached, or those below a tenant or project, of one kind with `-t` |
| `get all\|P[,P...] DATASET` | `hsctl dataset get`: the properties of a tenant, a project or a dataset, with where each comes from |
| `set P=V... DATASET` | `hsctl dataset set`: sets each, in order |
| `inherit P... DATASET` | `hsctl dataset inherit`: clears each, which is then inherited |

`DATASET` is a tenant (by name or number), `tenant/project`, or
`tenant/project/name`, with `@<volume id>` or `@bucket` where two
datasets of the project share the name.

| Option | Default | Meaning |
|---|---|---|
| `--api ADDR` | | As for hsctl: the cluster API to call |
| `--tls DIR` | | As for hsctl: the certificates to call it with |
| `--socket PATH` | | As for hsctl: the map service's local socket |
| `--mgmt URL` | | As for hsctl: call through hyperstor-mgmt, as a person |
| `--token-file F` | | As for hsctl: the person's token, with `--mgmt` |
| `--mgmt-ca FILE` | | As for hsctl: the CA mgmt's certificate is checked against |
| `--in-realm R` | | As for hsctl: act in one realm only |
| `--timeout-secs N` | | As for hsctl: how long a call may take |
| `--json` | off | Print the API's answer as JSON (`-o yaml` for YAML) |
