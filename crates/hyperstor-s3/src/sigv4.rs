//! AWS Signature Version 4, as S3 checks it (14-s3.md §14.2): in the
//! `Authorization` header or in a presigned URL's query, and the
//! signatures of an `aws-chunked` body's chunks
//! ([`crate::chunked`]). Pure functions of the request's method, path,
//! query and headers, so AWS's examples test them.
//!
//! - The canonical path and query are made from the decoded bytes,
//!   encoded again as AWS encodes them, so a client that encodes a
//!   character AWS leaves alone (or the reverse) still matches.
//! - The region is the one the client signed with: it only scopes the
//!   key, and S3 clients disagree on a default.

use crate::dates;
use ring::{digest, hmac};

pub const ALGORITHM: &str = "AWS4-HMAC-SHA256";
/// How far a request's time may be from ours (S3's 15 minutes).
pub const MAX_SKEW: u64 = 15 * 60;
/// The longest a presigned URL may last (S3's 7 days).
pub const MAX_EXPIRES: u64 = 7 * 24 * 3600;
/// The SHA-256 of nothing, in hex.
pub const EMPTY_SHA256: &str = "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855";

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AuthError {
    /// Neither a signed header nor a presigned query.
    Missing,
    Malformed(&'static str),
    UnknownKey,
    Mismatch,
    Skewed,
    Expired,
    Unsupported(&'static str),
}

/// What the body's hash in the signature says.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Payload {
    /// `UNSIGNED-PAYLOAD`: the body isn't hashed.
    Unsigned,
    /// The body's SHA-256, checked once it has all arrived.
    Sha256([u8; 32]),
    /// `STREAMING-AWS4-HMAC-SHA256-PAYLOAD`: an `aws-chunked` body, each
    /// chunk signed.
    Chunked,
    /// `STREAMING-UNSIGNED-PAYLOAD-TRAILER`: an `aws-chunked` body, its
    /// checksum in a trailer.
    ChunkedTrailer,
}

/// A request's signature, as sent.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Signed {
    pub key: String,
    /// `yyyymmdd`, the scope's date.
    pub date: String,
    pub region: String,
    /// `yyyymmddThhmmssZ`.
    pub amz_date: String,
    pub signed_headers: Vec<String>,
    pub signature: String,
    /// Seconds a presigned URL lasts; `None` for a signed header.
    pub expires: Option<u64>,
    pub payload: Payload,
    /// The payload's hash as signed (`UNSIGNED-PAYLOAD`, hex, …).
    payload_text: String,
}

impl Signed {
    pub fn scope(&self) -> String {
        format!("{}/{}/s3/aws4_request", self.date, self.region)
    }
}

/// The request's signature, if it has one: from the `Authorization`
/// header, or else the presigned query.
pub fn parse(query: &str, headers: &http::HeaderMap) -> Result<Signed, AuthError> {
    if let Some(a) = headers.get(http::header::AUTHORIZATION) {
        let a = a
            .to_str()
            .map_err(|_| AuthError::Malformed("authorization"))?;
        return parse_header(a, headers);
    }
    let q = query_pairs(query).ok_or(AuthError::Malformed("query"))?;
    let get = |n: &str| q.iter().find(|(k, _)| k == n.as_bytes()).map(|(_, v)| v);
    let Some(alg) = get("X-Amz-Algorithm") else {
        if get("AWSAccessKeyId").is_some() {
            return Err(AuthError::Unsupported("signature version 2"));
        }
        return Err(AuthError::Missing);
    };
    if alg != ALGORITHM.as_bytes() {
        return Err(AuthError::Unsupported("algorithm"));
    }
    let text = |n: &'static str| {
        get(n)
            .and_then(|v| String::from_utf8(v.clone()).ok())
            .ok_or(AuthError::Malformed(n))
    };
    let (key, date, region) = credential(&text("X-Amz-Credential")?)?;
    let expires: u64 = text("X-Amz-Expires")?
        .parse()
        .map_err(|_| AuthError::Malformed("X-Amz-Expires"))?;
    if expires > MAX_EXPIRES {
        return Err(AuthError::Malformed("X-Amz-Expires"));
    }
    if get("X-Amz-Security-Token").is_some() {
        return Err(AuthError::Unsupported("session tokens"));
    }
    let payload_text = match headers.get("x-amz-content-sha256") {
        Some(v) => v
            .to_str()
            .map_err(|_| AuthError::Malformed("x-amz-content-sha256"))?,
        None => "UNSIGNED-PAYLOAD",
    };
    Ok(Signed {
        key,
        date,
        region,
        amz_date: text("X-Amz-Date")?,
        signed_headers: signed_headers(&text("X-Amz-SignedHeaders")?)?,
        signature: text("X-Amz-Signature")?,
        expires: Some(expires),
        payload: payload(payload_text)?,
        payload_text: payload_text.to_string(),
    })
}

fn parse_header(a: &str, headers: &http::HeaderMap) -> Result<Signed, AuthError> {
    let Some(rest) = a.strip_prefix(ALGORITHM) else {
        return Err(if a.starts_with("AWS ") {
            AuthError::Unsupported("signature version 2")
        } else {
            AuthError::Malformed("authorization")
        });
    };
    let (mut cred, mut signed, mut sig) = (None, None, None);
    for part in rest.split(',') {
        let part = part.trim();
        if let Some(v) = part.strip_prefix("Credential=") {
            cred = Some(v);
        } else if let Some(v) = part.strip_prefix("SignedHeaders=") {
            signed = Some(v);
        } else if let Some(v) = part.strip_prefix("Signature=") {
            sig = Some(v);
        }
    }
    let m = AuthError::Malformed("authorization");
    let (key, date, region) = credential(cred.ok_or(m.clone())?)?;
    let header = |n: &'static str| {
        headers
            .get(n)
            .and_then(|v| v.to_str().ok())
            .ok_or(AuthError::Malformed(n))
    };
    let amz_date = match headers.get("x-amz-date") {
        Some(_) => header("x-amz-date")?.to_string(),
        // RFC 7231's Date, which S3 takes when x-amz-date is absent.
        None => dates::parse_http(header("date")?)
            .map(dates::amz)
            .ok_or(AuthError::Malformed("date"))?,
    };
    let payload_text = header("x-amz-content-sha256")?;
    Ok(Signed {
        key,
        date,
        region,
        amz_date,
        signed_headers: signed_headers(signed.ok_or(m.clone())?)?,
        signature: sig.ok_or(m)?.to_string(),
        expires: None,
        payload: payload(payload_text)?,
        payload_text: payload_text.to_string(),
    })
}

/// `key/date/region/s3/aws4_request`.
fn credential(c: &str) -> Result<(String, String, String), AuthError> {
    let m = AuthError::Malformed("credential");
    let p: Vec<&str> = c.split('/').collect();
    let [key, date, region, "s3", "aws4_request"] = p[..] else {
        return Err(m);
    };
    if key.is_empty() || date.len() != 8 || !date.bytes().all(|b| b.is_ascii_digit()) {
        return Err(m);
    }
    Ok((key.into(), date.into(), region.into()))
}

fn signed_headers(s: &str) -> Result<Vec<String>, AuthError> {
    let h: Vec<String> = s.split(';').map(str::to_string).collect();
    if h.iter()
        .any(|n| n.is_empty() || n.bytes().any(|b| b.is_ascii_uppercase()))
        || !h.iter().any(|n| n == "host")
    {
        return Err(AuthError::Malformed("signed headers"));
    }
    Ok(h)
}

fn payload(p: &str) -> Result<Payload, AuthError> {
    Ok(match p {
        "UNSIGNED-PAYLOAD" => Payload::Unsigned,
        "STREAMING-AWS4-HMAC-SHA256-PAYLOAD" => Payload::Chunked,
        "STREAMING-UNSIGNED-PAYLOAD-TRAILER" => Payload::ChunkedTrailer,
        p if p.starts_with("STREAMING-") => {
            return Err(AuthError::Unsupported("this streaming payload"));
        }
        p => Payload::Sha256(
            unhex(p)
                .and_then(|b| b.try_into().ok())
                .ok_or(AuthError::Malformed("x-amz-content-sha256"))?,
        ),
    })
}

/// The key that signs for `secret` on `date` in `region`.
pub fn signing_key(secret: &str, date: &str, region: &str) -> hmac::Key {
    let k = |key: &[u8], data: &str| {
        hmac::sign(&hmac::Key::new(hmac::HMAC_SHA256, key), data.as_bytes())
    };
    let d = k(format!("AWS4{secret}").as_bytes(), date);
    let r = k(d.as_ref(), region);
    let s = k(r.as_ref(), "s3");
    let a = k(s.as_ref(), "aws4_request");
    hmac::Key::new(hmac::HMAC_SHA256, a.as_ref())
}

/// The canonical request (AWS's term) for `s`.
pub fn canonical_request(
    s: &Signed,
    method: &str,
    path: &str,
    query: &str,
    headers: &http::HeaderMap,
) -> Result<String, AuthError> {
    let path = canonical_path(path).ok_or(AuthError::Malformed("path"))?;
    let query = canonical_query(query, s.expires.is_some()).ok_or(AuthError::Malformed("query"))?;
    let mut out = format!("{method}\n{path}\n{query}\n");
    for name in &s.signed_headers {
        let values: Vec<String> = headers
            .get_all(name.as_str())
            .iter()
            .map(|v| {
                let v = String::from_utf8_lossy(v.as_bytes());
                v.split_whitespace().collect::<Vec<_>>().join(" ")
            })
            .collect();
        if values.is_empty() {
            return Err(AuthError::Malformed("a signed header is missing"));
        }
        out.push_str(&format!("{name}:{}\n", values.join(",")));
    }
    out.push_str(&format!(
        "\n{}\n{}",
        s.signed_headers.join(";"),
        s.payload_text
    ));
    Ok(out)
}

pub fn string_to_sign(s: &Signed, canonical: &str) -> String {
    format!(
        "{ALGORITHM}\n{}\n{}\n{}",
        s.amz_date,
        s.scope(),
        hex(digest::digest(&digest::SHA256, canonical.as_bytes()).as_ref())
    )
}

/// Checks `s` against the request it came with and `secret`, at `now`
/// (Unix seconds). Returns the signing key, for an `aws-chunked` body's
/// chunks.
pub fn verify(
    s: &Signed,
    secret: &str,
    method: &str,
    path: &str,
    query: &str,
    headers: &http::HeaderMap,
    now: u64,
) -> Result<hmac::Key, AuthError> {
    let at = dates::parse_amz(&s.amz_date).ok_or(AuthError::Malformed("date"))?;
    if !s.amz_date.starts_with(&s.date) {
        return Err(AuthError::Malformed("the credential's date"));
    }
    match s.expires {
        None if at.abs_diff(now) > MAX_SKEW => return Err(AuthError::Skewed),
        Some(_) if at > now + MAX_SKEW => return Err(AuthError::Skewed),
        Some(e) if now > at + e => return Err(AuthError::Expired),
        _ => {}
    }
    let key = signing_key(secret, &s.date, &s.region);
    let canonical = canonical_request(s, method, path, query, headers)?;
    let got = unhex(&s.signature).ok_or(AuthError::Mismatch)?;
    // In constant time.
    hmac::verify(&key, string_to_sign(s, &canonical).as_bytes(), &got)
        .map_err(|_| AuthError::Mismatch)?;
    Ok(key)
}

/// The chain of an `aws-chunked` body's signatures: each chunk's signs
/// its bytes and the one before, the first after the request's.
pub struct ChunkSigner {
    key: hmac::Key,
    amz_date: String,
    scope: String,
    prev: String,
}

impl ChunkSigner {
    pub fn new(key: hmac::Key, s: &Signed) -> ChunkSigner {
        ChunkSigner {
            key,
            amz_date: s.amz_date.clone(),
            scope: s.scope(),
            prev: s.signature.clone(),
        }
    }

    fn to_sign(&self, sha256: &[u8]) -> String {
        format!(
            "AWS4-HMAC-SHA256-PAYLOAD\n{}\n{}\n{}\n{EMPTY_SHA256}\n{}",
            self.amz_date,
            self.scope,
            self.prev,
            hex(sha256)
        )
    }

    /// Signs a chunk, given its bytes' SHA-256, and moves on: what a
    /// client does (for tests).
    pub fn sign(&mut self, sha256: &[u8]) -> String {
        let sig = hex(hmac::sign(&self.key, self.to_sign(sha256).as_bytes()).as_ref());
        self.prev = sig.clone();
        sig
    }

    /// Checks a chunk's signature, given its bytes' SHA-256, and moves on.
    pub fn check(&mut self, sha256: &[u8], signature: &str) -> bool {
        let to_sign = self.to_sign(sha256);
        let Some(got) = unhex(signature) else {
            return false;
        };
        if hmac::verify(&self.key, to_sign.as_bytes(), &got).is_err() {
            return false;
        }
        self.prev = hex(&got);
        true
    }
}

pub fn hex(b: &[u8]) -> String {
    b.iter().map(|b| format!("{b:02x}")).collect()
}

pub fn unhex(s: &str) -> Option<Vec<u8>> {
    if !s.len().is_multiple_of(2) {
        return None;
    }
    (0..s.len())
        .step_by(2)
        .map(|i| u8::from_str_radix(s.get(i..i + 2)?, 16).ok())
        .collect()
}

/// `s` with `%XX` decoded; `None` if an escape is bad.
pub fn percent_decode(s: &[u8]) -> Option<Vec<u8>> {
    let mut out = Vec::with_capacity(s.len());
    let mut i = 0;
    while i < s.len() {
        if s[i] == b'%' {
            let h = std::str::from_utf8(s.get(i + 1..i + 3)?).ok()?;
            out.push(u8::from_str_radix(h, 16).ok()?);
            i += 3;
        } else {
            out.push(s[i]);
            i += 1;
        }
    }
    Some(out)
}

/// `b` encoded as AWS encodes: unreserved characters as they are, every
/// other byte `%XX`; `/` kept unless `slash`.
pub fn aws_encode(b: &[u8], slash: bool) -> String {
    let mut out = String::with_capacity(b.len());
    for &c in b {
        if c.is_ascii_alphanumeric()
            || matches!(c, b'-' | b'.' | b'_' | b'~')
            || (c == b'/' && !slash)
        {
            out.push(c as char);
        } else {
            out.push_str(&format!("%{c:02X}"));
        }
    }
    out
}

/// The path, decoded then encoded again (S3 doesn't normalize it).
fn canonical_path(path: &str) -> Option<String> {
    Some(aws_encode(&percent_decode(path.as_bytes())?, false))
}

/// A query's names and values, decoded.
pub type Pairs = Vec<(Vec<u8>, Vec<u8>)>;

/// The query's pairs, decoded, in the order sent; `None` if an escape
/// is bad.
pub fn query_pairs(query: &str) -> Option<Pairs> {
    query
        .split('&')
        .filter(|p| !p.is_empty())
        .map(|p| {
            let (k, v) = p.split_once('=').unwrap_or((p, ""));
            Some((percent_decode(k.as_bytes())?, percent_decode(v.as_bytes())?))
        })
        .collect()
}

/// The query encoded again and sorted; without the signature, presigned.
fn canonical_query(query: &str, presigned: bool) -> Option<String> {
    let mut pairs: Vec<(String, String)> = query_pairs(query)?
        .into_iter()
        .filter(|(k, _)| !(presigned && k == b"X-Amz-Signature"))
        .map(|(k, v)| (aws_encode(&k, true), aws_encode(&v, true)))
        .collect();
    pairs.sort();
    Some(
        pairs
            .iter()
            .map(|(k, v)| format!("{k}={v}"))
            .collect::<Vec<_>>()
            .join("&"),
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::chunked::{Chunked, encode};

    // AWS's published documentation example key, not a real credential;
    // the expected signatures are AWS's (Signature Version 4 examples for
    // S3), or botocore's for the same requests.
    const KEY: &str = "AKIAIOSFODNN7EXAMPLE";
    const SECRET: &str = "wJalrXUtnFEMI/K7MDENG/bPxRfiCYEXAMPLEKEY";
    const AT: u64 = 1_369_353_600;

    fn headers(h: &[(&str, &str)]) -> http::HeaderMap {
        let mut m = http::HeaderMap::new();
        for (k, v) in h {
            m.append(
                http::HeaderName::from_bytes(k.as_bytes()).unwrap(),
                http::HeaderValue::from_str(v).unwrap(),
            );
        }
        m
    }

    fn auth(signed: &str, sig: &str) -> String {
        format!(
            "{ALGORITHM} Credential={KEY}/20130524/us-east-1/s3/aws4_request, SignedHeaders={signed}, Signature={sig}"
        )
    }

    fn check(
        method: &str,
        path: &str,
        query: &str,
        h: &http::HeaderMap,
        now: u64,
    ) -> Result<Signed, AuthError> {
        let s = parse(query, h)?;
        verify(&s, SECRET, method, path, query, h, now)?;
        Ok(s)
    }

    #[test]
    fn signed_headers_match_aws() {
        let get = headers(&[
            ("host", "examplebucket.s3.amazonaws.com"),
            ("range", "bytes=0-9"),
            ("x-amz-content-sha256", EMPTY_SHA256),
            ("x-amz-date", "20130524T000000Z"),
            (
                "authorization",
                &auth(
                    "host;range;x-amz-content-sha256;x-amz-date",
                    "f0e8bdb87c964420e857bd35b5d6ed310bd44f0170aba48dd91039c6036bdb41",
                ),
            ),
        ]);
        let s = check("GET", "/test.txt", "", &get, AT + 60).unwrap();
        assert_eq!(s.key, KEY);
        assert_eq!(
            s.payload,
            Payload::Sha256(unhex(EMPTY_SHA256).unwrap().try_into().unwrap())
        );
        // Anything else: a mismatch, or a time too far off.
        assert_eq!(
            check("GET", "/test.txt", "x", &get, AT),
            Err(AuthError::Mismatch)
        );
        assert_eq!(
            check("HEAD", "/test.txt", "", &get, AT),
            Err(AuthError::Mismatch)
        );
        assert_eq!(
            check("GET", "/test.txt", "", &get, AT + MAX_SKEW + 1),
            Err(AuthError::Skewed)
        );
        let s = parse("", &get).unwrap();
        let r = verify(&s, "wrong", "GET", "/test.txt", "", &get, AT);
        assert!(matches!(r, Err(AuthError::Mismatch)));

        // The query, sorted; the path encoded as AWS does ($ as %24); the
        // Date header for want of x-amz-date.
        let list = headers(&[
            ("host", "examplebucket.s3.amazonaws.com"),
            ("x-amz-content-sha256", EMPTY_SHA256),
            ("x-amz-date", "20130524T000000Z"),
            (
                "authorization",
                &auth(
                    "host;x-amz-content-sha256;x-amz-date",
                    "34b48302e7b5fa45bde8084f4b7868a86f0a534bc59db6670ed5711ef69dc6f7",
                ),
            ),
        ]);
        check("GET", "/", "max-keys=2&prefix=J", &list, AT).unwrap();
        check("GET", "/", "prefix=J&max-keys=2", &list, AT).unwrap();
        let put = headers(&[
            ("date", "Fri, 24 May 2013 00:00:00 -0000"),
            ("host", "examplebucket.s3.amazonaws.com"),
            (
                "x-amz-content-sha256",
                "44ce7dd67c959e0d3524ffac1771dfbba87d2b6b4b4e99e42034a8b803f8b072",
            ),
            ("x-amz-storage-class", "REDUCED_REDUNDANCY"),
            (
                "authorization",
                &auth(
                    "date;host;x-amz-content-sha256;x-amz-storage-class",
                    "537a8c565f27da249b46cd0a32fad1134b73cc392327172d2b3958c4d26c34ca",
                ),
            ),
        ]);
        check("PUT", "/test%24file.text", "", &put, AT).unwrap();
        check("PUT", "/test$file.text", "", &put, AT).unwrap();
        assert_eq!(
            check("PUT", "/test%25file.text", "", &put, AT),
            Err(AuthError::Mismatch)
        );
    }

    #[test]
    fn presigned_urls_match_aws() {
        let q = "X-Amz-Algorithm=AWS4-HMAC-SHA256&X-Amz-Credential=AKIAIOSFODNN7EXAMPLE%2F20130524%2Fus-east-1%2Fs3%2Faws4_request&X-Amz-Date=20130524T000000Z&X-Amz-Expires=86400&X-Amz-SignedHeaders=host&X-Amz-Signature=aeeed9bbccd4d02ee5c0109b86d86835f995330da4c265957d157751f604d404";
        let h = headers(&[("host", "examplebucket.s3.amazonaws.com")]);
        let s = check("GET", "/test.txt", q, &h, AT + 86400).unwrap();
        assert_eq!(s.expires, Some(86400));
        assert_eq!(
            check("GET", "/test.txt", q, &h, AT + 86401),
            Err(AuthError::Expired)
        );
        assert_eq!(
            check("GET", "/test.txt", q, &h, AT - MAX_SKEW - 1),
            Err(AuthError::Skewed)
        );
        assert_eq!(
            check("PUT", "/test.txt", q, &h, AT),
            Err(AuthError::Mismatch)
        );
        assert_eq!(
            check("GET", "/test.txt", "", &h, AT),
            Err(AuthError::Missing)
        );
        assert_eq!(
            check(
                "GET",
                "/test.txt",
                "AWSAccessKeyId=a&Signature=b&Expires=1",
                &h,
                AT
            ),
            Err(AuthError::Unsupported("signature version 2"))
        );
        let long = q.replace("Expires=86400", "Expires=604801");
        assert_eq!(
            check("GET", "/test.txt", &long, &h, AT),
            Err(AuthError::Malformed("X-Amz-Expires"))
        );
    }

    #[test]
    fn chunk_signatures_match_aws() {
        // AWS's example of a signed chunked upload: 65 KiB of 'a' in a
        // 64 KiB chunk and a 1 KiB one.
        let h = headers(&[
            ("host", "s3.amazonaws.com"),
            ("x-amz-content-sha256", "STREAMING-AWS4-HMAC-SHA256-PAYLOAD"),
            ("x-amz-date", "20130524T000000Z"),
            (
                "authorization",
                &auth(
                    "content-encoding;content-length;host;x-amz-content-sha256;x-amz-date;x-amz-decoded-content-length;x-amz-storage-class",
                    "4f232c4386841ef735655705268965c44a0e4690baa4adea153f7db9fa80a0a9",
                ),
            ),
        ]);
        let s = parse("", &h).unwrap();
        assert_eq!(s.payload, Payload::Chunked);
        let key = signing_key(SECRET, "20130524", "us-east-1");
        let mut c = ChunkSigner::new(key.clone(), &s);
        let sha = |b: &[u8]| digest::digest(&digest::SHA256, b).as_ref().to_vec();
        let data = vec![b'a'; 65 * 1024];
        assert_eq!(
            c.sign(&sha(&data[..65536])),
            "ad80c730a21e5b8d04586a2213dd63b9a0e99e0e2307b0ade35a65485a288648"
        );
        assert_eq!(
            c.sign(&sha(&data[65536..])),
            "0055627c9e194cb4542bae2aa5492e3c1575bbb81b612b7d234b86a503ef5497"
        );
        assert_eq!(
            c.sign(&sha(b"")),
            "b6c6ea8a5354eaf15b3cb7646744f4275b71ea724fed81ceb9323e279d449df9"
        );
        // Decoded a byte at a time, and in one go.
        let body = encode(
            &data,
            65536,
            Some(&mut ChunkSigner::new(key.clone(), &s)),
            "",
        );
        for step in [1, 7, body.len()] {
            let mut d = Chunked::new(Some(ChunkSigner::new(key.clone(), &s)));
            let mut out = Vec::new();
            for piece in body.chunks(step) {
                d.feed(piece, &mut out).unwrap();
            }
            assert_eq!(d.finish().unwrap(), []);
            assert_eq!(out, data);
        }
        // A byte changed: the chunk's signature fails.
        let mut bad = body.clone();
        let at = bad.len() - 200;
        bad[at] = b'b';
        let mut d = Chunked::new(Some(ChunkSigner::new(key.clone(), &s)));
        let r = d.feed(&bad, &mut Vec::new());
        assert_eq!(r, Err(crate::chunked::BodyError::Signature));
        // Cut short: incomplete.
        let mut d = Chunked::new(Some(ChunkSigner::new(key, &s)));
        d.feed(&body[..body.len() - 3], &mut Vec::new()).unwrap();
        assert_eq!(d.finish(), Err(crate::chunked::BodyError::Incomplete));
    }

    #[test]
    fn paths_and_queries_encode_as_aws() {
        assert_eq!(aws_encode(b"a b/c+~", false), "a%20b/c%2B~");
        assert_eq!(aws_encode("ü/".as_bytes(), true), "%C3%BC%2F");
        assert_eq!(canonical_path("/a%2Bb/%7e"), Some("/a%2Bb/~".into()));
        assert_eq!(canonical_path("/a%zz"), None);
        assert_eq!(
            canonical_query("b=2&a=&a=1&X-Amz-Signature=x", true),
            Some("a=&a=1&b=2".into())
        );
    }
}
