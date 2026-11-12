//! Small bounded tables for the login paths: how often each client
//! address tried something ([`Attempts`]), and the session and token
//! hashes the map service just said it doesn't have ([`Unknown`]). Each
//! keeps at most a fixed number of entries, so memory stays bounded
//! whatever the requests.

use std::collections::HashMap;
use std::net::IpAddr;
use std::sync::Mutex;
use std::time::{Duration, Instant};

/// Failed password logins one client address may make in
/// [`LOGIN_WINDOW`] before mgmt answers 429 itself: a person mistyping
/// stays well under it, and the PAM helper's own limit, shared by every
/// address, isn't what people meet.
pub const LOGIN_FAILURES: u32 = 10;
pub const LOGIN_WINDOW: Duration = Duration::from_secs(300);
/// Logins one client address may begin with the identity provider in
/// [`OIDC_WINDOW`]: each is a row in the map service until it ends.
pub const OIDC_STARTS: u32 = 20;
pub const OIDC_WINDOW: Duration = Duration::from_secs(300);
/// Addresses each table follows at most.
pub const ADDRESSES: usize = 4096;
/// Session and token hashes remembered as unknown, and for how long: a
/// cookie or token that isn't one costs the map service one read per
/// [`UNKNOWN_TTL`], not one per request.
pub const UNKNOWN: usize = 4096;
pub const UNKNOWN_TTL: Duration = Duration::from_secs(30);

/// What an address is counted under: itself, or for IPv6 its /64, which
/// one host usually holds whole.
fn key_of(ip: IpAddr) -> IpAddr {
    match ip {
        IpAddr::V4(_) => ip,
        IpAddr::V6(v6) => match v6.to_ipv4_mapped() {
            Some(v4) => IpAddr::V4(v4),
            None => {
                let s = v6.segments();
                IpAddr::V6(std::net::Ipv6Addr::new(s[0], s[1], s[2], s[3], 0, 0, 0, 0))
            }
        },
    }
}

/// Up to `max` attempts per address in a window that starts with the
/// first; at most `cap` addresses, the one whose window began first
/// forgotten to make room.
pub struct Attempts {
    max: u32,
    window: Duration,
    cap: usize,
    seen: Mutex<HashMap<IpAddr, (u32, Instant)>>,
}

impl Attempts {
    pub fn new(max: u32, window: Duration, cap: usize) -> Attempts {
        Attempts {
            max,
            window,
            cap,
            seen: Mutex::new(HashMap::new()),
        }
    }

    /// How long until `ip` may try again, if it used its attempts.
    pub fn wait(&self, ip: IpAddr, now: Instant) -> Option<Duration> {
        let seen = self.seen.lock().unwrap_or_else(|e| e.into_inner());
        let (n, since) = seen.get(&key_of(ip))?;
        let end = *since + self.window;
        (*n >= self.max && end > now).then(|| end - now)
    }

    /// Counts an attempt by `ip`.
    pub fn note(&self, ip: IpAddr, now: Instant) {
        let mut seen = self.seen.lock().unwrap_or_else(|e| e.into_inner());
        let key = key_of(ip);
        if !seen.contains_key(&key) && seen.len() >= self.cap {
            let window = self.window;
            seen.retain(|_, (_, since)| *since + window > now);
            if seen.len() >= self.cap
                && let Some(oldest) = seen.iter().min_by_key(|(_, (_, s))| *s).map(|(k, _)| *k)
            {
                seen.remove(&oldest);
            }
        }
        let e = seen.entry(key).or_insert((0, now));
        if e.1 + self.window <= now {
            *e = (0, now);
        }
        e.0 += 1;
    }

    #[cfg(test)]
    fn len(&self) -> usize {
        self.seen.lock().unwrap().len()
    }
}

/// Keys known not to be there, for `ttl`; at most `cap` of them.
pub struct Unknown {
    ttl: Duration,
    cap: usize,
    keys: Mutex<HashMap<String, Instant>>,
}

impl Unknown {
    pub fn new(ttl: Duration, cap: usize) -> Unknown {
        Unknown {
            ttl,
            cap,
            keys: Mutex::new(HashMap::new()),
        }
    }

    /// Whether `key` was found missing less than the ttl ago.
    pub fn has(&self, key: &str, now: Instant) -> bool {
        let keys = self.keys.lock().unwrap_or_else(|e| e.into_inner());
        keys.get(key).is_some_and(|at| *at + self.ttl > now)
    }

    /// Remembers `key` as missing.
    pub fn insert(&self, key: &str, now: Instant) {
        let mut keys = self.keys.lock().unwrap_or_else(|e| e.into_inner());
        if !keys.contains_key(key) && keys.len() >= self.cap {
            let ttl = self.ttl;
            keys.retain(|_, at| *at + ttl > now);
            if keys.len() >= self.cap
                && let Some(oldest) = keys
                    .iter()
                    .min_by_key(|(_, at)| **at)
                    .map(|(k, _)| k.clone())
            {
                keys.remove(&oldest);
            }
        }
        keys.insert(key.to_string(), now);
    }

    /// Forgets `key`: it exists now.
    pub fn remove(&self, key: &str) {
        self.keys
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .remove(key);
    }

    #[cfg(test)]
    fn len(&self) -> usize {
        self.keys.lock().unwrap().len()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn attempts_are_counted_per_address_in_a_window() {
        let a = Attempts::new(3, Duration::from_secs(60), 2);
        let t = Instant::now();
        let one: IpAddr = "192.0.2.1".parse().unwrap();
        let two: IpAddr = "192.0.2.2".parse().unwrap();
        for _ in 0..2 {
            a.note(one, t);
        }
        assert_eq!(a.wait(one, t), None);
        a.note(one, t);
        assert_eq!(a.wait(one, t), Some(Duration::from_secs(60)));
        assert_eq!(
            a.wait(one, t + Duration::from_secs(20)),
            Some(Duration::from_secs(40))
        );
        // Another address isn't held back.
        assert_eq!(a.wait(two, t), None);
        // The window over: free again, and counted afresh.
        let later = t + Duration::from_secs(60);
        assert_eq!(a.wait(one, later), None);
        a.note(one, later);
        assert_eq!(a.wait(one, later), None);
        // An IPv6 /64 is one address.
        let v6a: IpAddr = "2001:db8::1".parse().unwrap();
        let v6b: IpAddr = "2001:db8::2:3".parse().unwrap();
        let b = Attempts::new(2, Duration::from_secs(60), 8);
        b.note(v6a, t);
        b.note(v6b, t);
        assert!(b.wait("2001:db8::99".parse().unwrap(), t).is_some());
        assert!(b.wait("2001:db8:0:1::1".parse().unwrap(), t).is_none());
    }

    #[test]
    fn attempts_follow_at_most_cap_addresses() {
        let a = Attempts::new(1, Duration::from_secs(60), 100);
        let t = Instant::now();
        for i in 0..1000u32 {
            a.note(
                IpAddr::from(i.to_be_bytes()),
                t + Duration::from_millis(i.into()),
            );
        }
        assert_eq!(a.len(), 100);
        // The newest are kept.
        assert!(a.wait(IpAddr::from(999u32.to_be_bytes()), t).is_some());
    }

    #[test]
    fn unknown_keys_are_remembered_for_a_while_and_bounded() {
        let u = Unknown::new(Duration::from_secs(30), 3);
        let t = Instant::now();
        u.insert("sess/a", t);
        assert!(u.has("sess/a", t + Duration::from_secs(29)));
        assert!(!u.has("sess/a", t + Duration::from_secs(30)));
        assert!(!u.has("sess/b", t));
        u.remove("sess/a");
        assert!(!u.has("sess/a", t));
        for i in 0..10 {
            u.insert(&format!("k{i}"), t + Duration::from_secs(i));
        }
        assert_eq!(u.len(), 3);
        assert!(u.has("k9", t + Duration::from_secs(9)));
        assert!(!u.has("k0", t + Duration::from_secs(9)));
    }
}
