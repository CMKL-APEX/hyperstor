# hyperstor-s3

The S3 gateway: Amazon S3's API over HTTPS, on the cluster's pools.
Run as many as you like, on any host that reaches the map service and
the storage nodes; they keep no state of their own, so a load balancer
may send any request to any of them.

> **Early.** This is stage 3 of 5 (see the [status](../status.md)).
> Access keys and placements are kept by the map service and managed with
> [`hyperstorctl s3`](hyperstorctl.md); [`hyperstor-install
> --s3`](hyperstor-install.md) installs gateways. Multipart uploads,
> copies, conditional requests and multi-object deletes come in stage 4.

```text
usage: hyperstor-s3 --meta ADDR[,ADDR...] --tls DIR (--api ADDR[,ADDR...] --identity DIR | --keys FILE --data-pool NAME --index-pool NAME) [--listen ADDR[,ADDR...]] [--reactors N] [--threads N] [--networks NAME[,NAME...]]
```

| Option | Default | Meaning |
|---|---|---|
| `--meta ADDR[,ADDR...]` | | The map service's members (their `--listen` addresses) |
| `--tls DIR` | | Holds `s3.crt` (the certificate chain, leaf first) and `s3.key`: the name clients connect to must be in the certificate. Read at start |
| `--api ADDR[,ADDR...]` | | The cluster API of the map service's members, tried in turn: the access keys and placements come from there, every 5 seconds |
| `--identity DIR` | | With `--api`: the gateway's own certificate, `s3.crt` and `s3.key`, of role `s3`, and the cluster's `root.crt`. Only an `s3` certificate may read the keys. It names the gateway's addresses, so `--tls` may be the same directory where clients trust the cluster's root |
| `--keys FILE` | | Instead of `--api`, for running without the cluster API: the access keys, one per line: `ID SECRET TENANT`; `#` starts a comment. Readable by its owner only (`chmod 600`), or the gateway won't start |
| `--data-pool NAME` | | With `--keys`: the pool new buckets keep their objects' bytes in |
| `--index-pool NAME` | | With `--keys`: the pool new buckets keep their index in: flash, ideally |
| `--listen ADDR[,ADDR...]` | `0.0.0.0:8848` | Where clients connect |
| `--reactors N` | 2 | Threads that talk to the cluster (1–64) |
| `--threads N` | 2 | Threads for HTTPS and signatures (1–64) |
| `--networks NAME[,NAME...]` | none | The data networks this host reaches storage nodes on, in order, as for [hyperstor-vhost](hyperstor-vhost.md) |

```sh
hyperstorctl s3 placement set --name default --data-pool bulk --index-pool flash
hyperstorctl s3 key create --tenant 1 --description photos   # prints ID SECRET
hyperstor-s3 --meta 192.0.2.10:8849 --api 192.0.2.10:8850 \
  --identity /etc/hyperstor/pki --tls /etc/hyperstor/s3 &
aws --endpoint-url https://s3.example.com:8848 s3 mb s3://photos
aws --endpoint-url https://s3.example.com:8848 s3 cp cat.jpg s3://photos/
```

Give each user a key of their own; the tenant number groups the buckets
a key may use, and a key reaches only its tenant's buckets. The secret
is printed once: the map service keeps it encrypted with the cluster key
and hands it only to gateways. A revoked key (`hyperstorctl s3 key
revoke`) is refused within seconds. Gateways keep the keys they have
while the map service is away, and start only once it has answered.

New buckets go in the `default` placement's pools, or in the placement
a CreateBucket's `LocationConstraint` names (`aws s3api create-bucket
--create-bucket-configuration LocationConstraint=fast`). A
`LocationConstraint` equal to the region the client signs for counts as
none, since SDKs send their region there. Buckets keep their pools when a
placement changes.

## What it answers

Path-style requests (`https://host/bucket/key`), signed with Signature
Version 4 in the `Authorization` header or in a presigned URL (up to 7
days); clients must use version 4 and path-style addressing. Requests
more than 15 minutes from the gateway's clock are refused.

| Request | Notes |
|---|---|
| ListBuckets, CreateBucket, HeadBucket, DeleteBucket, GetBucketLocation | Any key may create a bucket, for its tenant, in a placement. A bucket is deleted only when empty |
| PutObject | Up to 5 GiB. `Content-MD5`, and the `x-amz-checksum-` CRC32, CRC32C, SHA1 and SHA256 values, in headers or after the body, are checked before the object appears; a body signed chunk by chunk is checked chunk by chunk |
| GetObject, HeadObject | With a `Range` of one span, and the `response-` parameters that set headers |
| DeleteObject | |
| ListObjects, ListObjectsV2 | Prefix, delimiter, pages, `start-after`, `encoding-type=url` |

Anything else is answered `501 NotImplemented`. ETags are MD5s, as S3's.
Uploads that a gateway didn't finish (it stopped, or the client went)
are cleaned up a day later.
