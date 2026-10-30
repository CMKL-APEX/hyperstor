//! The node's Tailscale certificate for hyperstor-mgmt (04-decisions.md D109),
//! as glidex's installer grants it to glidex-ui (glidex's
//! tailscale-tls.md §4.6).
//!
//! Opt-in (`--tailscale-cert`, saved). `tailscaled` hands certificates
//! only to root and to the one user `TS_PERMIT_CERT_UID` names. Turning it
//! on writes a marked drop-in naming `hyperstor`, unless that variable is
//! already `hyperstor` (nothing to do) or names someone else, glidex-ui
//! say: then nothing changes and mgmt serves only its usual certificate.
//! Turning it off removes only the marked drop-in.

use std::path::Path;

/// `tailscaled`'s local API.
pub const SOCKET: &str = "/run/tailscale/tailscaled.sock";
pub const DROPIN: &str = "/etc/systemd/system/tailscaled.service.d/50-hyperstor-cert.conf";
const MARKER: &str = "# Managed by hyperstor-install";
const PERMIT: &str = "TS_PERMIT_CERT_UID";
/// The user mgmt runs as.
pub const USER: &str = "hyperstor";

pub fn dropin() -> String {
    format!(
        "{MARKER}: lets hyperstor-mgmt fetch this node's Tailscale certificate (04-decisions.md D109).\n\
         # Remove with hyperstor-install install --no-tailscale-cert.\n\
         [Service]\nEnvironment={PERMIT}={USER}\n"
    )
}

/// What to do about the grant.
#[derive(Debug, PartialEq, Eq)]
pub enum Grant {
    /// Write the drop-in (and restart `tailscaled`).
    Write,
    /// Already granted to `hyperstor`.
    Have,
    /// Granted to someone else: leave it, and don't serve it.
    Refuse(String),
}

/// `configured`: `TS_PERMIT_CERT_UID` as `tailscaled` would get it, if
/// set; `uid`: hyperstor's.
pub fn grant(configured: Option<&str>, uid: Option<u32>) -> Grant {
    match configured.map(str::trim) {
        None | Some("") => Grant::Write,
        Some(v) if v == USER || uid.is_some_and(|u| v == u.to_string()) => Grant::Have,
        Some(v) => Grant::Refuse(v.to_string()),
    }
}

fn unquote(s: &str) -> String {
    s.trim().trim_matches('"').trim_matches('\'').to_string()
}

/// `PERMIT`'s value among systemd `Environment=` assignments
/// (`A=1 "B=2 3"`), the last one winning.
pub fn from_environment(value: &str) -> Option<String> {
    let mut found = None;
    let mut rest = value.trim();
    while !rest.is_empty() {
        let (word, tail) = if let Some(r) = rest.strip_prefix('"') {
            r.split_once('"').unwrap_or((r, ""))
        } else {
            rest.split_once(char::is_whitespace).unwrap_or((rest, ""))
        };
        if let Some(v) = word.strip_prefix(&format!("{PERMIT}=")) {
            found = Some(unquote(v));
        }
        rest = tail.trim_start();
    }
    found
}

/// `PERMIT`'s value in an environment file (`KEY=value` lines).
pub fn from_env_file(text: &str) -> Option<String> {
    text.lines()
        .filter_map(|l| l.trim().strip_prefix(&format!("{PERMIT}=")))
        .map(unquote)
        .next_back()
}

/// What `tailscaled` would get: from the running system with `real`, else
/// from the drop-ins and `/etc/default/tailscaled` under `root`.
pub fn configured(root: &Path, real: bool) -> Option<String> {
    if real {
        let show = |p: &str| {
            std::process::Command::new("systemctl")
                .args(["show", "tailscaled.service", "-p", p, "--value"])
                .output()
                .map(|o| String::from_utf8_lossy(&o.stdout).into_owned())
                .unwrap_or_default()
        };
        let files: Vec<String> = show("EnvironmentFiles")
            .split_whitespace()
            .filter(|w| w.starts_with('/'))
            .map(String::from)
            .collect();
        let from_files = files
            .iter()
            .filter_map(|f| std::fs::read_to_string(f).ok())
            .filter_map(|t| from_env_file(&t))
            .next_back();
        return from_environment(&show("Environment")).or(from_files);
    }
    let dir = root.join("etc/systemd/system/tailscaled.service.d");
    let mut confs: Vec<_> = std::fs::read_dir(&dir)
        .map(|d| d.filter_map(|e| e.ok().map(|e| e.path())).collect())
        .unwrap_or_default();
    confs.sort();
    let from_dropins = confs
        .iter()
        .filter_map(|p| std::fs::read_to_string(p).ok())
        .flat_map(|t| {
            t.lines()
                .filter_map(|l| l.trim().strip_prefix("Environment=").map(String::from))
                .collect::<Vec<_>>()
        })
        .filter_map(|v| from_environment(&v))
        .next_back();
    from_dropins.or_else(|| {
        std::fs::read_to_string(root.join("etc/default/tailscaled"))
            .ok()
            .and_then(|t| from_env_file(&t))
    })
}

/// Whether `path` is a drop-in this installer wrote.
pub fn ours(path: &Path) -> bool {
    std::fs::read_to_string(path).is_ok_and(|t| t.starts_with(MARKER))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn who_holds_the_grant() {
        assert_eq!(grant(None, Some(990)), Grant::Write);
        assert_eq!(grant(Some(""), None), Grant::Write);
        assert_eq!(grant(Some("hyperstor"), None), Grant::Have);
        assert_eq!(grant(Some("990"), Some(990)), Grant::Have);
        assert_eq!(
            grant(Some("glidex-ui"), Some(990)),
            Grant::Refuse("glidex-ui".into())
        );
        assert_eq!(
            from_environment(r#"PORT=41641 "TS_PERMIT_CERT_UID=glidex-ui" X=1"#).as_deref(),
            Some("glidex-ui")
        );
        assert_eq!(from_environment("PORT=41641 FLAGS="), None);
        assert_eq!(
            from_env_file("PORT=\"41641\"\nTS_PERMIT_CERT_UID='hyperstor'\n").as_deref(),
            Some("hyperstor")
        );
    }

    #[test]
    fn under_a_root() {
        let d = std::env::temp_dir().join(format!("hs-ts-{}", std::process::id()));
        let dropins = d.join("etc/systemd/system/tailscaled.service.d");
        std::fs::create_dir_all(&dropins).unwrap();
        assert_eq!(configured(&d, false), None);
        std::fs::write(
            dropins.join("50-glidex-cert.conf"),
            "[Service]\nEnvironment=TS_PERMIT_CERT_UID=glidex-ui\n",
        )
        .unwrap();
        assert_eq!(configured(&d, false).as_deref(), Some("glidex-ui"));
        let ours_path = dropins.join("50-hyperstor-cert.conf");
        std::fs::write(&ours_path, dropin()).unwrap();
        assert!(ours(&ours_path));
        assert!(!ours(&dropins.join("50-glidex-cert.conf")));
        let _ = std::fs::remove_dir_all(&d);
    }
}
