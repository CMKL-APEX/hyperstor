"""AWS's Python SDK (boto3) against a gateway (14-s3.md §14.7, stages 2 to 5).

Run by hyperstor-daemon's `s3_gateway_serves_an_sdk`, which starts a
cluster and a gateway and passes, in the environment: HS_S3_ENDPOINT,
HS_S3_CA (the CA that signed the gateway's certificate), HS_S3_DOMAIN (the
gateway's --domain, which this script resolves to the endpoint's host),
and two keys of
different tenants, HS_S3_KEY/HS_S3_SECRET and HS_S3_KEY2/HS_S3_SECRET2,
made for the run. Prints "SDK OK" at the end; any failure raises.
"""

import base64
import hashlib
import os
import ssl
import urllib.error
import urllib.request

import boto3
from botocore.config import Config
from botocore.exceptions import ClientError, MissingDependencyException

EP = os.environ["HS_S3_ENDPOINT"]
CA = os.environ["HS_S3_CA"]


def client(key, secret):
    return boto3.client(
        "s3",
        endpoint_url=EP,
        aws_access_key_id=key,
        aws_secret_access_key=secret,
        region_name="us-east-1",
        verify=CA,
        # SigV4 for presigned URLs too: older botocore presigns with V2.
        config=Config(
            s3={"addressing_style": "path"},
            signature_version="s3v4",
            retries={"max_attempts": 1},
        ),
    )


s3 = client(os.environ["HS_S3_KEY"], os.environ["HS_S3_SECRET"])
theirs = client(os.environ["HS_S3_KEY2"], os.environ["HS_S3_SECRET2"])


def code(f):
    """The S3 error code `f` fails with (the status, for HEAD)."""
    try:
        f()
    except ClientError as e:
        if not e.response["Error"].get("Code"):
            print(e.response)
        return e.response["Error"]["Code"]
    raise AssertionError("no error")


def get(key, **kw):
    return s3.get_object(Bucket="sdk", Key=key, **kw)["Body"].read()


# Buckets: made by any key, for its tenant (D232). Names are per tenant,
# and another tenant's buckets don't exist to a key (D243, D244).
s3.create_bucket(Bucket="sdk")
c = code(lambda: s3.create_bucket(Bucket="sdk"))
assert c == "BucketAlreadyOwnedByYou", c
assert code(lambda: s3.create_bucket(Bucket="Bad_Name")) == "InvalidBucketName"
assert [b["Name"] for b in s3.list_buckets()["Buckets"]] == ["sdk"]
assert theirs.list_buckets()["Buckets"] == []
s3.head_bucket(Bucket="sdk")
assert code(lambda: theirs.head_bucket(Bucket="sdk")) == "404"
c = code(lambda: theirs.get_object(Bucket="sdk", Key="x"))
assert c == "NoSuchBucket", c
assert code(lambda: theirs.delete_bucket(Bucket="sdk")) == "NoSuchBucket"
assert code(lambda: s3.head_bucket(Bucket="nope")) == "404"
# The same name in another tenant is another bucket.
theirs.create_bucket(Bucket="sdk")
theirs.put_object(Bucket="sdk", Key="x", Body=b"theirs")
assert code(lambda: get("x")) == "NoSuchKey"
assert [b["Name"] for b in theirs.list_buckets()["Buckets"]] == ["sdk"]
theirs.delete_object(Bucket="sdk", Key="x")
theirs.delete_bucket(Bucket="sdk")
assert code(lambda: theirs.head_bucket(Bucket="sdk")) == "404"
s3.head_bucket(Bucket="sdk")
assert s3.get_bucket_location(Bucket="sdk").get("LocationConstraint") is None

# Objects: inline and in chunks (the gateway's are 64 KiB here).
small = b"hello"
big = os.urandom(300_017)
r = s3.put_object(
    Bucket="sdk", Key="a/small.txt", Body=small, ContentType="text/plain", Metadata={"k": "v"}
)
assert r["ETag"] == '"%s"' % hashlib.md5(small).hexdigest()
s3.put_object(Bucket="sdk", Key="a/b/big", Body=big)
o = s3.get_object(Bucket="sdk", Key="a/small.txt")
assert o["Body"].read() == small
assert o["ContentType"] == "text/plain" and o["Metadata"] == {"k": "v"}
assert get("a/b/big") == big
r = s3.get_object(Bucket="sdk", Key="a/b/big", Range="bytes=65530-200000")
assert r["Body"].read() == big[65530:200001]
assert r["ContentRange"] == "bytes 65530-200000/%d" % len(big)
assert get("a/b/big", Range="bytes=-10") == big[-10:]
assert code(lambda: get("a/b/big", Range="bytes=%d-" % len(big))) == "InvalidRange"
h = s3.head_object(Bucket="sdk", Key="a/b/big")
assert h["ContentLength"] == len(big) and h["ETag"] == '"%s"' % hashlib.md5(big).hexdigest()
assert code(lambda: s3.head_object(Bucket="sdk", Key="nope")) == "404"
assert code(lambda: get("nope")) == "NoSuchKey"
s3.put_object(Bucket="sdk", Key="empty", Body=b"")
assert get("empty") == b""
# Overwritten: the new bytes.
s3.put_object(Bucket="sdk", Key="a/b/big", Body=big[::-1])
assert get("a/b/big") == big[::-1]

# Names with characters that need encoding.
odd = "sp ace/ü+%&?=#~!$'()*,;:@[]"
s3.put_object(Bucket="sdk", Key=odd, Body=b"odd")
assert get(odd) == b"odd"

# Checksums, in a header or after an aws-chunked body; a bad Content-MD5.
checked = []
for alg in ["CRC32", "CRC32C", "SHA1", "SHA256"]:
    try:
        s3.put_object(Bucket="sdk", Key="c/" + alg, Body=big, ChecksumAlgorithm=alg)
    except MissingDependencyException:
        # CRC32C needs awscrt in older botocore.
        continue
    assert get("c/" + alg) == big
    checked.append(alg)
assert "CRC32" in checked and "SHA256" in checked, checked
# The checksum is kept and given back when asked for.
h = s3.head_object(Bucket="sdk", Key="c/SHA256", ChecksumMode="ENABLED")
assert h["ChecksumSHA256"] == base64.b64encode(hashlib.sha256(big).digest()).decode(), h
assert "ChecksumSHA256" not in s3.head_object(Bucket="sdk", Key="c/SHA256")
bad = base64.b64encode(hashlib.md5(b"other").digest()).decode()
assert code(lambda: s3.put_object(Bucket="sdk", Key="bad", Body=small, ContentMD5=bad)) == "BadDigest"
assert code(lambda: s3.head_object(Bucket="sdk", Key="bad")) == "404"
crc = base64.b64encode(b"\0\0\0\0").decode()
assert code(lambda: s3.put_object(Bucket="sdk", Key="bad", Body=small, ChecksumCRC32=crc)) in (
    "BadDigest",
    "InvalidRequest",
)

# Listings, in pages, with a delimiter, both versions.
keys = ["l/%03d" % i for i in range(25)]
for k in keys + ["l/sub/x", "l/sub/y", "l/zz"]:
    s3.put_object(Bucket="sdk", Key=k, Body=k.encode())
# Pages of 13 end on the common prefix: the next starts past it.
for op in ["list_objects_v2", "list_objects"]:
    for size in [7, 13]:
        got, prefixes = [], []
        pages = s3.get_paginator(op).paginate(
            Bucket="sdk", Prefix="l/", Delimiter="/", PaginationConfig={"PageSize": size}
        )
        for page in pages:
            got += [o["Key"] for o in page.get("Contents", [])]
            prefixes += [c["Prefix"] for c in page.get("CommonPrefixes", [])]
        assert got == keys + ["l/zz"] and prefixes == ["l/sub/"], (op, size, got, prefixes)
r = s3.list_objects_v2(Bucket="sdk", Prefix="l/", StartAfter="l/020")
assert [o["Key"] for o in r["Contents"]] == keys[21:] + ["l/sub/x", "l/sub/y", "l/zz"]
r = s3.list_objects_v2(Bucket="sdk", Prefix="l/", Delimiter="/", StartAfter="l/sub/")
assert [o["Key"] for o in r["Contents"]] == ["l/zz"] and "CommonPrefixes" not in r
r = s3.list_objects_v2(Bucket="sdk", Prefix="a/")
assert [(o["Key"], o["Size"]) for o in r["Contents"]] == [("a/b/big", len(big)), ("a/small.txt", 5)]

# Presigned URLs, as a browser or curl would use them.
ctx = ssl.create_default_context(cafile=CA)
# As Python 3.13 checks by default, whatever this Python's default.
ctx.verify_flags |= ssl.VERIFY_X509_STRICT
url = s3.generate_presigned_url("get_object", Params={"Bucket": "sdk", "Key": "a/small.txt"}, ExpiresIn=60)
assert urllib.request.urlopen(url, context=ctx).read() == small
put = s3.generate_presigned_url("put_object", Params={"Bucket": "sdk", "Key": "presigned"}, ExpiresIn=60)
urllib.request.urlopen(urllib.request.Request(put, data=b"pp", method="PUT"), context=ctx)
assert get("presigned") == b"pp"
try:
    urllib.request.urlopen(url.replace("small.txt", "b/big"), context=ctx)
    raise AssertionError("a changed URL worked")
except urllib.error.HTTPError as e:
    assert e.code == 403

# Conditional requests (D194). Reads: 304 and 412 as S3 answers them.
tag = s3.head_object(Bucket="sdk", Key="a/small.txt")["ETag"]
assert code(lambda: get("a/small.txt", IfNoneMatch=tag)) == "304"
assert get("a/small.txt", IfMatch=tag) == small
assert code(lambda: get("a/small.txt", IfMatch='"0"')) == "PreconditionFailed"
assert get("a/small.txt", IfNoneMatch='"0"') == small
when = s3.head_object(Bucket="sdk", Key="a/small.txt")["LastModified"]
assert code(lambda: get("a/small.txt", IfModifiedSince=when)) == "304"
assert code(lambda: s3.head_object(Bucket="sdk", Key="a/small.txt", IfMatch='"0"')) == "412"
# Writes, where this botocore has them: create only, or replace only
# the version read.
put_args = s3.meta.service_model.operation_model("PutObject").input_shape.members
if "IfNoneMatch" in put_args:
    c = code(lambda: s3.put_object(Bucket="sdk", Key="a/small.txt", Body=b"no", IfNoneMatch="*"))
    assert c == "PreconditionFailed", c
    s3.put_object(Bucket="sdk", Key="once", Body=b"1", IfNoneMatch="*")
    assert get("once") == b"1"
    assert get("a/small.txt") == small
if "IfMatch" in put_args:
    t = s3.head_object(Bucket="sdk", Key="once")["ETag"]
    s3.put_object(Bucket="sdk", Key="once", Body=b"2", IfMatch=t)
    c = code(lambda: s3.put_object(Bucket="sdk", Key="once", Body=b"3", IfMatch=t))
    assert c == "PreconditionFailed", c
    assert get("once") == b"2"

# Deleting many at once: names that don't exist are deleted too, as S3
# says; a wrong ETag (where this botocore sends one) is refused.
for k in ["m/1", "m/2", "m/3"]:
    s3.put_object(Bucket="sdk", Key=k, Body=b"m")
r = s3.delete_objects(Bucket="sdk", Delete={"Objects": [{"Key": k} for k in ["m/1", "m/2", "m/none"]]})
assert sorted(d["Key"] for d in r["Deleted"]) == ["m/1", "m/2", "m/none"], r
assert "Errors" not in r, r
assert [o["Key"] for o in s3.list_objects_v2(Bucket="sdk", Prefix="m/")["Contents"]] == ["m/3"]
r = s3.delete_objects(Bucket="sdk", Delete={"Objects": [{"Key": "m/3"}], "Quiet": True})
assert "Deleted" not in r and "Errors" not in r, r
ident = s3.meta.service_model.shape_for("ObjectIdentifier").members
if "ETag" in ident:
    s3.put_object(Bucket="sdk", Key="m/4", Body=b"m")
    r = s3.delete_objects(Bucket="sdk", Delete={"Objects": [{"Key": "m/4", "ETag": '"0"'}]})
    assert [e["Code"] for e in r["Errors"]] == ["PreconditionFailed"], r
    s3.delete_object(Bucket="sdk", Key="m/4")

# Multipart uploads (D196): parts as uploaded, listed, completed with
# some of them; an upload aborted; a part copied from an object.
MIB = 1 << 20
p1, p2, p3 = os.urandom(5 * MIB), os.urandom(5 * MIB + 3), os.urandom(1000)
up = s3.create_multipart_upload(Bucket="sdk", Key="mp/obj", ContentType="x/mp", Metadata={"m": "1"})
uid = up["UploadId"]
t1 = s3.upload_part(Bucket="sdk", Key="mp/obj", UploadId=uid, PartNumber=1, Body=os.urandom(MIB))["ETag"]
t1 = s3.upload_part(Bucket="sdk", Key="mp/obj", UploadId=uid, PartNumber=1, Body=p1)["ETag"]
t2 = s3.upload_part(Bucket="sdk", Key="mp/obj", UploadId=uid, PartNumber=2, Body=p2)["ETag"]
t3 = s3.upload_part(Bucket="sdk", Key="mp/obj", UploadId=uid, PartNumber=3, Body=p3)["ETag"]
assert t1 == '"%s"' % hashlib.md5(p1).hexdigest()
listed = s3.list_parts(Bucket="sdk", Key="mp/obj", UploadId=uid)["Parts"]
assert [(p["PartNumber"], p["Size"]) for p in listed] == [(1, len(p1)), (2, len(p2)), (3, 1000)], listed
page = s3.list_parts(Bucket="sdk", Key="mp/obj", UploadId=uid, MaxParts=2)
assert page["IsTruncated"] and page["NextPartNumberMarker"] == 2
ups = s3.list_multipart_uploads(Bucket="sdk")["Uploads"]
assert [(u["Key"], u["UploadId"]) for u in ups] == [("mp/obj", uid)], ups
assert "Contents" not in s3.list_objects_v2(Bucket="sdk", Prefix="mp/")
# A part but the last smaller than 5 MiB, out of order, or not as named.
c = code(lambda: s3.complete_multipart_upload(
    Bucket="sdk", Key="mp/obj", UploadId=uid,
    MultipartUpload={"Parts": [{"PartNumber": 3, "ETag": t3}, {"PartNumber": 2, "ETag": t2}]}))
assert c == "InvalidPartOrder", c
c = code(lambda: s3.complete_multipart_upload(
    Bucket="sdk", Key="mp/obj", UploadId=uid,
    MultipartUpload={"Parts": [{"PartNumber": 3, "ETag": t3}, {"PartNumber": 4, "ETag": t3}]}))
assert c in ("EntityTooSmall", "InvalidPart"), c
c = code(lambda: s3.complete_multipart_upload(
    Bucket="sdk", Key="mp/obj", UploadId=uid, MultipartUpload={"Parts": [{"PartNumber": 1, "ETag": t2}]}))
assert c == "InvalidPart", c
r = s3.complete_multipart_upload(
    Bucket="sdk", Key="mp/obj", UploadId=uid,
    MultipartUpload={"Parts": [{"PartNumber": 1, "ETag": t1}, {"PartNumber": 3, "ETag": t3}]})
want = hashlib.md5(hashlib.md5(p1).digest() + hashlib.md5(p3).digest()).hexdigest() + "-2"
assert r["ETag"] == '"%s"' % want, r
o = s3.get_object(Bucket="sdk", Key="mp/obj")
assert o["Body"].read() == p1 + p3 and o["ContentType"] == "x/mp" and o["Metadata"] == {"m": "1"}
assert get("mp/obj", Range="bytes=%d-%d" % (len(p1) - 5, len(p1) + 4)) == (p1 + p3)[len(p1) - 5 : len(p1) + 5]
assert s3.list_multipart_uploads(Bucket="sdk").get("Uploads", []) == []
assert code(lambda: s3.list_parts(Bucket="sdk", Key="mp/obj", UploadId=uid)) == "NoSuchUpload"
# Aborted: its parts go, and it's gone.
gone = s3.create_multipart_upload(Bucket="sdk", Key="mp/gone")["UploadId"]
s3.upload_part(Bucket="sdk", Key="mp/gone", UploadId=gone, PartNumber=1, Body=b"x")
s3.abort_multipart_upload(Bucket="sdk", Key="mp/gone", UploadId=gone)
c = code(lambda: s3.upload_part(Bucket="sdk", Key="mp/gone", UploadId=gone, PartNumber=2, Body=b"y"))
assert c == "NoSuchUpload", c
assert code(lambda: s3.abort_multipart_upload(Bucket="sdk", Key="mp/gone", UploadId="0" * 32)) == "NoSuchUpload"
# A part copied from a range of an object, with a composite SHA256.
cp = s3.create_multipart_upload(Bucket="sdk", Key="mp/copied", ChecksumAlgorithm="SHA256")["UploadId"]
r1 = s3.upload_part_copy(
    Bucket="sdk", Key="mp/copied", UploadId=cp, PartNumber=1,
    CopySource={"Bucket": "sdk", "Key": "mp/obj"}, CopySourceRange="bytes=0-%d" % (5 * MIB - 1))
r2 = s3.upload_part(Bucket="sdk", Key="mp/copied", UploadId=cp, PartNumber=2, Body=b"tail", ChecksumAlgorithm="SHA256")
s1 = base64.b64encode(hashlib.sha256(p1[: 5 * MIB]).digest()).decode()
assert r1["CopyPartResult"]["ChecksumSHA256"] == s1, r1
r = s3.complete_multipart_upload(
    Bucket="sdk", Key="mp/copied", UploadId=cp,
    MultipartUpload={"Parts": [
        {"PartNumber": 1, "ETag": r1["CopyPartResult"]["ETag"], "ChecksumSHA256": s1},
        {"PartNumber": 2, "ETag": r2["ETag"], "ChecksumSHA256": r2["ChecksumSHA256"]},
    ]})
assert get("mp/copied") == p1[: 5 * MIB] + b"tail"
composite = hashlib.sha256(hashlib.sha256(p1[: 5 * MIB]).digest() + hashlib.sha256(b"tail").digest()).digest()
h = s3.head_object(Bucket="sdk", Key="mp/copied", ChecksumMode="ENABLED")
assert h["ChecksumSHA256"] == base64.b64encode(composite).decode() + "-2", h
# CRC64NVME, which botocore computes only with awscrt (HS_S3_SDK_CRC64
# makes it required): kept and given back, a wrong value refused, and a
# multipart upload's, FULL_OBJECT only, combined from its parts'.
try:
    from awscrt import checksums as crt
except ImportError:
    crt = None
if crt is None:
    assert not os.environ.get("HS_S3_SDK_CRC64"), "HS_S3_SDK_CRC64 is set but there is no awscrt"
    print("CRC64NVME skipped: no awscrt")
else:
    def crc64(data):
        return base64.b64encode(crt.crc64nvme(data).to_bytes(8, "big")).decode()

    s3.put_object(Bucket="sdk", Key="c/CRC64NVME", Body=big, ChecksumAlgorithm="CRC64NVME")
    h = s3.head_object(Bucket="sdk", Key="c/CRC64NVME", ChecksumMode="ENABLED")
    assert h["ChecksumCRC64NVME"] == crc64(big), h
    assert h.get("ChecksumType", "FULL_OBJECT") == "FULL_OBJECT", h
    # botocore checks the value it is given back against the body.
    o = s3.get_object(Bucket="sdk", Key="c/CRC64NVME", ChecksumMode="ENABLED")
    assert o["Body"].read() == big
    assert o["ChecksumCRC64NVME"] == crc64(big), o
    # In a header rather than after the body.
    s3.put_object(Bucket="sdk", Key="c/CRC64NVME-h", Body=small, ChecksumCRC64NVME=crc64(small))
    assert s3.head_object(Bucket="sdk", Key="c/CRC64NVME-h", ChecksumMode="ENABLED")["ChecksumCRC64NVME"] == crc64(small)
    wrong = base64.b64encode(bytes(8)).decode()
    c = code(lambda: s3.put_object(Bucket="sdk", Key="bad64", Body=small, ChecksumCRC64NVME=wrong))
    assert c in ("BadDigest", "InvalidRequest"), c
    assert code(lambda: s3.head_object(Bucket="sdk", Key="bad64")) == "404"
    # Multipart: FULL_OBJECT without being asked, from the parts' CRCs.
    q1, q2 = os.urandom(5 * MIB), os.urandom(3 * MIB + 11)
    u = s3.create_multipart_upload(Bucket="sdk", Key="mp/crc64", ChecksumAlgorithm="CRC64NVME")
    assert u.get("ChecksumType", "FULL_OBJECT") == "FULL_OBJECT", u
    r1 = s3.upload_part(Bucket="sdk", Key="mp/crc64", UploadId=u["UploadId"], PartNumber=1, Body=q1,
                        ChecksumAlgorithm="CRC64NVME")
    r2 = s3.upload_part(Bucket="sdk", Key="mp/crc64", UploadId=u["UploadId"], PartNumber=2, Body=q2,
                        ChecksumAlgorithm="CRC64NVME")
    assert r1["ChecksumCRC64NVME"] == crc64(q1) and r2["ChecksumCRC64NVME"] == crc64(q2), (r1, r2)
    r = s3.complete_multipart_upload(
        Bucket="sdk", Key="mp/crc64", UploadId=u["UploadId"],
        MultipartUpload={"Parts": [
            {"PartNumber": 1, "ETag": r1["ETag"], "ChecksumCRC64NVME": r1["ChecksumCRC64NVME"]},
            {"PartNumber": 2, "ETag": r2["ETag"], "ChecksumCRC64NVME": r2["ChecksumCRC64NVME"]},
        ]})
    h = s3.head_object(Bucket="sdk", Key="mp/crc64", ChecksumMode="ENABLED")
    assert h["ChecksumCRC64NVME"] == crc64(q1 + q2), (h, crc64(q1 + q2))
    assert h.get("ChecksumType", "FULL_OBJECT") == "FULL_OBJECT", h
    o = s3.get_object(Bucket="sdk", Key="mp/crc64", ChecksumMode="ENABLED")
    assert o["Body"].read() == q1 + q2
    # COMPOSITE CRC64NVME doesn't exist in S3: refused.
    c = code(lambda: s3.create_multipart_upload(Bucket="sdk", Key="mp/c64", ChecksumAlgorithm="CRC64NVME",
                                                ChecksumType="COMPOSITE"))
    assert c == "InvalidRequest", c
    print("CRC64NVME OK")

# boto3's own managed transfer, in parts of 5 MiB.
from boto3.s3.transfer import TransferConfig
import io
blob = os.urandom(12 * MIB + 7)
s3.upload_fileobj(io.BytesIO(blob), "sdk", "mp/managed",
                  Config=TransferConfig(multipart_threshold=5 * MIB, multipart_chunksize=5 * MIB))
assert s3.head_object(Bucket="sdk", Key="mp/managed")["ETag"].endswith('-3"')
out = io.BytesIO()
s3.download_fileobj("sdk", "mp/managed", out, Config=TransferConfig(multipart_chunksize=4 * MIB))
assert out.getvalue() == blob

# Virtual-hosted requests (D245): the bucket in the host, under the
# gateway's domain, resolved here to the endpoint's address; the same
# buckets as path-style, the tenant the signer's.
import socket
import urllib.parse

DOMAIN = os.environ["HS_S3_DOMAIN"]
ep = urllib.parse.urlsplit(EP)
real_getaddrinfo = socket.getaddrinfo


def getaddrinfo(host, *a, **kw):
    if host == DOMAIN or host.endswith("." + DOMAIN):
        host = ep.hostname
    return real_getaddrinfo(host, *a, **kw)


socket.getaddrinfo = getaddrinfo
vh = boto3.client(
    "s3",
    endpoint_url="https://%s:%d" % (DOMAIN, ep.port),
    aws_access_key_id=os.environ["HS_S3_KEY"],
    aws_secret_access_key=os.environ["HS_S3_SECRET"],
    region_name="us-east-1",
    verify=CA,
    config=Config(s3={"addressing_style": "virtual"}, signature_version="s3v4", retries={"max_attempts": 1}),
)
vh.create_bucket(Bucket="vhost")
vh.put_object(Bucket="vhost", Key="a b/c", Body=b"virtual")
assert vh.get_object(Bucket="vhost", Key="a b/c")["Body"].read() == b"virtual"
assert [o["Key"] for o in vh.list_objects_v2(Bucket="vhost")["Contents"]] == ["a b/c"]
assert s3.get_object(Bucket="vhost", Key="a b/c")["Body"].read() == b"virtual"
assert "vhost" in [b["Name"] for b in vh.list_buckets()["Buckets"]]
url = vh.generate_presigned_url("get_object", Params={"Bucket": "vhost", "Key": "a b/c"}, ExpiresIn=60)
assert urllib.parse.urlsplit(url).hostname == "vhost." + DOMAIN, url
assert urllib.request.urlopen(url, context=ctx).read() == b"virtual"
vh.delete_object(Bucket="vhost", Key="a b/c")
vh.delete_bucket(Bucket="vhost")
assert code(lambda: s3.head_bucket(Bucket="vhost")) == "404"

# Keys that don't sign, and what isn't built yet.
assert code(lambda: client(os.environ["HS_S3_KEY"], "wrong").list_buckets()) == "SignatureDoesNotMatch"
assert code(lambda: client("NOSUCHKEY", "x").list_buckets()) == "InvalidAccessKeyId"
assert code(lambda: s3.get_bucket_tagging(Bucket="sdk")) == "NotImplemented"

# Copies (D193): the bytes and metadata, or new metadata; conditions on
# the source; never from another tenant's bucket.
r = s3.copy_object(Bucket="sdk", Key="copy", CopySource="sdk/a/small.txt")
assert r["CopyObjectResult"]["ETag"] == '"%s"' % hashlib.md5(small).hexdigest(), r
o = s3.get_object(Bucket="sdk", Key="copy")
assert o["Body"].read() == small and o["ContentType"] == "text/plain" and o["Metadata"] == {"k": "v"}
s3.copy_object(
    Bucket="sdk", Key="copy", CopySource={"Bucket": "sdk", "Key": "a/b/big"},
    MetadataDirective="REPLACE", ContentType="x/y", Metadata={"n": "1"},
)
o = s3.get_object(Bucket="sdk", Key="copy")
assert o["Body"].read() == big[::-1] and o["ContentType"] == "x/y" and o["Metadata"] == {"n": "1"}
s3.copy_object(Bucket="sdk", Key=odd + "2", CopySource={"Bucket": "sdk", "Key": odd})
assert get(odd + "2") == b"odd"
c = code(lambda: s3.copy_object(Bucket="sdk", Key="copy", CopySource="sdk/copy"))
assert c == "InvalidRequest", c
c = code(lambda: s3.copy_object(Bucket="sdk", Key="x", CopySource="sdk/a/small.txt", CopySourceIfMatch='"0"'))
assert c == "PreconditionFailed", c
assert code(lambda: s3.copy_object(Bucket="sdk", Key="x", CopySource="sdk/nope")) == "NoSuchKey"
theirs.create_bucket(Bucket="theirs")
c = code(lambda: theirs.copy_object(Bucket="theirs", Key="x", CopySource="sdk/copy"))
assert c == "NoSuchBucket", c
theirs.delete_bucket(Bucket="theirs")
r = s3.copy_object(Bucket="sdk", Key="copy2", CopySource="sdk/c/SHA256", ChecksumAlgorithm="SHA1")
assert r["CopyObjectResult"]["ChecksumSHA1"] == base64.b64encode(hashlib.sha1(big).digest()).decode(), r
# A copy is its own: the source deleted, the copy remains.
s3.delete_object(Bucket="sdk", Key="c/SHA256")
h = s3.head_object(Bucket="sdk", Key="copy2", ChecksumMode="ENABLED")
assert h["ChecksumSHA1"] == r["CopyObjectResult"]["ChecksumSHA1"]
assert get("copy2") == big

# Deletes.
s3.delete_object(Bucket="sdk", Key="a/small.txt")
assert code(lambda: get("a/small.txt")) == "NoSuchKey"
s3.delete_object(Bucket="sdk", Key="a/small.txt")
assert code(lambda: s3.delete_bucket(Bucket="sdk")) == "BucketNotEmpty"
for page in s3.get_paginator("list_objects_v2").paginate(Bucket="sdk"):
    gone = page.get("Contents", [])
    if gone:
        r = s3.delete_objects(Bucket="sdk", Delete={"Objects": [{"Key": o["Key"]} for o in gone]})
        assert len(r["Deleted"]) == len(gone) and "Errors" not in r, r
s3.delete_bucket(Bucket="sdk")
assert s3.list_buckets()["Buckets"] == []
assert code(lambda: s3.delete_bucket(Bucket="sdk")) == "NoSuchBucket"
print("SDK OK")
