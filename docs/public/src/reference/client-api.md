# Client API

The API reference of the crates a client program uses is generated from
the source with rustdoc:

- [`hyperstor_client`](../../api/hyperstor_client/index.html): `Client`,
  `ClientError`;
- [`hyperstor_reactor`](../../api/hyperstor_reactor/index.html): the
  reactor the client runs on (`block_on`, `spawn`).

The links work on the published site. In a checkout, build the same
reference with:

```sh
cargo doc --no-deps -p hyperstor-client -p hyperstor-reactor --open
```

The [client library guide](../user-guide/client-library.md) shows how to
use them.
