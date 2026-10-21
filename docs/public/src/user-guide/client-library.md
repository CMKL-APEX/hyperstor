# The client library

`hyperstor-client` is a Rust crate. It holds a copy of the cluster map,
keeps connections to storage nodes, splits each request at chunk
boundaries, sends the pieces in parallel to their primaries, and retries
when the map changes.

## Adding the dependency

The crate is not on crates.io yet. Depend on it from git, with the
reactor it runs on:

```toml
[dependencies]
hyperstor-client = { git = "https://github.com/CMKL-APEX/hyperstor" }
hyperstor-reactor = { git = "https://github.com/CMKL-APEX/hyperstor" }
```

Pin a revision (`rev = "..."`) while the API is changing.

## The reactor

The client runs on Hyperstor's own io_uring reactor, not on tokio or
another async runtime. `hyperstor_reactor::block_on` runs a future, and
everything it spawns, on the current thread. A `Client` belongs to the
thread that created it (it is not `Send`); use one client per thread.

## Writing and reading

```rust
use hyperstor_client::Client;
use hyperstor_reactor::block_on;

fn main() {
    let meta = "127.0.0.1:19000".parse().unwrap();
    block_on(async move {
        // The second argument identifies this client to the cluster.
        let client = Client::connect(meta, 1).await.expect("connect");

        let pool = 1;
        let volume: u128 = 0x1234;
        let data = vec![0xab; 64 * 1024];

        // Offsets and lengths are multiples of 4096.
        client.write(pool, volume, 0, data.clone()).await.expect("write");
        let back = client.read(pool, volume, 0, 64 * 1024).await.expect("read");
        assert_eq!(back, data);
    });
}
```

- `Client::connect(meta, id)` fetches the map from the map service at
  `meta`.
- `write(pool, volume, offset, data)` returns once every reachable copy
  of every chunk touched has the data (see
  [guarantees](guarantees.md)).
- `read(pool, volume, offset, len)` returns `len` bytes; blocks never
  written are zeros.
- `map()` returns the client's current copy of the cluster map, for
  example to list pools.

The full API is in the [client API reference](../reference/client-api.md).

## Choosing volume numbers

Volume numbers (lineages) are 128-bit and are not allocated by the
cluster yet: two programs that write to the same number in the same pool
write to the same volume. Pick them so they don't collide, for example at
random.
