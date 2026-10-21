# Measuring performance

`hyperstor-bench` measures a device, the engine alone, or a running
cluster. It prints Markdown table rows: test, operations per second,
MB/s, and latencies. Each test runs for `HS_BENCH_SECS` seconds (3 by
default). All modes are in the [reference](../reference/hyperstor-bench.md).

## Before a cluster

Measure each device raw, then with the engine on top:

```sh
hyperstor-bench raw /var/lib/hyperstor/bench.img 2
hyperstor-bench engine /var/lib/hyperstor/bench.img 2
rm /var/lib/hyperstor/bench.img
```

The gap between the two is the engine's cost. Run it on the filesystem
or device the node will use, never on a device that holds data: the file
is overwritten.

## A running cluster

```sh
HS_BENCH_SECS=10 hyperstor-bench remote 192.0.2.10:9000
```

This connects as a client, writes to and reads from pool 1, and reports
4 KiB random and 1 MiB sequential results. It writes test data into the
pool: use a test cluster.

## Tips

- Run the client on a host other than the storage nodes, or results
  include the client's CPU use.
- Pin storage threads with `--cpus` and compare; unpinned threads move
  between cores.
- Compare against the raw device numbers: a replicated write costs one
  network round trip and a flush on every copy.
