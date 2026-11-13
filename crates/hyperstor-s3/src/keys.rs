//! Access keys: fetched from the map service, which keeps them (D231;
//! [`crate::gateway::Source::Api`]), or, without the cluster API (tests),
//! from a file the operator writes, one key per line,
//!
//! ```text
//! # access key id, secret, tenant[, project for its new buckets]
//! EXAMPLEKEYID0000 <secret> 1
//! ```
//!
//! read at start. The file must be readable by its owner alone; the
//! secrets stay in memory, wiped when dropped, and are never logged.

use std::collections::HashMap;
use std::os::unix::fs::MetadataExt;
use std::path::Path;
use zeroize::Zeroizing;

pub struct Key {
    pub secret: Zeroizing<String>,
    pub tenant: u32,
    /// The project its new buckets go to (20-tenants.md §24.9, D300); 0:
    /// the tenant's default project.
    pub project: u32,
}

#[derive(Default)]
pub struct Keys(HashMap<String, Key>);

impl Keys {
    pub fn load(path: &Path) -> Result<Keys, String> {
        let shown = path.display();
        let m = std::fs::metadata(path).map_err(|e| format!("{shown}: {e}"))?;
        if m.mode() & 0o077 != 0 {
            return Err(format!(
                "{shown}: readable by others than its owner (chmod 600 it)"
            ));
        }
        let text =
            Zeroizing::new(std::fs::read_to_string(path).map_err(|e| format!("{shown}: {e}"))?);
        Keys::parse(&text).map_err(|e| format!("{shown}: {e}"))
    }

    /// Lines of `id secret tenant [project]`; `#` starts a comment.
    pub fn parse(text: &str) -> Result<Keys, String> {
        let mut keys = HashMap::new();
        for (n, line) in text.lines().enumerate() {
            let line = line.split('#').next().unwrap_or("").trim();
            if line.is_empty() {
                continue;
            }
            // Never the line itself: it holds a secret.
            let bad = || format!("line {}: not `id secret tenant [project]`", n + 1);
            let f: Vec<&str> = line.split_whitespace().collect();
            let (id, secret, tenant, project) = match f[..] {
                [id, secret, tenant] => (id, secret, tenant, "0"),
                [id, secret, tenant, project] => (id, secret, tenant, project),
                _ => return Err(bad()),
            };
            let tenant = tenant.parse().map_err(|_| bad())?;
            let project = project.parse().map_err(|_| bad())?;
            let key = Key {
                secret: Zeroizing::new(secret.to_string()),
                tenant,
                project,
            };
            if keys.insert(id.to_string(), key).is_some() {
                return Err(format!("line {}: key {id} again", n + 1));
            }
        }
        Ok(Keys(keys))
    }

    /// From `(id, secret, tenant)`s.
    pub fn from_keys(keys: impl IntoIterator<Item = (String, Zeroizing<String>, u32)>) -> Keys {
        Keys::from_keys_in(
            keys.into_iter()
                .map(|(id, secret, tenant)| (id, secret, tenant, 0)),
        )
    }

    /// From `(id, secret, tenant, project)`s, as the cluster API gives them.
    pub fn from_keys_in(
        keys: impl IntoIterator<Item = (String, Zeroizing<String>, u32, u32)>,
    ) -> Keys {
        Keys(
            keys.into_iter()
                .map(|(id, secret, tenant, project)| {
                    (
                        id,
                        Key {
                            secret,
                            tenant,
                            project,
                        },
                    )
                })
                .collect(),
        )
    }

    pub fn get(&self, id: &str) -> Option<&Key> {
        self.0.get(id)
    }

    pub fn len(&self) -> usize {
        self.0.len()
    }

    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn keys_parse() {
        let k = Keys::parse("# c\nAK1 s1 3\n\n  AK2\ts2  4 # x\n").unwrap();
        assert_eq!(k.len(), 2);
        assert_eq!(
            k.get("AK2").map(|k| (k.secret.as_str(), k.tenant)),
            Some(("s2", 4))
        );
        assert!(Keys::parse("AK1 s1").is_err());
        assert_eq!(
            Keys::parse("AK1 s1 3 12")
                .unwrap()
                .get("AK1")
                .unwrap()
                .project,
            12
        );
        assert_eq!(k.get("AK2").unwrap().project, 0);
        assert!(Keys::parse("AK1 s1 3 12 13").is_err());
        assert!(Keys::parse("AK1 s1 x").is_err());
        assert!(Keys::parse("AK1 s1 1\nAK1 s2 1").is_err());
        let e = Keys::parse("AK1 hidden-secret").err().unwrap();
        assert!(!e.contains("hidden-secret"));
    }
}
