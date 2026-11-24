//! The sealing keys a storage node checks sealed capabilities with
//! (29-hsfs.md §32.6, 22-nfs.md §26.7; 04-decisions.md D133, D363, D370).
//!
//! They come from the cluster API (`CaService.GetSealingKeys`) over mutual
//! TLS, fetched by a thread of its own: the storage network carries no
//! secrets, and tokio never runs on a reactor (D3). Reactors read the keys
//! held here, once per capability and so once per connection, never per
//! request.
//!
//! A node asks at start, again every [`EVERY`], and every [`RETRY`] until
//! its first answer. Until then it holds no key and opens no capability,
//! which refuses what needs one and nothing else. A failed fetch keeps the
//! keys held: the map service keeps the key before the newest valid, so a
//! node one fetch behind still opens what the older key sealed.

use hyperstor_pki::seal::{SealingKey, SealingKeys};
use std::sync::{Arc, RwLock};
use std::time::Duration;

/// How often a node asks for the keys once it has them.
pub const EVERY: Duration = Duration::from_secs(30);
/// How often it asks until the first answer.
pub const RETRY: Duration = Duration::from_secs(2);

/// The keys a node holds now, shared by its reactors and the thread that
/// fetches them.
#[derive(Default)]
pub struct Sealing {
    keys: RwLock<Arc<SealingKeys>>,
}

impl Sealing {
    /// With no key: nothing opens.
    pub fn new() -> Arc<Sealing> {
        Arc::new(Sealing::default())
    }

    /// The keys held now.
    pub fn keys(&self) -> Arc<SealingKeys> {
        self.keys.read().unwrap_or_else(|e| e.into_inner()).clone()
    }

    /// Replaces the keys held; the ids before and after.
    pub fn set(&self, keys: SealingKeys) -> (Vec<u8>, Vec<u8>) {
        let new = keys.ids();
        let mut held = self.keys.write().unwrap_or_else(|e| e.into_inner());
        let old = held.ids();
        *held = Arc::new(keys);
        (old, new)
    }
}

/// Where the keys come from: the cluster API at these addresses, tried in
/// turn, with the node's identity.
pub struct Source {
    pub addrs: Vec<String>,
    pub tls: Arc<hyperstor_pki::ClientConfig>,
    pub every: Duration,
    pub retry: Duration,
}

/// The node's identity for the cluster API, from `dir`: `node.crt` on a
/// host that joined as a storage host; on a map service member, whose
/// node has no certificate of its own, the member's `api.crt` (D370).
pub fn identity(dir: &std::path::Path) -> Result<Arc<hyperstor_pki::ClientConfig>, String> {
    let read = |f: &str| {
        std::fs::read_to_string(dir.join(f)).map_err(|e| format!("{}: {e}", dir.join(f).display()))
    };
    let name = ["node", "api"]
        .into_iter()
        .find(|n| dir.join(format!("{n}.key")).exists())
        .ok_or_else(|| format!("{}: neither node.key nor api.key", dir.display()))?;
    hyperstor_pki::client_config(
        &read(&format!("{name}.crt"))?,
        &read(&format!("{name}.key"))?,
        &read("root.crt")?,
    )
    .map(Arc::new)
}

fn client(
    src: &Source,
    addr: &str,
) -> Result<
    (
        connectrpc::client::HttpClient,
        connectrpc::client::ClientConfig,
    ),
    String,
> {
    let uri = format!("https://{addr}")
        .parse::<http::Uri>()
        .map_err(|_| "not an address".to_string())?;
    Ok((
        connectrpc::client::HttpClient::with_tls(src.tls.clone()),
        connectrpc::client::ClientConfig::new(uri).with_default_timeout(Duration::from_secs(5)),
    ))
}

/// The keys the member at `addr` answers with, oldest first.
async fn ask(src: &Source, addr: &str) -> Result<SealingKeys, String> {
    use hyperstor_api::proto::hyperstor::v1 as pb;
    let (http, config) = client(src, addr)?;
    let r = hyperstor_api::CaServiceClient::new(http, config)
        .get_sealing_keys(pb::GetSealingKeysRequest::default())
        .await
        .map_err(|e| e.message.unwrap_or_else(|| "no answer".into()))?;
    let mut got = r.into_owned().keys;
    got.sort_by_key(|k| k.seq);
    let mut keys = Vec::with_capacity(got.len());
    let mut bad = None;
    for mut k in got {
        let bytes: Result<[u8; 32], _> = k.key.as_slice().try_into();
        // The answer's copy isn't left behind in freed memory.
        k.key.iter_mut().for_each(|b| *b = 0);
        match (u8::try_from(k.id), bytes) {
            (Ok(id), Ok(bytes)) if id != 0 => keys.push(SealingKey::new(id, bytes)),
            _ => bad = Some(k.seq),
        }
    }
    match bad {
        Some(seq) => Err(format!("key {seq} is not a sealing key")),
        None if keys.is_empty() => Err("no sealing key".into()),
        None => Ok(SealingKeys::new(keys)),
    }
}

/// The leader's API address, as the member at `addr` knows it.
async fn leader_at(src: &Source, addr: &str) -> Option<String> {
    let (http, config) = client(src, addr).ok()?;
    let r = hyperstor_api::MemberServiceClient::new(http, config)
        .list_members(hyperstor_api::ListMembersRequest::default())
        .await
        .ok()?
        .into_owned();
    r.members
        .into_iter()
        .find(|m| m.leader)
        .map(|m| m.api_address)
        .filter(|a| !a.is_empty())
}

/// One answer of the cluster API, or why none came: each address in
/// turn, and after one that answers without keys, the leader it names.
/// Only the leader makes the cluster's first key, and a node may know
/// only the member on its own host.
async fn fetch(src: &Source) -> Result<SealingKeys, String> {
    let mut why = Vec::new();
    let mut tried: Vec<String> = Vec::new();
    for a in &src.addrs {
        let mut next = Some(a.clone());
        while let Some(addr) = next.take() {
            if tried.contains(&addr) {
                continue;
            }
            tried.push(addr.clone());
            match ask(src, &addr).await {
                Ok(keys) => return Ok(keys),
                Err(e) => {
                    why.push(format!("{addr}: {e}"));
                    next = leader_at(src, &addr).await;
                }
            }
        }
    }
    Err(why.join("; "))
}

/// Fetches into `held` from a new thread until the process ends; logs
/// when the keys change and when fetching starts or stops failing (ids
/// only, never a key).
pub fn start(node: u32, held: Arc<Sealing>, src: Source) -> std::thread::JoinHandle<()> {
    std::thread::Builder::new()
        .name("hyperstord-keys".into())
        .spawn(move || {
            let rt = match tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
            {
                Ok(rt) => rt,
                Err(e) => {
                    eprintln!("hyperstord: node {node}: no runtime for the sealing keys: {e}");
                    return;
                }
            };
            rt.block_on(async {
                let mut failing = false;
                loop {
                    match fetch(&src).await {
                        Ok(keys) => {
                            let (old, new) = held.set(keys);
                            if failing || old != new {
                                eprintln!("hyperstord: node {node}: sealing keys {new:?}");
                            }
                            failing = false;
                        }
                        Err(e) if !failing => {
                            eprintln!(
                                "hyperstord: node {node}: sealing keys from the cluster API: {e}; keeping {:?}",
                                held.keys().ids()
                            );
                            failing = true;
                        }
                        Err(_) => {}
                    }
                    let wait = if held.keys().is_empty() {
                        src.retry
                    } else {
                        src.every
                    };
                    tokio::time::sleep(wait).await;
                }
            })
        })
        .expect("spawn the sealing keys' thread")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_node_holds_nothing_until_set_and_then_the_newest() {
        let held = Sealing::new();
        assert!(held.keys().is_empty());
        let before = held.keys();
        let (old, new) = held.set(SealingKeys::new(vec![SealingKey::new(1, [7; 32])]));
        assert_eq!((old, new), (vec![], vec![1]));
        // A reader keeps the keys it took: a swap never changes them under it.
        assert!(before.is_empty());
        let (old, new) = held.set(SealingKeys::new(vec![
            SealingKey::new(1, [7; 32]),
            SealingKey::new(2, [8; 32]),
        ]));
        assert_eq!((old, new), (vec![1], vec![1, 2]));
        assert_eq!(held.keys().current(), Some(2));
    }

    #[test]
    fn the_identity_is_the_nodes_or_the_members() {
        let dir = tempfile::tempdir().unwrap();
        let e = identity(dir.path()).err().unwrap();
        assert!(e.contains("neither node.key nor api.key"), "{e}");
        // A key without its certificate says which file is missing.
        std::fs::write(dir.path().join("api.key"), "x").unwrap();
        let e = identity(dir.path()).err().unwrap();
        assert!(e.contains("api.crt"), "{e}");
        std::fs::write(dir.path().join("node.key"), "x").unwrap();
        let e = identity(dir.path()).err().unwrap();
        assert!(e.contains("node.crt"), "{e}");
    }
}
