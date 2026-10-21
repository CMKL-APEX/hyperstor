# Quick start

> To install a trial cluster as system services in one command instead,
> see [Installing a trial cluster](admin-guide/installation.md#installing-a-trial-cluster).

This starts a cluster of three storage nodes and a map service on one
Linux machine, writes to it, and shows a node failure. It needs the
[build requirements](admin-guide/installation.md#requirements) and about
3 GiB of free disk space.

## 1. Build

```sh
git clone https://github.com/CMKL-APEX/hyperstor
cd hyperstor
cargo build --release
```

The binaries are in `target/release/`: `hyperstor-meta` (the map
service), `hyperstord` (a storage node) and `hyperstor-bench` (a load
generator).

## 2. Describe the cluster

The [cluster file](reference/cluster-file.md) lists the storage nodes,
the address each one listens on, and the pools:

```sh
cat > cluster.txt <<'END'
cluster 1
node 0 rack 0 127.0.0.1:19100
node 1 rack 0 127.0.0.1:19200
node 2 rack 0 127.0.0.1:19300
pool 1 vol replicated size 3 min 2 pgs 128 chunk 4194304 domain host
END
```

Pool 1 keeps three copies of every 4 MiB chunk, each on a different
node, and accepts writes while at least two copies are reachable.

## 3. Start the services

```sh
target/release/hyperstor-meta --listen 127.0.0.1:19000 --cluster cluster.txt &
for n in 0 1 2; do
  target/release/hyperstord --node $n --meta 127.0.0.1:19000 \
    --shard 127.0.0.1:19$((n+1))00=/var/tmp/hs-n$n.img:1 &
done
```

Each storage node creates a 1 GiB file as its device and formats it on
first start.

## 4. Write and read

`hyperstor-bench remote` connects as a client, writes and reads for a
few seconds, and prints the throughput as table rows:

```sh
HS_BENCH_SECS=2 target/release/hyperstor-bench remote 127.0.0.1:19000
```

## 5. Fail a node

Stop one storage node and run the load again:

```sh
kill %2          # the first hyperstord
sleep 5
HS_BENCH_SECS=2 target/release/hyperstor-bench remote 127.0.0.1:19000
```

About three seconds after the node stops answering, the map service
marks it down and gives its placement groups to the other nodes; writes
then go on with two copies. Start the node again with the same command
and it is refilled with the writes it missed (see
[Failures and recovery](admin-guide/failures.md)).

## 6. Clean up

```sh
kill $(jobs -p)
rm /var/tmp/hs-n?.img cluster.txt
```

Next: [Concepts](concepts.md), then the
[user guide](user-guide/index.md) to write your own client or the
[administrator guide](admin-guide/index.md) to run a cluster on several
machines.
