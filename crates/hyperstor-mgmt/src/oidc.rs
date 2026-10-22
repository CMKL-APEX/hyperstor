//! Logins with an identity provider (spec/administration.md §23.9.1,
//! decisions.md D96): OIDC's Authorization Code flow with PKCE and a
//! nonce, through `glidex-auth`'s client, configured by `--oidc FILE`.
//!
//! `FILE` is JSON:
//!
//! - `issuer`, `client_id`: the provider and this client (required);
//! - `client_secret_file`: a file holding the client's secret, read at
//!   each use so a rotated secret needs no restart (required);
//! - `scopes` (default `openid email profile`), `groups_claim` (default
//!   `groups`), `allowed_domains` and `required_groups` (default none:
//!   anyone the provider lets in);
//! - `redirect_uri`: where the provider sends the browser back, as
//!   registered with it; default `<scheme>://<Host>/auth/oidc/callback`.
//!
//! A person is named by the email the provider vouches for, and tied to
//! the provider's subject ([`crate::people::People::oidc_user`]). A
//! login is begun with a `POST`, so a claim code never travels in a URL,
//! and is bound to the browser that began it (cookie `hs_oidc`).

use glidex_auth::oidc::{ClientConfig, ClientSecret, OidcState};
use serde::Deserialize;
use std::path::{Path, PathBuf};

/// What a login is for, kept with its state and never in a URL.
#[derive(Debug, Clone, Default)]
pub struct After {
    /// The first security administrator's claim code.
    pub claim: Option<String>,
}

pub type Oidc = OidcState<After>;

/// The path the provider sends the browser back to.
pub const CALLBACK: &str = "/auth/oidc/callback";
/// The cookie binding a login to the browser that began it.
pub const BROWSER_COOKIE: &str = "hs_oidc";

fn scopes() -> Vec<String> {
    ["openid", "email", "profile"].map(String::from).to_vec()
}

fn groups() -> String {
    "groups".into()
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct File {
    issuer: String,
    client_id: String,
    client_secret_file: PathBuf,
    #[serde(default = "scopes")]
    scopes: Vec<String>,
    #[serde(default = "groups")]
    groups_claim: String,
    #[serde(default)]
    allowed_domains: Vec<String>,
    #[serde(default)]
    required_groups: Vec<String>,
    #[serde(default)]
    redirect_uri: Option<String>,
}

fn read_secret(path: &Path) -> Result<zeroize::Zeroizing<String>, String> {
    let s = zeroize::Zeroizing::new(
        std::fs::read_to_string(path).map_err(|e| format!("{}: {e}", path.display()))?,
    );
    let t = zeroize::Zeroizing::new(s.trim().to_string());
    if t.is_empty() {
        return Err(format!("{}: empty", path.display()));
    }
    Ok(t)
}

/// The client `path` describes; its secret must be readable now.
pub fn load(path: &Path) -> Result<Oidc, String> {
    let text = std::fs::read_to_string(path).map_err(|e| format!("{}: {e}", path.display()))?;
    let f: File = serde_json::from_str(&text).map_err(|e| format!("{}: {e}", path.display()))?;
    if f.issuer.is_empty() || f.client_id.is_empty() {
        return Err(format!(
            "{}: issuer and client_id are required",
            path.display()
        ));
    }
    if !f.scopes.iter().any(|s| s == "openid") {
        return Err(format!(
            "{}: the scopes must include openid",
            path.display()
        ));
    }
    read_secret(&f.client_secret_file)?;
    let secret_file = f.client_secret_file.clone();
    let cfg = ClientConfig {
        enabled: true,
        issuer: f.issuer,
        client_id: f.client_id,
        scopes: f.scopes,
        groups_claim: f.groups_claim,
        allowed_domains: f.allowed_domains,
        required_groups: f.required_groups,
        redirect_uri: f.redirect_uri,
        callback_path: CALLBACK.into(),
        mfa_acr: Vec::new(),
    };
    Ok(OidcState::new(
        cfg,
        ClientSecret::new(move || read_secret(&secret_file)),
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn configuration_is_checked() {
        let d = std::env::temp_dir().join(format!("hs-oidc-{}", std::process::id()));
        std::fs::create_dir_all(&d).unwrap();
        let secret = d.join("secret");
        std::fs::write(&secret, "s3cret\n").unwrap();
        let write = |body: serde_json::Value| {
            let p = d.join("oidc.json");
            std::fs::write(&p, body.to_string()).unwrap();
            p
        };
        let ok = write(serde_json::json!({
            "issuer": "https://idp.example", "client_id": "hyperstor",
            "client_secret_file": secret,
        }));
        let o = load(&ok).unwrap();
        assert!(o.enabled());
        assert_eq!(o.config().scopes, ["openid", "email", "profile"]);
        assert_eq!(
            o.redirect_uri(Some("https://mgmt.example:8852")),
            "https://mgmt.example:8852/auth/oidc/callback"
        );
        let unknown = write(serde_json::json!({
            "issuer": "https://idp.example", "client_id": "hyperstor",
            "client_secret_file": secret, "client_secret": "inline",
        }));
        assert!(load(&unknown).is_err(), "no secrets in the file itself");
        let no_openid = write(serde_json::json!({
            "issuer": "https://idp.example", "client_id": "hyperstor",
            "client_secret_file": secret, "scopes": ["email"],
        }));
        assert!(load(&no_openid).err().unwrap().contains("openid"));
        let no_secret = write(serde_json::json!({
            "issuer": "https://idp.example", "client_id": "hyperstor",
            "client_secret_file": d.join("missing"),
        }));
        assert!(load(&no_secret).is_err());
        let _ = std::fs::remove_dir_all(&d);
    }
}
