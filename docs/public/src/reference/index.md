# Reference

| Page | Describes |
|---|---|
| [Cluster file](cluster-file.md) | The file that describes nodes and pools |
| [hyperstor-meta](hyperstor-meta.md) | The map service's options |
| [hyperstord](hyperstord.md) | The storage node's options |
| [hyperstor-bench](hyperstor-bench.md) | The load generator's modes |
| [hyperstorctl](hyperstorctl.md) | Managing and testing a cluster |
| [hyperstor-ublk](hyperstor-ublk.md) | A volume as a Linux block device |
| [hyperstor-install](hyperstor-install.md) | Host checks and the installer |
| [hyperstor-mgmt](hyperstor-mgmt.md) | The dashboard |
| [hyperstor-authd](hyperstor-authd.md) | Checks people's passwords for the management service |
| [Cluster API](cluster-api.md) | The `hyperstor.v1` API, with `curl` examples |
| [Client API](client-api.md) | The Rust client library |

Each binary prints its usage line and exits with status 2 when an option
is missing or malformed. The usage lines on these pages are checked
against the binaries on every change.
