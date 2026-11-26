//! Sealing keys and dataset capabilities (spec/29-hsfs.md §32.6,
//! 04-decisions.md D363; the key is D133's).
//!
//! The map service keeps a few **sealing keys**, each with an id of one
//! byte, the newest two valid at once so a rotation invalidates nothing
//! at once. Whoever grants access (a metadata server at mount, the NFS
//! server at open) seals a small statement under the newest key; whoever
//! serves the data (a reactor) checks it with no lookup.
//!
//! A **capability** says: this client instance, on this host, may read
//! (or read and write) this branch of this lineage, for this tenant,
//! until this time. Sealed, it is its fields and an HMAC-SHA256 over
//! them:
//!
//! ```text
//! 1   version (1)
//! 1   key id
//! 1   access (1 read, 2 read and write)
//! 4   tenant
//! 16  lineage
//! 4   branch
//! 8   client instance
//! 8   expiry, seconds since the epoch
//! 2+n host name
//! 32  HMAC-SHA256 of all the above, under the key of that id
//! ```
//!
//! Integers are little-endian. The MAC is whole: unlike an NFS stateid
//! (D133), a capability has no size to fit.

use ring::hmac;

/// The sealed form's version.
const VERSION: u8 = 1;
/// What a MAC is taken over before the capability's bytes, so a sealing
/// key's MAC of anything else (a stateid, D133) is never a capability's.
const DOMAIN: &[u8] = b"hyperstor capability v1\0";
const MAC_LEN: usize = 32;
/// A host name's longest, as a DNS name's.
pub const HOST_MAX: usize = 253;

/// One sealing key: 32 random bytes and the id sealed statements name it
/// by.
#[derive(Clone)]
pub struct SealingKey {
    pub id: u8,
    key: [u8; 32],
}

impl SealingKey {
    pub fn new(id: u8, key: [u8; 32]) -> SealingKey {
        SealingKey { id, key }
    }

    /// A new random key with `id`.
    pub fn generate(id: u8) -> Result<SealingKey, String> {
        let rng = ring::rand::SystemRandom::new();
        let key: [u8; 32] = ring::rand::generate(&rng)
            .map_err(|e| e.to_string())?
            .expose();
        Ok(SealingKey { id, key })
    }

    /// The key's bytes, for the map service to seal under its cluster
    /// key and to hand to a storage node over mutual TLS.
    pub fn bytes(&self) -> &[u8; 32] {
        &self.key
    }

    fn mac(&self, body: &[u8]) -> hmac::Tag {
        let k = hmac::Key::new(hmac::HMAC_SHA256, &self.key);
        let mut c = hmac::Context::with_key(&k);
        c.update(DOMAIN);
        c.update(body);
        c.sign()
    }
}

impl Drop for SealingKey {
    fn drop(&mut self) {
        // Best effort: don't leave the key in freed memory.
        self.key.fill(0);
        std::hint::black_box(&self.key);
    }
}

impl std::fmt::Debug for SealingKey {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "SealingKey({}, ..)", self.id)
    }
}

/// What a capability lets its holder do.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum Access {
    Read = 1,
    ReadWrite = 2,
}

impl Access {
    /// Whether a holder of `self` may do what needs `need`.
    pub fn allows(self, need: Access) -> bool {
        self >= need
    }
}

/// A grant of access to one branch of one lineage (D363).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Capability {
    pub tenant: u32,
    pub lineage: u128,
    pub branch: u32,
    pub access: Access,
    /// The client's instance, as it gives in `Hello`: what the blocklist
    /// names, so revoking the client revokes this.
    pub instance: u64,
    /// The host the client runs on, as the granter knew it.
    pub host: String,
    /// After this second, the capability is refused.
    pub expires_unix: u64,
}

/// Why a sealed capability was refused. Nothing in it comes from the
/// sealed bytes unless their MAC was right.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CapabilityError {
    /// Not a capability of a version this build reads.
    Malformed,
    /// Sealed under a key this holder doesn't have: rotated away, or
    /// another cluster's.
    UnknownKey(u8),
    /// The MAC doesn't match: forged, damaged, or another cluster's key
    /// with the same id.
    BadMac,
    Expired {
        expires_unix: u64,
    },
    /// Presented by another client instance than it was granted to.
    WrongInstance,
    /// Presented from another host than it was granted to.
    WrongHost,
}

impl std::fmt::Display for CapabilityError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            CapabilityError::Malformed => f.write_str("not a capability"),
            CapabilityError::UnknownKey(id) => write!(f, "sealed under unknown key {id}"),
            CapabilityError::BadMac => f.write_str("the capability's seal doesn't match"),
            CapabilityError::Expired { expires_unix } => {
                write!(f, "the capability expired at {expires_unix}")
            }
            CapabilityError::WrongInstance => {
                f.write_str("the capability was granted to another client")
            }
            CapabilityError::WrongHost => f.write_str("the capability was granted to another host"),
        }
    }
}

impl std::error::Error for CapabilityError {}

/// How much of its HMAC-SHA256 a ping carries (D379): 128 bits, which a
/// forger gets one try at per nonce.
pub const PING_MAC_LEN: usize = 16;
/// A ping's MAC.
pub type PingMac = [u8; PING_MAC_LEN];

/// The sealing keys a process holds: the newest seals, any opens.
#[derive(Debug, Clone, Default)]
pub struct SealingKeys {
    /// Oldest first.
    keys: Vec<SealingKey>,
}

impl SealingKeys {
    /// From keys in the order they were made, oldest first.
    pub fn new(keys: Vec<SealingKey>) -> SealingKeys {
        SealingKeys { keys }
    }

    pub fn is_empty(&self) -> bool {
        self.keys.is_empty()
    }

    /// The ids held, oldest first.
    pub fn ids(&self) -> Vec<u8> {
        self.keys.iter().map(|k| k.id).collect()
    }

    /// The id new statements are sealed under.
    pub fn current(&self) -> Option<u8> {
        self.keys.last().map(|k| k.id)
    }

    /// The MAC of a liveness ping or its answer (D379; 30-data-path-tls.md
    /// §33.7) under key `id`: over which of the two it is (`kind`), the
    /// ping's nonce, and the node pinged, whose answer it is. None
    /// without that key.
    ///
    /// Why the node: an answer a live node gave can't then be shown for a
    /// dead one. Why the label: the keys also seal capabilities, whose
    /// bodies never start with it.
    pub fn ping_mac(&self, id: u8, kind: &[u8; 4], nonce: u64, node: u32) -> Option<PingMac> {
        let key = self.keys.iter().find(|k| k.id == id)?;
        let k = hmac::Key::new(hmac::HMAC_SHA256, &key.key);
        let mut c = hmac::Context::with_key(&k);
        c.update(b"hyperstor ping 1\0");
        c.update(kind);
        c.update(&nonce.to_le_bytes());
        c.update(&node.to_le_bytes());
        let tag = c.sign();
        let mut out = [0u8; PING_MAC_LEN];
        out.copy_from_slice(&tag.as_ref()[..PING_MAC_LEN]);
        Some(out)
    }

    /// Whether `mac` is [`Self::ping_mac`] of the rest, compared in
    /// constant time.
    pub fn ping_verify(&self, id: u8, kind: &[u8; 4], nonce: u64, node: u32, mac: &[u8]) -> bool {
        let Some(want) = self.ping_mac(id, kind, nonce, node) else {
            return false;
        };
        mac.len() == PING_MAC_LEN && want.iter().zip(mac).fold(0u8, |d, (a, b)| d | (a ^ b)) == 0
    }

    /// `cap` sealed under the newest key; None with no key, or a host
    /// name too long to be one.
    pub fn seal(&self, cap: &Capability) -> Option<Vec<u8>> {
        let key = self.keys.last()?;
        if cap.host.len() > HOST_MAX {
            return None;
        }
        let mut out = Vec::with_capacity(45 + cap.host.len() + MAC_LEN);
        out.push(VERSION);
        out.push(key.id);
        out.push(cap.access as u8);
        out.extend_from_slice(&cap.tenant.to_le_bytes());
        out.extend_from_slice(&cap.lineage.to_le_bytes());
        out.extend_from_slice(&cap.branch.to_le_bytes());
        out.extend_from_slice(&cap.instance.to_le_bytes());
        out.extend_from_slice(&cap.expires_unix.to_le_bytes());
        out.extend_from_slice(&(cap.host.len() as u16).to_le_bytes());
        out.extend_from_slice(cap.host.as_bytes());
        let tag = key.mac(&out);
        out.extend_from_slice(tag.as_ref());
        Some(out)
    }

    /// The capability in `sealed`, if a key held here sealed it, it hasn't
    /// expired at `now_unix`, and it was granted to `instance` on `host`.
    /// The MAC is checked before anything in it is believed.
    pub fn open(
        &self,
        sealed: &[u8],
        now_unix: u64,
        instance: u64,
        host: &str,
    ) -> Result<Capability, CapabilityError> {
        use CapabilityError::Malformed;
        let (body, tag) = sealed
            .len()
            .checked_sub(MAC_LEN)
            .map(|n| sealed.split_at(n))
            .ok_or(Malformed)?;
        if body.len() < 2 || body[0] != VERSION {
            return Err(Malformed);
        }
        let id = body[1];
        let key = self
            .keys
            .iter()
            .rev()
            .find(|k| k.id == id)
            .ok_or(CapabilityError::UnknownKey(id))?;
        // Constant-time, by ring.
        let k = hmac::Key::new(hmac::HMAC_SHA256, &key.key);
        let mut signed = Vec::with_capacity(DOMAIN.len() + body.len());
        signed.extend_from_slice(DOMAIN);
        signed.extend_from_slice(body);
        hmac::verify(&k, &signed, tag).map_err(|_| CapabilityError::BadMac)?;

        let mut r = Reader(&body[2..]);
        let access = match r.take::<1>().ok_or(Malformed)?[0] {
            1 => Access::Read,
            2 => Access::ReadWrite,
            _ => return Err(Malformed),
        };
        let tenant = u32::from_le_bytes(r.take().ok_or(Malformed)?);
        let lineage = u128::from_le_bytes(r.take().ok_or(Malformed)?);
        let branch = u32::from_le_bytes(r.take().ok_or(Malformed)?);
        let cap_instance = u64::from_le_bytes(r.take().ok_or(Malformed)?);
        let expires_unix = u64::from_le_bytes(r.take().ok_or(Malformed)?);
        let n = usize::from(u16::from_le_bytes(r.take().ok_or(Malformed)?));
        if r.0.len() != n || n > HOST_MAX {
            return Err(Malformed);
        }
        let cap_host = std::str::from_utf8(r.0).map_err(|_| Malformed)?;
        if now_unix > expires_unix {
            return Err(CapabilityError::Expired { expires_unix });
        }
        if cap_instance != instance {
            return Err(CapabilityError::WrongInstance);
        }
        if cap_host != host {
            return Err(CapabilityError::WrongHost);
        }
        Ok(Capability {
            tenant,
            lineage,
            branch,
            access,
            instance: cap_instance,
            host: cap_host.to_string(),
            expires_unix,
        })
    }
}

struct Reader<'a>(&'a [u8]);

impl Reader<'_> {
    fn take<const N: usize>(&mut self) -> Option<[u8; N]> {
        let (a, rest) = self.0.split_at_checked(N)?;
        self.0 = rest;
        a.try_into().ok()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// D379: a ping's MAC is of its kind, nonce and node, under a key
    /// held; the key before still verifies, another cluster's doesn't.
    #[test]
    fn a_pings_mac_is_of_its_kind_nonce_and_node() {
        let keys = SealingKeys::new(vec![
            SealingKey::new(1, [7; 32]),
            SealingKey::new(2, [9; 32]),
        ]);
        let (ping, pong) = (b"HSK?", b"HSK!");
        let mac = keys.ping_mac(2, ping, 42, 3).unwrap();
        assert!(keys.ping_verify(2, ping, 42, 3, &mac));
        // Not as an answer, for another nonce or node, or under the
        // other key; nor cut short or changed.
        assert!(!keys.ping_verify(2, pong, 42, 3, &mac));
        assert!(!keys.ping_verify(2, ping, 43, 3, &mac));
        assert!(!keys.ping_verify(2, ping, 42, 4, &mac));
        assert!(!keys.ping_verify(1, ping, 42, 3, &mac));
        assert!(!keys.ping_verify(2, ping, 42, 3, &mac[..15]));
        let mut bad = mac;
        bad[0] ^= 1;
        assert!(!keys.ping_verify(2, ping, 42, 3, &bad));
        // The key before the newest still makes and verifies them.
        let old = keys.ping_mac(1, ping, 42, 3).unwrap();
        assert!(old != mac && keys.ping_verify(1, ping, 42, 3, &old));
        // A key not held, and another cluster's under the same id.
        assert!(keys.ping_mac(3, ping, 42, 3).is_none());
        let other = SealingKeys::new(vec![SealingKey::new(2, [8; 32])]);
        assert!(!other.ping_verify(2, ping, 42, 3, &mac));
        assert!(!SealingKeys::default().ping_verify(2, ping, 42, 3, &mac));
    }

    fn cap() -> Capability {
        Capability {
            tenant: 7,
            lineage: 0x0123_4567_89ab_cdef_0011_2233_4455_6677,
            branch: 3,
            access: Access::Read,
            instance: 0xfeed_beef,
            host: "gpu-12".into(),
            expires_unix: 2_000,
        }
    }

    fn keys(ids: &[u8]) -> SealingKeys {
        SealingKeys::new(
            ids.iter()
                .map(|&id| SealingKey::new(id, [id.wrapping_mul(31).wrapping_add(1); 32]))
                .collect(),
        )
    }

    #[test]
    fn a_capability_opens_only_as_it_was_granted() {
        let k = keys(&[1]);
        let c = cap();
        let sealed = k.seal(&c).unwrap();
        assert_eq!(k.open(&sealed, 1_000, c.instance, &c.host), Ok(c.clone()));
        // At its last second, then past it.
        assert!(k.open(&sealed, 2_000, c.instance, &c.host).is_ok());
        assert_eq!(
            k.open(&sealed, 2_001, c.instance, &c.host),
            Err(CapabilityError::Expired {
                expires_unix: 2_000
            })
        );
        assert_eq!(
            k.open(&sealed, 1_000, c.instance + 1, &c.host),
            Err(CapabilityError::WrongInstance)
        );
        assert_eq!(
            k.open(&sealed, 1_000, c.instance, "gpu-13"),
            Err(CapabilityError::WrongHost)
        );
        assert!(Access::ReadWrite.allows(Access::Read) && Access::Read.allows(Access::Read));
        assert!(!Access::Read.allows(Access::ReadWrite));
    }

    /// Every single-bit change, every truncation and every extension is
    /// refused, and never as anything but a bad seal or a bad shape: no
    /// field of a forged capability is believed.
    #[test]
    fn any_change_to_a_sealed_capability_is_refused() {
        let k = keys(&[1]);
        let c = cap();
        let sealed = k.seal(&c).unwrap();
        for i in 0..sealed.len() {
            for bit in 0..8 {
                let mut bad = sealed.clone();
                bad[i] ^= 1 << bit;
                let r = k.open(&bad, 1_000, c.instance, &c.host);
                assert!(
                    matches!(
                        r,
                        Err(CapabilityError::BadMac
                            | CapabilityError::Malformed
                            | CapabilityError::UnknownKey(_))
                    ),
                    "byte {i} bit {bit}: {r:?}"
                );
            }
        }
        for n in 0..sealed.len() {
            assert!(k.open(&sealed[..n], 1_000, c.instance, &c.host).is_err());
        }
        let mut long = sealed.clone();
        long.push(0);
        assert!(k.open(&long, 1_000, c.instance, &c.host).is_err());
        // Read can't be turned into read and write, whatever else changes.
        let mut rw = sealed.clone();
        rw[2] = Access::ReadWrite as u8;
        assert_eq!(
            k.open(&rw, 1_000, c.instance, &c.host),
            Err(CapabilityError::BadMac)
        );
    }

    #[test]
    fn two_keys_are_valid_through_a_rotation() {
        let c = cap();
        let old = keys(&[1]);
        let sealed_old = old.seal(&c).unwrap();
        // After a rotation: 2 seals, 1 still opens.
        let both = keys(&[1, 2]);
        assert_eq!((both.current(), both.ids()), (Some(2), vec![1, 2]));
        assert!(both.open(&sealed_old, 1_000, c.instance, &c.host).is_ok());
        let sealed_new = both.seal(&c).unwrap();
        assert_eq!(sealed_new[1], 2);
        // A holder that hasn't heard of 2 yet says so, rather than "forged".
        assert_eq!(
            old.open(&sealed_new, 1_000, c.instance, &c.host),
            Err(CapabilityError::UnknownKey(2))
        );
        // After the next: 1 is gone.
        let later = keys(&[2, 3]);
        assert_eq!(
            later.open(&sealed_old, 1_000, c.instance, &c.host),
            Err(CapabilityError::UnknownKey(1))
        );
        assert!(later.open(&sealed_new, 1_000, c.instance, &c.host).is_ok());
        // Another cluster's key with the same id.
        let other = SealingKeys::new(vec![SealingKey::new(2, [9; 32])]);
        assert_eq!(
            other.open(&sealed_new, 1_000, c.instance, &c.host),
            Err(CapabilityError::BadMac)
        );
    }

    #[test]
    fn nothing_seals_without_a_key_or_with_a_name_too_long() {
        assert!(SealingKeys::default().seal(&cap()).is_none());
        assert!(SealingKeys::default().is_empty());
        let mut c = cap();
        c.host = "h".repeat(HOST_MAX + 1);
        assert!(keys(&[1]).seal(&c).is_none());
        c.host = "h".repeat(HOST_MAX);
        let k = keys(&[1]);
        let sealed = k.seal(&c).unwrap();
        assert_eq!(k.open(&sealed, 0, c.instance, &c.host), Ok(c));
        // An empty host name is a name too: a client that gave none.
        let mut c = cap();
        c.host.clear();
        let sealed = k.seal(&c).unwrap();
        assert!(k.open(&sealed, 0, c.instance, "").is_ok());
        assert_eq!(
            k.open(&sealed, 0, c.instance, "x"),
            Err(CapabilityError::WrongHost)
        );
        assert_eq!(
            format!("{:?}", SealingKey::new(4, [1; 32])),
            "SealingKey(4, ..)"
        );
        let a = SealingKey::generate(1).unwrap();
        let b = SealingKey::generate(1).unwrap();
        assert_ne!(a.bytes(), b.bytes());
    }
}
