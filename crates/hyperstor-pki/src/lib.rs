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
pub use rustls::ClientConfig;
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

/// How far certificates are backdated, for clocks slightly behind.
pub const BACKDATE_SECS: u64 = 300;

fn validity(p: &mut CertificateParams, days: u32) {
    let now = time::OffsetDateTime::now_utc();
    p.not_before = now - time::Duration::seconds(BACKDATE_SECS as i64);
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

/// The parameters of a leaf for `who`: its identity as a URI SAN, the
/// given addresses and names, server and client use, `days` long.
fn leaf_params(
    who: &Identity,
    ips: &[IpAddr],
    names: &[String],
    days: u32,
) -> Result<CertificateParams, String> {
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
    // A random serial: rcgen's default is a hash of the key, which a
    // certificate renewed for the same key would share, and revocation
    // (D56) names certificates by serial.
    let rng = ring::rand::SystemRandom::new();
    let mut serial: [u8; 16] = ring::rand::generate(&rng).map_err(err)?.expose();
    serial[0] &= 0x7f;
    p.serial_number = Some(rcgen::SerialNumber::from_slice(&serial));
    validity(&mut p, days);
    Ok(p)
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
    let key = KeyPair::generate().map_err(err)?;
    let cert = leaf_params(who, ips, names, days)?
        .signed_by(&key, &issuer)
        .map_err(err)?;
    Ok(Pem {
        cert: format!("{}{}", cert.pem(), ca.cert),
        key: key.serialize_pem(),
    })
}

/// A new key and a certificate request for it (D57): the key stays with
/// the caller, the request (PEM) goes to the signer. The request names
/// nothing; the signer decides the identity.
pub fn new_request() -> Result<(String, String), String> {
    let key = KeyPair::generate().map_err(err)?;
    let csr = CertificateParams::default()
        .serialize_request(&key)
        .map_err(err)?;
    Ok((key.serialize_pem(), csr.pem().map_err(err)?))
}

/// Signs the public key of the request `csr` (PEM, its signature checked)
/// as `who`, for `ips` and `names`, with the intermediate `ca`. Only the
/// key comes from the request: identity, addresses and lifetime are the
/// signer's. Returns the chain, leaf then intermediate.
pub fn sign_request(
    ca: &Pem,
    csr: &str,
    who: &Identity,
    ips: &[IpAddr],
    names: &[String],
    days: u32,
) -> Result<String, String> {
    let req = rcgen::CertificateSigningRequestParams::from_pem(csr)
        .map_err(|e| format!("certificate request: {e}"))?;
    let ca_key = KeyPair::from_pem(&ca.key).map_err(err)?;
    let issuer = Issuer::from_ca_cert_pem(&ca.cert, ca_key).map_err(err)?;
    let signed = rcgen::CertificateSigningRequestParams {
        params: leaf_params(who, ips, names, days)?,
        public_key: req.public_key,
    }
    .signed_by(&issuer)
    .map_err(err)?;
    Ok(format!("{}{}", signed.pem(), ca.cert))
}

/// The SHA-256 of the first certificate in `pem` (its DER), in hex: what a
/// join token pins the root by (D57).
pub fn fingerprint(pem: &str) -> Result<String, String> {
    let der = first_cert(pem)?;
    Ok(hex(
        ring::digest::digest(&ring::digest::SHA256, &der).as_ref()
    ))
}

fn hex(b: &[u8]) -> String {
    b.iter().map(|x| format!("{x:02x}")).collect()
}

/// The SHA-256 of `bytes`, in hex: how join tokens are kept (D57).
pub fn sha256_hex(bytes: &[u8]) -> String {
    hex(ring::digest::digest(&ring::digest::SHA256, bytes).as_ref())
}

/// The addresses and names the first certificate in `pem` is for, so a
/// renewal names the same ones.
pub fn names_of(pem: &str) -> Result<(Vec<IpAddr>, Vec<String>), String> {
    names_of_der(&first_cert(pem)?)
}

/// As [`names_of`], for a certificate in DER.
pub fn names_of_der(der: &[u8]) -> Result<(Vec<IpAddr>, Vec<String>), String> {
    let (_, cert) = x509_parser::parse_x509_certificate(der).map_err(err)?;
    let (mut ips, mut names) = (Vec::new(), Vec::new());
    if let Ok(Some(san)) = cert.subject_alternative_name() {
        for n in &san.value.general_names {
            match n {
                x509_parser::extensions::GeneralName::IPAddress(b) => match b.len() {
                    4 => ips.push(IpAddr::from(<[u8; 4]>::try_from(*b).expect("4 bytes"))),
                    16 => ips.push(IpAddr::from(<[u8; 16]>::try_from(*b).expect("16 bytes"))),
                    _ => {}
                },
                x509_parser::extensions::GeneralName::DNSName(d) => names.push(d.to_string()),
                _ => {}
            }
        }
    }
    Ok((ips, names))
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

/// The first certificate in `pem`, in DER.
fn first_cert(pem: &str) -> Result<CertificateDer<'static>, String> {
    certs(pem)?
        .into_iter()
        .next()
        .ok_or_else(|| "no certificate in PEM".to_string())
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

/// As [`server_config`], but a client may also connect without a
/// certificate: the cluster API, where a host joining with a token has
/// none yet (D57). Every call but the join then refuses it.
pub fn api_server_config(
    chain: &str,
    key_pem: &str,
    root: &str,
) -> Result<rustls::ServerConfig, String> {
    let verifier =
        rustls::server::WebPkiClientVerifier::builder_with_provider(roots(root)?, provider())
            .allow_unauthenticated()
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

/// A server for browsers: presents `chain`, asks no client certificate
/// (people log in instead), HTTP/1.1 only: the management service's web
/// side (spec/administration.md §23.7).
pub fn web_server_config(chain: &str, key_pem: &str) -> Result<rustls::ServerConfig, String> {
    let mut c = rustls::ServerConfig::builder_with_provider(provider())
        .with_safe_default_protocol_versions()
        .map_err(err)?
        .with_no_client_auth()
        .with_single_cert(certs(chain)?, key(key_pem)?)
        .map_err(err)?;
    c.alpn_protocols = vec![b"http/1.1".to_vec()];
    Ok(c)
}

/// A client without a certificate that trusts a server only if its chain
/// includes a root whose fingerprint is `pin` ([`fingerprint`]) and
/// verifies under it: how a joining host reaches the cluster before it
/// has the root (D57). The servers send the root in their chain.
pub fn pinned_client_config(pin: &str) -> Result<rustls::ClientConfig, String> {
    let verifier = Arc::new(Pinned {
        pin: pin.to_ascii_lowercase(),
        provider: provider(),
    });
    Ok(rustls::ClientConfig::builder_with_provider(provider())
        .with_safe_default_protocol_versions()
        .map_err(err)?
        .dangerous()
        .with_custom_certificate_verifier(verifier)
        .with_no_client_auth())
}

#[derive(Debug)]
struct Pinned {
    pin: String,
    provider: Arc<rustls::crypto::CryptoProvider>,
}

impl rustls::client::danger::ServerCertVerifier for Pinned {
    fn verify_server_cert(
        &self,
        end_entity: &CertificateDer<'_>,
        intermediates: &[CertificateDer<'_>],
        server_name: &rustls::pki_types::ServerName<'_>,
        ocsp: &[u8],
        now: rustls::pki_types::UnixTime,
    ) -> Result<rustls::client::danger::ServerCertVerified, rustls::Error> {
        let root = intermediates
            .iter()
            .find(|c| hex(ring::digest::digest(&ring::digest::SHA256, c).as_ref()) == self.pin)
            .ok_or_else(|| {
                rustls::Error::General(
                    "the server's chain has no root with the token's fingerprint".into(),
                )
            })?;
        let mut store = rustls::RootCertStore::empty();
        store
            .add(root.clone().into_owned())
            .map_err(|e| rustls::Error::General(format!("pinned root: {e}")))?;
        let others: Vec<CertificateDer<'_>> = intermediates
            .iter()
            .filter(|c| c.as_ref() != root.as_ref())
            .cloned()
            .collect();
        rustls::client::WebPkiServerVerifier::builder_with_provider(
            Arc::new(store),
            self.provider.clone(),
        )
        .build()
        .map_err(|e| rustls::Error::General(e.to_string()))?
        .verify_server_cert(end_entity, &others, server_name, ocsp, now)
    }

    fn verify_tls12_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &rustls::DigitallySignedStruct,
    ) -> Result<rustls::client::danger::HandshakeSignatureValid, rustls::Error> {
        rustls::crypto::verify_tls12_signature(
            message,
            cert,
            dss,
            &self.provider.signature_verification_algorithms,
        )
    }

    fn verify_tls13_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &rustls::DigitallySignedStruct,
    ) -> Result<rustls::client::danger::HandshakeSignatureValid, rustls::Error> {
        rustls::crypto::verify_tls13_signature(
            message,
            cert,
            dss,
            &self.provider.signature_verification_algorithms,
        )
    }

    fn supported_verify_schemes(&self) -> Vec<rustls::SignatureScheme> {
        self.provider
            .signature_verification_algorithms
            .supported_schemes()
    }
}

/// A client of `hyperstor-mgmt`'s web side (no certificate of its own):
/// it trusts the host's CAs, and `extra_ca`'s certificates too when given
/// (an operator's own CA, or the cluster's), so any of D107's web
/// certificates verifies.
pub fn web_client_config(extra_ca: Option<&str>) -> Result<rustls::ClientConfig, String> {
    let mut store = rustls::RootCertStore::empty();
    let native = rustls_native_certs::load_native_certs();
    store.add_parsable_certificates(native.certs);
    if let Some(pem) = extra_ca {
        for c in certs(pem)? {
            store.add(c).map_err(err)?;
        }
    }
    if store.is_empty() {
        return Err("no trusted CA: none on this host, and no --mgmt-ca".into());
    }
    Ok(rustls::ClientConfig::builder_with_provider(provider())
        .with_safe_default_protocol_versions()
        .map_err(err)?
        .with_root_certificates(store)
        .with_no_client_auth())
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

/// A certificate's serial number, hex, without leading zeros: how the
/// revocation list names it (D56).
pub fn serial_of(der: &[u8]) -> Option<String> {
    let (_, cert) = x509_parser::parse_x509_certificate(der).ok()?;
    let b = cert.raw_serial();
    let h = hex(b);
    let t = h.trim_start_matches('0');
    Some(if t.is_empty() {
        "0".into()
    } else {
        t.to_string()
    })
}

/// The serial of the first certificate in `pem`.
pub fn serial_in(pem: &str) -> Option<String> {
    serial_of(&first_cert(pem).ok()?)
}

/// When a certificate (DER) expires, in seconds since 1970.
pub fn not_after_unix(der: &[u8]) -> Option<u64> {
    let (_, cert) = x509_parser::parse_x509_certificate(der).ok()?;
    u64::try_from(cert.validity().not_after.timestamp()).ok()
}

/// When a certificate (DER) was issued, in seconds since 1970: its start,
/// less the backdating.
pub fn issued_unix(der: &[u8]) -> Option<u64> {
    let (_, cert) = x509_parser::parse_x509_certificate(der).ok()?;
    u64::try_from(cert.validity().not_before.timestamp())
        .ok()
        .map(|t| t + BACKDATE_SECS)
}

/// The identity in the first certificate in `pem`, if it has one.
pub fn identity_in(pem: &str) -> Option<Identity> {
    identity_of(&first_cert(pem).ok()?)
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

/// PBKDF2-HMAC-SHA256 iterations for a root backup's passphrase (OWASP's
/// 2023 recommendation): slow enough to make guessing costly, a fraction
/// of a second for the one person restoring.
pub const BACKUP_ITERATIONS: u32 = 600_000;

const BACKUP_MAGIC: &str = "hyperstor root backup v1";

fn unhex(s: &str) -> Option<Vec<u8>> {
    if !s.len().is_multiple_of(2) {
        return None;
    }
    (0..s.len())
        .step_by(2)
        .map(|i| u8::from_str_radix(s.get(i..i + 2)?, 16).ok())
        .collect()
}

fn backup_key(passphrase: &str, salt: &[u8]) -> Result<ring::aead::LessSafeKey, String> {
    let mut key = [0u8; 32];
    ring::pbkdf2::derive(
        ring::pbkdf2::PBKDF2_HMAC_SHA256,
        std::num::NonZeroU32::new(BACKUP_ITERATIONS).expect("non-zero"),
        salt,
        passphrase.as_bytes(),
        &mut key,
    );
    let k = ring::aead::UnboundKey::new(&ring::aead::AES_256_GCM, &key).map_err(err)?;
    Ok(ring::aead::LessSafeKey::new(k))
}

/// The root CA, certificate and key, encrypted with `passphrase`, as the
/// text of a backup file (spec/administration.md §23.8, D84): AES-256-GCM
/// with a key derived by PBKDF2-HMAC-SHA256 from the passphrase and a
/// random salt. The cluster and the root's fingerprint are in the clear,
/// to tell backups apart, and authenticated with the rest.
pub fn root_backup(root: &Pem, cluster: &str, passphrase: &str) -> Result<String, String> {
    let rng = ring::rand::SystemRandom::new();
    let salt: [u8; 16] = ring::rand::generate(&rng).map_err(err)?.expose();
    let nonce: [u8; 12] = ring::rand::generate(&rng).map_err(err)?.expose();
    let head = format!(
        "{BACKUP_MAGIC}\ncluster {cluster}\nfingerprint {}\nkdf pbkdf2-sha256 {BACKUP_ITERATIONS} {}\nnonce {}\n",
        fingerprint(&root.cert)?,
        hex(&salt),
        hex(&nonce)
    );
    let mut data = format!("{}{}", root.cert, root.key).into_bytes();
    backup_key(passphrase, &salt)?
        .seal_in_place_append_tag(
            ring::aead::Nonce::assume_unique_for_key(nonce),
            ring::aead::Aad::from(head.as_bytes()),
            &mut data,
        )
        .map_err(err)?;
    Ok(format!("{head}data {}\n", hex(&data)))
}

/// The cluster a [`root_backup`] is for, from its clear header.
pub fn backup_cluster(text: &str) -> Option<String> {
    let mut l = text.lines();
    (l.next()? == BACKUP_MAGIC)
        .then(|| l.next()?.strip_prefix("cluster ").map(String::from))
        .flatten()
}

/// Opens a [`root_backup`]: the cluster and the root CA. A wrong
/// passphrase, or a file changed in any way, fails.
pub fn root_restore(text: &str, passphrase: &str) -> Result<(String, Pem), String> {
    let bad = || "not a hyperstor root backup".to_string();
    let lines: Vec<&str> = text.lines().collect();
    let [magic, cluster, fp, kdf, nonce, data, ..] = lines[..] else {
        return Err(bad());
    };
    if magic != BACKUP_MAGIC {
        return Err(bad());
    }
    let cluster = cluster.strip_prefix("cluster ").ok_or_else(bad)?;
    let fp = fp.strip_prefix("fingerprint ").ok_or_else(bad)?;
    let kdf: Vec<&str> = kdf.split(' ').collect();
    let ["kdf", "pbkdf2-sha256", iters, salt] = kdf[..] else {
        return Err(bad());
    };
    if iters.parse::<u32>() != Ok(BACKUP_ITERATIONS) {
        return Err(format!("{iters} iterations: not this version's"));
    }
    let salt = unhex(salt).ok_or_else(bad)?;
    let nonce: [u8; 12] = unhex(nonce.strip_prefix("nonce ").ok_or_else(bad)?)
        .and_then(|n| n.try_into().ok())
        .ok_or_else(bad)?;
    let mut ct = unhex(data.strip_prefix("data ").ok_or_else(bad)?).ok_or_else(bad)?;
    // The authenticated header: everything before the data line.
    let head_len = text.find("\ndata ").ok_or_else(bad)? + 1;
    let plain = backup_key(passphrase, &salt)?
        .open_in_place(
            ring::aead::Nonce::assume_unique_for_key(nonce),
            ring::aead::Aad::from(&text.as_bytes()[..head_len]),
            &mut ct,
        )
        .map_err(|_| "wrong passphrase, or the backup was changed".to_string())?;
    let plain = std::str::from_utf8(plain).map_err(|_| bad())?;
    let at = plain.find("-----BEGIN PRIVATE KEY-----").ok_or_else(bad)?;
    let root = Pem {
        cert: plain[..at].to_string(),
        key: plain[at..].to_string(),
    };
    if fingerprint(&root.cert)? != fp {
        return Err(bad());
    }
    // The key must be the certificate's.
    KeyPair::from_pem(&root.key).map_err(err)?;
    Issuer::from_ca_cert_pem(&root.cert, KeyPair::from_pem(&root.key).map_err(err)?)
        .map_err(err)?;
    Ok((cluster.to_string(), root))
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

    /// Runs a TLS handshake between the two in memory.
    fn handshake(
        client: rustls::ClientConfig,
        server: rustls::ServerConfig,
        ip: &str,
    ) -> Result<(), rustls::Error> {
        let name = rustls::pki_types::ServerName::IpAddress(ip.parse::<IpAddr>().unwrap().into());
        let mut c = rustls::ClientConnection::new(Arc::new(client), name)?;
        let mut s = rustls::ServerConnection::new(Arc::new(server))?;
        for _ in 0..10 {
            let mut buf = Vec::new();
            while c.wants_write() {
                c.write_tls(&mut buf).unwrap();
            }
            let mut r = &buf[..];
            while !r.is_empty() {
                s.read_tls(&mut r).unwrap();
            }
            s.process_new_packets()?;
            let mut buf = Vec::new();
            while s.wants_write() {
                s.write_tls(&mut buf).unwrap();
            }
            let mut r = &buf[..];
            while !r.is_empty() {
                c.read_tls(&mut r).unwrap();
            }
            c.process_new_packets()?;
            if !c.is_handshaking() && !s.is_handshaking() {
                return Ok(());
            }
        }
        panic!("handshake didn't finish");
    }

    #[test]
    fn requests_are_signed_with_the_signers_identity() {
        let root = new_root("42").unwrap();
        let ca = new_intermediate(&root, "42").unwrap();
        let (key, csr) = new_request().unwrap();
        let who = Identity {
            cluster: "42".into(),
            role: Role::Node,
            id: "3".into(),
        };
        let ip: IpAddr = "192.0.2.7".parse().unwrap();
        let chain = sign_request(&ca, &csr, &who, &[ip], &[], LEAF_DAYS).unwrap();
        let der = certs(&chain).unwrap();
        assert_eq!(der.len(), 2, "leaf then intermediate");
        assert_eq!(identity_of(&der[0]), Some(who));
        assert_eq!(names_of(&chain).unwrap(), (vec![ip], vec![]));
        // The key the requester kept fits the certificate.
        client_config(&chain, &key, &root.cert).unwrap();
        // A request whose signature doesn't hold is refused.
        let mut bad = csr.lines().map(String::from).collect::<Vec<_>>();
        let i = bad.len() / 2;
        bad[i] = bad[i].chars().rev().collect();
        assert!(sign_request(&ca, &bad.join("\n"), &admin(), &[], &[], 1).is_err());
        // The fingerprint is of the DER, hex, stable.
        let f = fingerprint(&root.cert).unwrap();
        assert_eq!(f.len(), 64);
        assert_eq!(f, fingerprint(&root.cert).unwrap());
        assert_ne!(f, fingerprint(&ca.cert).unwrap());
    }

    /// A joining host trusts the API only if the root it pins is in the
    /// server's chain, and the API lets it in without a certificate.
    #[test]
    fn a_pinned_client_trusts_only_its_root() {
        let root = new_root("42").unwrap();
        let ca = new_intermediate(&root, "42").unwrap();
        let ip: IpAddr = "127.0.0.1".parse().unwrap();
        let meta = Identity {
            cluster: "42".into(),
            role: Role::Meta,
            id: "1".into(),
        };
        let api = issue(&ca, &meta, &[ip], &[], LEAF_DAYS).unwrap();
        let with_root = format!("{}{}", api.cert, root.cert);
        let server = || api_server_config(&with_root, &api.key, &root.cert).unwrap();
        let pin = fingerprint(&root.cert).unwrap();
        handshake(pinned_client_config(&pin).unwrap(), server(), "127.0.0.1").unwrap();
        // Another cluster's root: refused.
        let other = new_root("43").unwrap();
        let wrong = fingerprint(&other.cert).unwrap();
        assert!(handshake(pinned_client_config(&wrong).unwrap(), server(), "127.0.0.1").is_err());
        // A server that doesn't send the root can't be pinned.
        let bare = api_server_config(&api.cert, &api.key, &root.cert).unwrap();
        assert!(handshake(pinned_client_config(&pin).unwrap(), bare, "127.0.0.1").is_err());
        // The right root but the wrong address: refused.
        assert!(handshake(pinned_client_config(&pin).unwrap(), server(), "127.0.0.2").is_err());
        // An impostor that copies the root into its own chain, signed by
        // another CA: refused.
        let fake_ca = new_intermediate(&other, "42").unwrap();
        let fake = issue(&fake_ca, &meta, &[ip], &[], LEAF_DAYS).unwrap();
        let fake_chain = format!("{}{}", fake.cert, root.cert);
        let impostor = api_server_config(&fake_chain, &fake.key, &other.cert).unwrap();
        assert!(handshake(pinned_client_config(&pin).unwrap(), impostor, "127.0.0.1").is_err());
        // Clients with certificates still work, as before.
        let adm = issue(&ca, &admin(), &[], &[], LEAF_DAYS).unwrap();
        handshake(
            client_config(&adm.cert, &adm.key, &root.cert).unwrap(),
            server(),
            "127.0.0.1",
        )
        .unwrap();
    }

    #[test]
    fn root_backups_open_only_with_their_passphrase() {
        let root = new_root("42").unwrap();
        let b = root_backup(&root, "42", "correct horse battery").unwrap();
        assert!(b.starts_with("hyperstor root backup v1\ncluster 42\n"));
        assert!(!b.contains("PRIVATE KEY"), "the key is encrypted");
        let (cluster, back) = root_restore(&b, "correct horse battery").unwrap();
        assert_eq!(
            (cluster.as_str(), back.cert.as_str(), back.key.as_str()),
            ("42", root.cert.as_str(), root.key.as_str())
        );
        assert!(root_restore(&b, "correct horse batter").is_err());
        // The clear header is authenticated: a changed cluster fails.
        assert!(
            root_restore(
                &b.replace("cluster 42", "cluster 43"),
                "correct horse battery"
            )
            .is_err()
        );
        assert!(root_restore("nonsense", "x").is_err());
        // Each backup is salted afresh.
        assert_ne!(
            b,
            root_backup(&root, "42", "correct horse battery").unwrap()
        );
    }

    #[test]
    fn leaves_get_random_serials() {
        let root = new_root("42").unwrap();
        let ca = new_intermediate(&root, "42").unwrap();
        let a = issue(&ca, &admin(), &[], &[], LEAF_DAYS).unwrap();
        let b = issue(&ca, &admin(), &[], &[], LEAF_DAYS).unwrap();
        let (sa, sb) = (serial_in(&a.cert).unwrap(), serial_in(&b.cert).unwrap());
        assert_ne!(sa, sb);
        assert!(sa.len() >= 24 && sa.bytes().all(|c| c.is_ascii_hexdigit()));
        // The same key signed twice still gets two serials.
        let (_, csr) = new_request().unwrap();
        let c1 = sign_request(&ca, &csr, &admin(), &[], &[], 1).unwrap();
        let c2 = sign_request(&ca, &csr, &admin(), &[], &[], 1).unwrap();
        assert_ne!(serial_in(&c1), serial_in(&c2));
        let end = not_after_unix(&first_cert(&a.cert).unwrap()).unwrap();
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_secs();
        assert!(end > now + 29 * 86400 && end <= now + 30 * 86400 + 60);
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
