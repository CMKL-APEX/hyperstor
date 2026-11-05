"""One step of the S3 key and placement test (tests/s3.rs), with AWS's
Python SDK against a gateway that reads its keys from the map service.

    s3_keys.py ENDPOINT CA STEP [ARGS...]

The key is AWS_ACCESS_KEY_ID and AWS_SECRET_ACCESS_KEY from the
environment, never the command line. Steps:

- create BUCKET [PLACEMENT]: CreateBucket, with PLACEMENT as its
  LocationConstraint if given; prints "created" or the error code.
- roundtrip BUCKET: puts an object, reads it back and lists it.
- denied: ListBuckets; prints the error code it gets, or "allowed".
"""

import sys

import boto3
import botocore.config
import botocore.exceptions


def client(endpoint, ca):
    return boto3.client(
        "s3",
        endpoint_url=endpoint,
        region_name="us-east-1",
        verify=ca,
        config=botocore.config.Config(
            signature_version="s3v4",
            s3={"addressing_style": "path"},
            retries={"max_attempts": 1},
        ),
    )


def code(e):
    return e.response.get("Error", {}).get("Code", "?")


def main():
    endpoint, ca, step, *args = sys.argv[1:]
    s3 = client(endpoint, ca)
    if step == "create":
        kw = {"Bucket": args[0]}
        if len(args) > 1:
            kw["CreateBucketConfiguration"] = {"LocationConstraint": args[1]}
        try:
            s3.create_bucket(**kw)
            print("created")
        except botocore.exceptions.ClientError as e:
            print(code(e))
    elif step == "roundtrip":
        body = b"placed by the map service\n" * 100
        s3.put_object(Bucket=args[0], Key="k", Body=body)
        got = s3.get_object(Bucket=args[0], Key="k")["Body"].read()
        assert got == body, "the object read back differs"
        keys = [o["Key"] for o in s3.list_objects_v2(Bucket=args[0]).get("Contents", [])]
        assert keys == ["k"], keys
        print("roundtrip ok")
    elif step == "denied":
        try:
            s3.list_buckets()
            print("allowed")
        except botocore.exceptions.ClientError as e:
            print(code(e))
    else:
        sys.exit(f"unknown step {step}")


main()
