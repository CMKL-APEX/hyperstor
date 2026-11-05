# hyperstor-s3

The S3 gateway: Amazon S3's API over HTTPS, on the cluster's pools.
Run as many as you like, on any host that reaches the map service and
the storage nodes; they keep no state of their own, so a load balancer
may send any request to any of them.

> **Early.** This is stage 2 of 5 (see the [status](../status.md)).
> Access keys come from a file, and new buckets go in the two pools named
> on the command line; both move to the map service in stage 3, with
> `hyperstorctl s3` and `hyperstor-install --s3`. Multipart uploads,
> copies, conditional requests and multi-object deletes come in stage 4.

```text
usage: hyperstor-s3 --meta ADDR[,ADDR...] --tls DIR --keys FILE --data-pool NAME --index-pool NAME [--listen ADDR[,ADDR...]] [--reactors N] [--threads N] [--networks NAME[,NAME...]]
```

| Option | Default | Meaning |
|---|---|---|
| `--meta ADDR[,ADDR...]` | | The map service's members (their `--listen` addresses) |
| `--tls DIR` | | Holds `s3.crt` (the certificate chain, leaf first) and `s3.key`: the name clients connect to must be in the certificate. Read at start |
| `--keys FILE` | | The access keys, one per line: `ID SECRET TENANT`; `#` starts a comment. Readable by its owner only (`chmod 600`), or the gateway won't start |
| `--data-pool NAME` | | The pool new buckets keep their objects' bytes in |
| `--index-pool NAME` | | The pool new buckets keep their index in: flash, ideally |
| `--listen ADDR[,ADDR...]` | `0.0.0.0:443` | Where clients connect |
| `--reactors N` | 2 | Threads that talk to the cluster (1–64) |
| `--threads N` | 2 | Threads for HTTPS and signatures (1–64) |
| `--networks NAME[,NAME...]` | none | The data networks this host reaches storage nodes on, in order, as for [hyperstor-vhost](hyperstor-vhost.md) |

```sh
umask 077
printf '%s %s 1\n' "$KEY_ID" "$SECRET" > /etc/hyperstor/s3-keys
hyperstor-s3 --meta 192.0.2.10:8849 --tls /etc/hyperstor/s3 \
  --keys /etc/hyperstor/s3-keys --data-pool bulk --index-pool flash &
aws --endpoint-url https://s3.example.com s3 mb s3://photos
aws --endpoint-url https://s3.example.com s3 cp cat.jpg s3://photos/
```

Make up keys as long random strings (for example `openssl rand -hex 20`)
and give each user its own; the tenant number groups the buckets a key
may use. A key reaches only its tenant's buckets.

## What it answers

Path-style requests (`https://host/bucket/key`), signed with Signature
Version 4 in the `Authorization` header or in a presigned URL (up to 7
days); clients must use version 4 and path-style addressing. Requests
more than 15 minutes from the gateway's clock are refused.

| Request | Notes |
|---|---|
| ListBuckets, CreateBucket, HeadBucket, DeleteBucket, GetBucketLocation | Any key may create a bucket, for its tenant. A bucket is deleted only when empty |
| PutObject | Up to 5 GiB. `Content-MD5`, and the `x-amz-checksum-` CRC32, CRC32C, SHA1 and SHA256 values, in headers or after the body, are checked before the object appears; a body signed chunk by chunk is checked chunk by chunk |
| GetObject, HeadObject | With a `Range` of one span, and the `response-` parameters that set headers |
| DeleteObject | |
| ListObjects, ListObjectsV2 | Prefix, delimiter, pages, `start-after`, `encoding-type=url` |

Anything else is answered `501 NotImplemented`. ETags are MD5s, as S3's.
Uploads that a gateway didn't finish (it stopped, or the client went)
are cleaned up a day later.
