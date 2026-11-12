# hyperstor-s3

The S3 gateway: Amazon S3's API over HTTPS, on the cluster's pools.
Run as many as you like, on any host that reaches the map service and
the storage nodes; they keep no state of their own, so a load balancer
may send any request to any of them.

> **New.** Its five stages are built (see the [status](../status.md)).
> Access keys and placements are kept by the map service and managed with
> [`hyperstorctl s3`](hyperstorctl.md); [`hyperstor-install
> --s3`](hyperstor-install.md) installs gateways; buckets, keys and
> placements are in the web UI's Object storage page and `hyperstorctl
> s3`.

```text
usage: hyperstor-s3 --meta ADDR[,ADDR...] --tls DIR (--api ADDR[,ADDR...] --identity DIR [--realm NAME] | --keys FILE --data-pool NAME --index-pool NAME) [--listen ADDR[,ADDR...]] [--domain NAME[,NAME...]] [--reactors N] [--threads N] [--networks NAME[,NAME...]]
```

| Option | Default | Meaning |
|---|---|---|
| `--meta ADDR[,ADDR...]` | | The map service's members (their `--listen` addresses) |
| `--tls DIR` | | Holds `s3.crt` (the certificate chain, leaf first) and `s3.key`: the name clients connect to must be in the certificate. Read at start |
| `--api ADDR[,ADDR...]` | | The cluster API of the map service's members, tried in turn: the access keys and placements come from there, every 5 seconds |
| `--identity DIR` | | With `--api`: the gateway's own certificate, `s3.crt` and `s3.key`, of role `s3`, and the cluster's `root.crt`. Only an `s3` certificate may read the keys. It names the gateway's addresses, so `--tls` may be the same directory where clients trust the cluster's root |
| `--realm NAME` | `default` | With `--api`: the realm this gateway serves. It knows only the keys of that realm's tenants, and its placements: another realm's key is unknown here (`InvalidAccessKeyId`) |
| `--domain NAME[,NAME...]` | | The gateway's domains: a request to `bucket.NAME` names its bucket in the host (virtual-hosted), one to `NAME` or any other host in its path. Needs a wildcard DNS name and certificate for `*.NAME`; a wildcard covers one label, so buckets with dots in their names are reached path-style |
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

Path-style requests (`https://host/bucket/key`) and, with `--domain`,
virtual-hosted ones (`https://bucket.s3.example.com/key`), signed with
Signature Version 4 in the `Authorization` header or in a presigned URL
(up to 7 days); clients must use version 4. Requests more than 15
minutes from the gateway's clock are refused. As in S3, every `x-amz-*`
header a request carries must be among those it signs (presigned URLs
included), except `x-amz-content-sha256`; otherwise the answer is 403
`AccessDenied`. Either way the bucket is
the signing key's tenant's: no host name or path names another tenant's.

| Request | Notes |
|---|---|
| ListBuckets, CreateBucket, HeadBucket, DeleteBucket, GetBucketLocation | Any key may create a bucket, for its tenant, in a placement. A bucket is deleted only when empty |
| PutObject | Up to 5 GiB. `Content-MD5`, and the `x-amz-checksum-` CRC32, CRC32C, SHA1 and SHA256 values, in headers or after the body, are checked before the object appears; a body signed chunk by chunk is checked chunk by chunk. `If-None-Match: *` writes only a new object and `If-Match` replaces only the version named; otherwise `412 PreconditionFailed` |
| GetObject, HeadObject | With a `Range` of one span, the `response-` parameters that set headers, and `If-Match`, `If-None-Match`, `If-Modified-Since` and `If-Unmodified-Since` (`304` or `412`). With `x-amz-checksum-mode: ENABLED`, a whole object's checksum: the one it was written with, else its CRC32C |
| CopyObject | From a bucket of the same tenant, up to 5 GiB. The bytes are copied, so the copy shares nothing with its source. Metadata is the source's, or the request's with `x-amz-metadata-directive: REPLACE`; the `x-amz-copy-source-if-` conditions; `x-amz-checksum-algorithm` computes a new checksum, else the source's is kept |
| CreateMultipartUpload, UploadPart, UploadPartCopy, CompleteMultipartUpload, AbortMultipartUpload, ListParts, ListMultipartUploads | Up to 10,000 parts of up to 5 GiB, each but the last at least 5 MiB. Nothing is copied at completion: the object names its parts. A part uploaded again replaces the one before. `x-amz-checksum-algorithm` with `COMPOSITE` (any algorithm) or `FULL_OBJECT` (CRC32, CRC32C) checksums. Uploads not completed are cleaned up 7 days after they began |
| DeleteObject | `If-Match` deletes only the version named |
| DeleteObjects | Up to 1000 names, with `Content-MD5` or a checksum, as S3 requires; an `ETag` per name deletes only that version |
| ListObjects, ListObjectsV2 | Prefix, delimiter, pages, `start-after`, `encoding-type=url` |

Anything else is answered `501 NotImplemented`. ETags are MD5s, as S3's.
ETags of multipart objects are the MD5 of their parts' MD5s, a dash and
the number of parts, as S3's. Uploads that a gateway didn't finish (it
stopped, or the client went) are cleaned up a day later.

## Connections

- A new connection has 10 s to finish its TLS handshake and send its
  first request's headers; a kept connection, as long from the first
  byte of each next request.
- A kept connection with no request for 2 minutes is closed, as is one
  whose answer the client stops reading for as long. SDKs keep idle
  connections for less, and open a new one when the gateway has closed
  theirs.
- A request's body may go 60 s without a byte arriving; past that the
  request fails.
- At most 4096 connections are open at once (fewer when the open-file
  limit is lower: half of it). Past that, new connections wait until
  one closes.
