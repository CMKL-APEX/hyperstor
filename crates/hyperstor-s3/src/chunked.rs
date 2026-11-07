//! A request body's framing and checks (14-s3.md §14.2): the
//! `aws-chunked` encoding that SDKs use to sign a body as it streams, or
//! to send its checksum after it, and the hashes a body is checked
//! against once it has all arrived: the signed SHA-256 and the
//! `x-amz-checksum-*` value, from a header or a trailer. Both run as the
//! bytes arrive, so a body is never held whole.
//!
//! An `aws-chunked` body is chunks, each `hex size[;chunk-signature=sig]`
//! CRLF, the bytes, CRLF; then a chunk of size 0, the trailers if any,
//! and an empty line.

use crate::sigv4::ChunkSigner;
use base64::Engine;
use base64::engine::general_purpose::STANDARD as B64;
use ring::digest;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum BodyError {
    /// Not `aws-chunked` as it should be.
    Malformed(&'static str),
    /// A chunk's signature doesn't match.
    Signature,
    /// The body ended before its last chunk.
    Incomplete,
    /// The signed SHA-256 doesn't match.
    Sha256,
    /// The `x-amz-checksum-*` value doesn't match.
    Checksum(&'static str),
    Unsupported(&'static str),
}

/// The longest size line or trailer taken.
const MAX_LINE: usize = 4096;

enum State {
    Size,
    Data(u64),
    DataEnd,
    Trailers,
    Done,
}

pub struct Chunked {
    signer: Option<ChunkSigner>,
    state: State,
    line: Vec<u8>,
    /// The chunk's signature, as sent.
    sig: String,
    /// The SHA-256 of the chunk's bytes, when chunks are signed.
    sha: Option<digest::Context>,
    trailers: Vec<(String, String)>,
}

impl Chunked {
    /// A decoder; `signer` checks each chunk's signature.
    pub fn new(signer: Option<ChunkSigner>) -> Chunked {
        Chunked {
            signer,
            state: State::Size,
            line: Vec::new(),
            sig: String::new(),
            sha: None,
            trailers: Vec::new(),
        }
    }

    /// Decodes `input`, the body's bytes as they arrive, appending what
    /// they carry to `out`.
    pub fn feed(&mut self, mut input: &[u8], out: &mut Vec<u8>) -> Result<(), BodyError> {
        while !input.is_empty() {
            match self.state {
                State::Size => {
                    let Some(line) = self.take_line(&mut input)? else {
                        continue;
                    };
                    let line =
                        std::str::from_utf8(&line).map_err(|_| BodyError::Malformed("size"))?;
                    let (size, ext) = line.split_once(';').unwrap_or((line, ""));
                    let size = u64::from_str_radix(size.trim(), 16)
                        .map_err(|_| BodyError::Malformed("size"))?;
                    if self.signer.is_some() {
                        self.sig = ext
                            .strip_prefix("chunk-signature=")
                            .ok_or(BodyError::Malformed("chunk-signature"))?
                            .to_string();
                        self.sha = Some(digest::Context::new(&digest::SHA256));
                    }
                    if size == 0 {
                        self.end_chunk()?;
                        self.state = State::Trailers;
                    } else {
                        self.state = State::Data(size);
                    }
                }
                State::Data(n) => {
                    let k = (n as usize).min(input.len());
                    out.extend_from_slice(&input[..k]);
                    if let Some(s) = &mut self.sha {
                        s.update(&input[..k]);
                    }
                    input = &input[k..];
                    let left = n - k as u64;
                    self.state = if left == 0 {
                        State::DataEnd
                    } else {
                        State::Data(left)
                    };
                }
                State::DataEnd => {
                    let Some(line) = self.take_line(&mut input)? else {
                        continue;
                    };
                    if !line.is_empty() {
                        return Err(BodyError::Malformed("a chunk's end"));
                    }
                    self.end_chunk()?;
                    self.state = State::Size;
                }
                State::Trailers => {
                    let Some(line) = self.take_line(&mut input)? else {
                        continue;
                    };
                    if line.is_empty() {
                        self.state = State::Done;
                        continue;
                    }
                    let line =
                        std::str::from_utf8(&line).map_err(|_| BodyError::Malformed("trailer"))?;
                    let (k, v) = line
                        .split_once(':')
                        .ok_or(BodyError::Malformed("trailer"))?;
                    if k.trim().eq_ignore_ascii_case("x-amz-trailer-signature") {
                        return Err(BodyError::Unsupported("signed trailers"));
                    }
                    self.trailers
                        .push((k.trim().to_ascii_lowercase(), v.trim().to_string()));
                }
                State::Done => return Err(BodyError::Malformed("bytes after the last chunk")),
            }
        }
        Ok(())
    }

    /// The trailers, once the body has ended where it should.
    pub fn finish(self) -> Result<Vec<(String, String)>, BodyError> {
        match self.state {
            State::Done => Ok(self.trailers),
            _ => Err(BodyError::Incomplete),
        }
    }

    fn end_chunk(&mut self) -> Result<(), BodyError> {
        if let Some(s) = &mut self.signer {
            let h = self.sha.take().map(|c| c.finish());
            let h = h.as_ref().map_or(&[][..], |h| h.as_ref());
            if !s.check(h, &self.sig) {
                return Err(BodyError::Signature);
            }
        }
        Ok(())
    }

    /// A whole line from `input` (without its CRLF), or `None` if it
    /// hasn't all arrived; what it took is gone from `input`.
    fn take_line(&mut self, input: &mut &[u8]) -> Result<Option<Vec<u8>>, BodyError> {
        let end = match input.iter().position(|&b| b == b'\n') {
            Some(i) => i + 1,
            None => input.len(),
        };
        self.line.extend_from_slice(&input[..end]);
        *input = &input[end..];
        if self.line.len() > MAX_LINE + 2 {
            return Err(BodyError::Malformed("a line too long"));
        }
        if !self.line.ends_with(b"\n") {
            return Ok(None);
        }
        let mut l = std::mem::take(&mut self.line);
        l.pop();
        if l.pop() != Some(b'\r') {
            return Err(BodyError::Malformed("a line not ended by CRLF"));
        }
        Ok(Some(l))
    }
}

/// An `x-amz-checksum-*` algorithm.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Algorithm {
    Crc32,
    Crc32c,
    Sha1,
    Sha256,
}

impl Algorithm {
    /// From a header's name: `x-amz-checksum-crc32` and so on.
    pub fn of_header(name: &str) -> Result<Option<Algorithm>, BodyError> {
        let Some(a) = name.strip_prefix("x-amz-checksum-") else {
            return Ok(None);
        };
        Ok(Some(match a {
            "crc32" => Algorithm::Crc32,
            "crc32c" => Algorithm::Crc32c,
            "sha1" => Algorithm::Sha1,
            "sha256" => Algorithm::Sha256,
            // x-amz-checksum-type and -algorithm aren't values.
            "type" | "algorithm" => return Ok(None),
            _ => return Err(BodyError::Unsupported("this checksum algorithm")),
        }))
    }

    pub fn name(self) -> &'static str {
        match self {
            Algorithm::Crc32 => "CRC32",
            Algorithm::Crc32c => "CRC32C",
            Algorithm::Sha1 => "SHA1",
            Algorithm::Sha256 => "SHA256",
        }
    }

    /// Its header: `x-amz-checksum-crc32` and so on.
    pub fn header(self) -> String {
        format!("x-amz-checksum-{}", self.name().to_ascii_lowercase())
    }

    /// Its number in an index entry (14-s3.md §14.6).
    pub fn code(self) -> u8 {
        match self {
            Algorithm::Crc32 => 1,
            Algorithm::Crc32c => 2,
            Algorithm::Sha1 => 3,
            Algorithm::Sha256 => 4,
        }
    }

    pub fn of_code(c: u8) -> Option<Algorithm> {
        Some(match c {
            1 => Algorithm::Crc32,
            2 => Algorithm::Crc32c,
            3 => Algorithm::Sha1,
            4 => Algorithm::Sha256,
            _ => return None,
        })
    }
}

enum Running {
    Crc32(crc32fast::Hasher),
    Crc32c(u32),
    Digest(Box<digest::Context>),
}

/// The hashes a body is checked against, kept as it streams.
pub struct Checks {
    sha256: Option<([u8; 32], digest::Context)>,
    checksum: Option<(Algorithm, Option<Vec<u8>>, Running)>,
}

impl Checks {
    /// `sha256`: the signed hash. `checksum`: the algorithm and, if a
    /// header gave it, the value (base64); else a trailer will.
    pub fn new(
        sha256: Option<[u8; 32]>,
        checksum: Option<(Algorithm, Option<&str>)>,
    ) -> Result<Checks, BodyError> {
        let checksum = match checksum {
            None => None,
            Some((a, v)) => {
                let want = match v {
                    Some(v) => Some(
                        B64.decode(v)
                            .map_err(|_| BodyError::Malformed("checksum"))?,
                    ),
                    None => None,
                };
                let r = match a {
                    Algorithm::Crc32 => Running::Crc32(crc32fast::Hasher::new()),
                    Algorithm::Crc32c => Running::Crc32c(0),
                    Algorithm::Sha1 => Running::Digest(Box::new(digest::Context::new(
                        &digest::SHA1_FOR_LEGACY_USE_ONLY,
                    ))),
                    Algorithm::Sha256 => {
                        Running::Digest(Box::new(digest::Context::new(&digest::SHA256)))
                    }
                };
                Some((a, want, r))
            }
        };
        Ok(Checks {
            sha256: sha256.map(|h| (h, digest::Context::new(&digest::SHA256))),
            checksum,
        })
    }

    pub fn update(&mut self, b: &[u8]) {
        if let Some((_, c)) = &mut self.sha256 {
            c.update(b);
        }
        match &mut self.checksum {
            Some((_, _, Running::Crc32(h))) => h.update(b),
            Some((_, _, Running::Crc32c(c))) => *c = crc32c::crc32c_append(*c, b),
            Some((_, _, Running::Digest(c))) => c.update(b),
            None => {}
        }
    }

    /// Checks the body, all of it seen, given its trailers. Returns the
    /// checksum it was checked against, if any, to keep with the object.
    pub fn finish(
        self,
        trailers: &[(String, String)],
    ) -> Result<Option<(Algorithm, Vec<u8>)>, BodyError> {
        if let Some((want, c)) = self.sha256
            && c.finish().as_ref() != want
        {
            return Err(BodyError::Sha256);
        }
        let Some((a, want, r)) = self.checksum else {
            return Ok(None);
        };
        let want = match want {
            Some(w) => w,
            None => {
                let name = a.header();
                let v = trailers
                    .iter()
                    .find(|(k, _)| *k == name)
                    .ok_or(BodyError::Malformed("the checksum trailer is missing"))?;
                B64.decode(&v.1)
                    .map_err(|_| BodyError::Malformed("checksum"))?
            }
        };
        let got = match r {
            Running::Crc32(h) => h.finalize().to_be_bytes().to_vec(),
            Running::Crc32c(c) => c.to_be_bytes().to_vec(),
            Running::Digest(c) => c.finish().as_ref().to_vec(),
        };
        if got != want {
            return Err(BodyError::Checksum(a.name()));
        }
        Ok(Some((a, got)))
    }
}

/// For tests: `data` as an `aws-chunked` body in chunks of `size`,
/// signed by `signer` if given, then `trailers`.
pub fn encode(
    data: &[u8],
    size: usize,
    mut signer: Option<&mut ChunkSigner>,
    trailers: &str,
) -> Vec<u8> {
    let mut out = Vec::new();
    let mut pieces: Vec<&[u8]> = data.chunks(size).collect();
    pieces.push(&[]);
    for p in pieces {
        out.extend_from_slice(format!("{:x}", p.len()).as_bytes());
        if let Some(s) = signer.as_deref_mut() {
            let sig = s.sign(digest::digest(&digest::SHA256, p).as_ref());
            out.extend_from_slice(format!(";chunk-signature={sig}").as_bytes());
        }
        out.extend_from_slice(b"\r\n");
        if !p.is_empty() {
            out.extend_from_slice(p);
            out.extend_from_slice(b"\r\n");
        }
    }
    out.extend_from_slice(trailers.as_bytes());
    out.extend_from_slice(b"\r\n");
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn unsigned_bodies_carry_their_checksum_after() {
        let data: Vec<u8> = (0..100_000u32).map(|i| (i * 7) as u8).collect();
        let crc = crc32fast::hash(&data).to_be_bytes();
        let trailer = format!("x-amz-checksum-crc32:{}\r\n", B64.encode(crc));
        let body = encode(&data, 8192, None, &trailer);
        for step in [1, 5, 8194, body.len()] {
            let mut d = Chunked::new(None);
            let mut c = Checks::new(None, Some((Algorithm::Crc32, None))).unwrap();
            let mut out = Vec::new();
            for piece in body.chunks(step) {
                d.feed(piece, &mut out).unwrap();
            }
            c.update(&out);
            let t = d.finish().unwrap();
            c.finish(&t).unwrap();
            assert_eq!(out, data);
        }
        // Another body's checksum; none; one in a header.
        let mut c = Checks::new(None, Some((Algorithm::Crc32, None))).unwrap();
        c.update(b"x");
        let t = [("x-amz-checksum-crc32".to_string(), B64.encode(crc))];
        assert_eq!(c.finish(&t), Err(BodyError::Checksum("CRC32")));
        let c = Checks::new(None, Some((Algorithm::Crc32c, None))).unwrap();
        assert!(matches!(c.finish(&t), Err(BodyError::Malformed(_))));
        let want = B64.encode(crc32c::crc32c(&data).to_be_bytes());
        let mut c = Checks::new(None, Some((Algorithm::Crc32c, Some(&want)))).unwrap();
        c.update(&data);
        c.finish(&[]).unwrap();
        let sha: [u8; 32] = digest::digest(&digest::SHA256, &data)
            .as_ref()
            .try_into()
            .unwrap();
        for (a, v) in [
            (Algorithm::Sha256, sha.to_vec()),
            (
                Algorithm::Sha1,
                digest::digest(&digest::SHA1_FOR_LEGACY_USE_ONLY, &data)
                    .as_ref()
                    .to_vec(),
            ),
        ] {
            let want = B64.encode(v);
            let mut c = Checks::new(Some(sha), Some((a, Some(&want)))).unwrap();
            c.update(&data);
            c.finish(&[]).unwrap();
        }
        let mut c = Checks::new(Some(sha), None).unwrap();
        c.update(b"y");
        assert_eq!(c.finish(&[]), Err(BodyError::Sha256));
        // Framing errors.
        for bad in [
            &b"zz\r\n"[..],
            b"1\r\nab\r\n",
            b"1\nx\r\n",
            b"0\r\n\r\nmore",
        ] {
            let mut d = Chunked::new(None);
            assert!(d.feed(bad, &mut Vec::new()).is_err(), "{bad:?}");
        }
        assert_eq!(
            Algorithm::of_header("x-amz-checksum-crc64nvme"),
            Err(BodyError::Unsupported("this checksum algorithm"))
        );
        assert_eq!(Algorithm::of_header("x-amz-checksum-type"), Ok(None));
    }
}
