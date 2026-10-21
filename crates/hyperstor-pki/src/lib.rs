//! Hyperstor's certificates (spec/administration.md §23.8, decisions.md
//! D53 and D56).
//!
//! - A **root** CA (10 years) signs only intermediates; everyone trusts
//!   the root.
//! - An **intermediate** CA (1 year) signs leaf certificates. Rotating it
//!   changes no trust store.
//! - A **leaf** (30 days) carries its holder's identity as a URI SAN,
//!   `hyperstor://<cluster>/<role>/<id>`; the API authorizes by role.
//!
//! Keys are ECDSA P-256. TLS configurations use rustls with the ring
//! provider, chosen explicitly so the choice doesn't depend on which
//! other crates enable which provider.

use rcgen::{
    BasicConstraints, CertificateParams, DistinguishedName, DnType, ExtendedKeyUsagePurpose, IsCa,
    Issuer, KeyPair, KeyUsagePurpose, SanType,
};
use rustls::pki_types::pem::PemObject;
use rustls::pki_types::{CertificateDer, PrivateKeyDer};
use std::net::IpAddr;
use std::sync::Arc;
use std::time::{Duration, SystemTime};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Role {
    /// A storage node.
    Node,
    /// A map service member.
    Meta,
    /// The management service (`hyperstor-mgmt`) or glidex's control plane.
    Mgmt,
    /// An administrator's tool, `hyperstorctl`.
    Admin,
    /// A bare-metal host that attaches exported volumes.
    Host,
}

impl Role {
    pub fn as_str(self) -> &'static str {
        match self {
            Role::Node => "node",
            Role::Meta => "meta",
            Role::Mgmt => "mgmt",
            Role::Admin => "admin",
            Role::Host => "host",
        }
    }

    pub fn parse(s: &str) -> Option<Role> {
        Some(match s {
            "node" => Role::Node,
            "meta" => Role::Meta,
            "mgmt" => Role::Mgmt,
            "admin" => Role::Admin,
            "host" => Role::Host,
            _ => return None,
        })
    }

    /// May change the cluster through the API.
    pub fn may_administer(self) -> bool {
        matches!(self, Role::Admin | Role::Mgmt)
    }
}

/// Who a leaf certificate belongs to.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Identity {
    pub cluster: String,
    pub role: Role,
    pub id: String,
}

impl Identity {
    pub fn uri(&self) -> String {
        format!(
            "hyperstor://{}/{}/{}",
            self.cluster,
            self.role.as_str(),
            self.id
        )
    }

    pub fn parse(uri: &str) -> Option<Identity> {
        let rest = uri.strip_prefix("hyperstor://")?;
        let mut p = rest.splitn(3, '/');
        let (cluster, role, id) = (p.next()?, p.next()?, p.next()?);
        if cluster.is_empty() || id.is_empty() || id.contains('/') {
            return None;
        }
        Some(Identity {
            cluster: cluster.to_string(),
            role: Role::parse(role)?,
            id: id.to_string(),
        })
    }
}

/// A certificate and its private key, both PEM.
#[derive(Debug, Clone)]
pub struct Pem {
    pub cert: String,
    pub key: String,
}

pub const ROOT_DAYS: u32 = 3650;
pub const INTERMEDIATE_DAYS: u32 = 365;
pub const LEAF_DAYS: u32 = 30;

fn validity(p: &mut CertificateParams, days: u32) {
    let now = time::OffsetDateTime::now_utc();
    // A little backdating, for clocks slightly behind.
    p.not_before = now - time::Duration::minutes(5);
    p.not_after = now + time::Duration::days(days.into());
}

fn ca_params(name: &str, days: u32, path_len: u8) -> CertificateParams {
    let mut p = CertificateParams::default();
    let mut dn = DistinguishedName::new();
    dn.push(DnType::CommonName, name);
    p.distinguished_name = dn;
    p.is_ca = IsCa::Ca(BasicConstraints::Constrained(path_len));
    p.key_usages = vec![KeyUsagePurpose::KeyCertSign, KeyUsagePurpose::CrlSign];
    validity(&mut p, days);
    p
}

fn err(e: impl std::fmt::Display) -> String {
    e.to_string()
}

/// A new root CA for `cluster`, which signs intermediates only.
pub fn new_root(cluster: &str) -> Result<Pem, String> {
    let key = KeyPair::generate().map_err(err)?;
    let cert = ca_params(&format!("hyperstor {cluster} root"), ROOT_DAYS, 1)
        .self_signed(&key)
        .map_err(err)?;
    Ok(Pem {
        cert: cert.pem(),
        key: key.serialize_pem(),
    })
}

/// A new intermediate CA signed by `root`.
pub fn new_intermediate(root: &Pem, cluster: &str) -> Result<Pem, String> {
    let root_key = KeyPair::from_pem(&root.key).map_err(err)?;
    let issuer = Issuer::from_ca_cert_pem(&root.cert, root_key).map_err(err)?;
    let key = KeyPair::generate().map_err(err)?;
    let cert = ca_params(
        &format!("hyperstor {cluster} intermediate"),
        INTERMEDIATE_DAYS,
        0,
    )
    .signed_by(&key, &issuer)
    .map_err(err)?;
    Ok(Pem {
        cert: cert.pem(),
        key: key.serialize_pem(),
    })
}

/// A leaf certificate for `who`, signed by the intermediate `ca`, valid
/// for `days` and for the given addresses and names (a server's; a
/// client needs none). The returned certificate is the chain: the leaf,
/// then the intermediate.
pub fn issue(
    ca: &Pem,
    who: &Identity,
    ips: &[IpAddr],
    names: &[String],
    days: u32,
) -> Result<Pem, String> {
    let ca_key = KeyPair::from_pem(&ca.key).map_err(err)?;
    let issuer = Issuer::from_ca_cert_pem(&ca.cert, ca_key).map_err(err)?;
    let mut p = CertificateParams::default();
    let mut dn = DistinguishedName::new();
    dn.push(DnType::CommonName, who.uri());
    p.distinguished_name = dn;
    p.subject_alt_names = vec![SanType::URI(who.uri().try_into().map_err(err)?)];
    for ip in ips {
        p.subject_alt_names.push(SanType::IpAddress(*ip));
    }
    for n in names {
        p.subject_alt_names
            .push(SanType::DnsName(n.clone().try_into().map_err(err)?));
    }
    p.key_usages = vec![KeyUsagePurpose::DigitalSignature];
    p.extended_key_usages = vec![
        ExtendedKeyUsagePurpose::ServerAuth,
        ExtendedKeyUsagePurpose::ClientAuth,
    ];
    validity(&mut p, days);
    let key = KeyPair::generate().map_err(err)?;
    let cert = p.signed_by(&key, &issuer).map_err(err)?;
    Ok(Pem {
        cert: format!("{}{}", cert.pem(), ca.cert),
        key: key.serialize_pem(),
    })
}

fn certs(pem: &str) -> Result<Vec<CertificateDer<'static>>, String> {
    let v: Vec<_> = CertificateDer::pem_slice_iter(pem.as_bytes())
        .collect::<Result<_, _>>()
        .map_err(err)?;
    if v.is_empty() {
        return Err("no certificate in PEM".into());
    }
    Ok(v)
}

fn key(pem: &str) -> Result<PrivateKeyDer<'static>, String> {
    PrivateKeyDer::from_pem_slice(pem.as_bytes()).map_err(err)
}

fn roots(root_pem: &str) -> Result<Arc<rustls::RootCertStore>, String> {
    let mut store = rustls::RootCertStore::empty();
    for c in certs(root_pem)? {
        store.add(c).map_err(err)?;
    }
    Ok(Arc::new(store))
}

fn provider() -> Arc<rustls::crypto::CryptoProvider> {
    Arc::new(rustls::crypto::ring::default_provider())
}

/// A server that presents `chain` and requires every client to present a
/// certificate that chains to `root`.
pub fn server_config(
    chain: &str,
    key_pem: &str,
    root: &str,
) -> Result<rustls::ServerConfig, String> {
    let verifier =
        rustls::server::WebPkiClientVerifier::builder_with_provider(roots(root)?, provider())
            .build()
            .map_err(err)?;
    let mut c = rustls::ServerConfig::builder_with_provider(provider())
        .with_safe_default_protocol_versions()
        .map_err(err)?
        .with_client_cert_verifier(verifier)
        .with_single_cert(certs(chain)?, key(key_pem)?)
        .map_err(err)?;
    c.alpn_protocols = vec![b"h2".to_vec(), b"http/1.1".to_vec()];
    Ok(c)
}

/// A client that presents `chain` and trusts servers that chain to `root`.
pub fn client_config(
    chain: &str,
    key_pem: &str,
    root: &str,
) -> Result<rustls::ClientConfig, String> {
    rustls::ClientConfig::builder_with_provider(provider())
        .with_safe_default_protocol_versions()
        .map_err(err)?
        .with_root_certificates(roots(root)?)
        .with_client_auth_cert(certs(chain)?, key(key_pem)?)
        .map_err(err)
}

/// The identity in a leaf certificate (DER), if it has one.
pub fn identity_of(der: &[u8]) -> Option<Identity> {
    let (_, cert) = x509_parser::parse_x509_certificate(der).ok()?;
    let san = cert.subject_alternative_name().ok()??;
    san.value.general_names.iter().find_map(|n| match n {
        x509_parser::extensions::GeneralName::URI(u) => Identity::parse(u),
        _ => None,
    })
}

/// When the first certificate in `pem` expires.
pub fn not_after(pem: &str) -> Result<SystemTime, String> {
    let der = CertificateDer::pem_slice_iter(pem.as_bytes())
        .next()
        .ok_or("no certificate in PEM")?
        .map_err(err)?;
    let (_, cert) = x509_parser::parse_x509_certificate(&der).map_err(err)?;
    let secs = cert.validity().not_after.timestamp();
    Ok(SystemTime::UNIX_EPOCH + Duration::from_secs(secs.max(0) as u64))
}

/// Whether the first certificate in `pem` is past two thirds of
/// `lifetime_days`, when it should be renewed (D56).
pub fn due_for_renewal(pem: &str, lifetime_days: u32) -> bool {
    let Ok(end) = not_after(pem) else {
        return true;
    };
    let left = end
        .duration_since(SystemTime::now())
        .unwrap_or(Duration::ZERO);
    left < Duration::from_secs(u64::from(lifetime_days) * 86400 / 3)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn admin() -> Identity {
        Identity {
            cluster: "42".into(),
            role: Role::Admin,
            id: "alice".into(),
        }
    }

    #[test]
    fn identity_uri_round_trip_and_rejects() {
        let a = admin();
        assert_eq!(a.uri(), "hyperstor://42/admin/alice");
        assert_eq!(Identity::parse(&a.uri()), Some(a));
        for bad in [
            "https://42/admin/x",
            "hyperstor://42/root/x",
            "hyperstor://42/admin/",
            "hyperstor:///admin/x",
            "hyperstor://42/admin/a/b",
        ] {
            assert_eq!(Identity::parse(bad), None, "{bad}");
        }
    }

    #[test]
    fn chain_issues_leaves_with_identities() {
        let root = new_root("42").unwrap();
        let ca = new_intermediate(&root, "42").unwrap();
        let leaf = issue(
            &ca,
            &admin(),
            &["127.0.0.1".parse().unwrap()],
            &["localhost".into()],
            LEAF_DAYS,
        )
        .unwrap();
        let chain = certs(&leaf.cert).unwrap();
        assert_eq!(chain.len(), 2, "leaf then intermediate");
        assert_eq!(identity_of(&chain[0]), Some(admin()));
        // The intermediate carries no identity.
        assert_eq!(identity_of(&chain[1]), None);
        assert!(!due_for_renewal(&leaf.cert, LEAF_DAYS));
        assert!(due_for_renewal(&leaf.cert, 365), "30 days left of a year");
        // A file without a certificate is renewed, not a panic.
        assert!(not_after(&leaf.key).is_err());
        assert!(not_after("").is_err());
        assert!(due_for_renewal("", LEAF_DAYS));
        // Both ends build, so the chain and keys fit together.
        server_config(&leaf.cert, &leaf.key, &root.cert).unwrap();
        client_config(&leaf.cert, &leaf.key, &root.cert).unwrap();
        // A key that doesn't match its certificate is refused.
        let other = issue(&ca, &admin(), &[], &[], LEAF_DAYS).unwrap();
        assert!(server_config(&leaf.cert, &other.key, &root.cert).is_err());
    }

    #[test]
    fn roles_that_may_administer() {
        assert!(Role::Admin.may_administer());
        assert!(Role::Mgmt.may_administer());
        for r in [Role::Node, Role::Meta, Role::Host] {
            assert!(!r.may_administer());
        }
    }
}
