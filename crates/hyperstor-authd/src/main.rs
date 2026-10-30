//! `hyperstor-authd --config FILE`
//!
//! The root helper that checks people's passwords with PAM for
//! `hyperstor-mgmt` (spec/19-administration.md §23.9.1, 04-decisions.md D99):
//! `glidex-authd`'s server (glidex security.md §5.3) with hyperstor's
//! defaults. It answers only the `hyperstor` service user, on
//! `/run/hyperstor-authd/auth.sock` (socket-activated by systemd, or bound
//! here), and lets in members of `hyperstor-users`. The PAM service is the
//! caller's: `hyperstor-mgmt` asks for `hyperstor` (`/etc/pam.d/hyperstor`).
//!
//! `FILE` (installed as `/etc/hyperstor/authd.json`) has glidex-authd's
//! keys: `service_user`, `allow_root`, `allowed_groups`,
//! `failure_delay_ms`, `max_connections`. If it doesn't exist, hyperstor's
//! defaults apply.
//!
//! Exit status: 1 the configuration or the socket can't be used, 2 usage.

use glidex_authd::authenticator::{SystemAccounts, user_uid};
use glidex_authd::config::Config;
use glidex_authd::pam::{Pam, PamAuthenticator};
use glidex_authd::proto::SOCKET_NAME;
use glidex_authd::server::{self, Authd};
use std::path::{Path, PathBuf};
use std::sync::Arc;

const USAGE: &str = "usage: hyperstor-authd --config FILE";
pub const RUN_DIR: &str = "/run/hyperstor-authd";
pub const SERVICE_USER: &str = "hyperstor";
pub const LOGIN_GROUP: &str = "hyperstor-users";

fn die(msg: impl std::fmt::Display) -> ! {
    eprintln!("hyperstor-authd: {msg}");
    std::process::exit(1);
}

/// hyperstor's defaults, then `path` over them if it exists.
pub fn config(path: &Path) -> Result<Config, String> {
    let mut c = if path.exists() {
        Config::load(path)?
    } else {
        Config {
            service_user: SERVICE_USER.to_string(),
            allowed_groups: vec![LOGIN_GROUP.to_string()],
            ..Config::default()
        }
    };
    // glidex's default socket directory is glidex's; ours unless the
    // environment (GLIDEX_AUTHD_RUN_DIR) chose one.
    if std::env::var_os(glidex_authd::config::ENV_RUN_DIR).is_none() {
        c.run_dir = PathBuf::from(RUN_DIR);
    }
    Ok(c)
}

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let config_path = match args.as_slice() {
        [flag, path] if flag == "--config" => PathBuf::from(path),
        _ => {
            eprintln!("{USAGE}");
            std::process::exit(2)
        }
    };
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info")),
        )
        .with_target(false)
        .init();
    // Before any thread starts: it edits the environment.
    let activated = glidex_authd::activation::listener();
    let config = config(&config_path).unwrap_or_else(|e| die(e));
    let service_uid = user_uid(&config.service_user);
    if service_uid.is_none() {
        tracing::warn!(user = %config.service_user, "service user does not exist; refusing all peers");
    }
    let pam = Pam::load_default().unwrap_or_else(|e| die(e));
    let listener = match activated {
        Some(Ok(l)) => l,
        Some(Err(e)) => die(format!("socket activation: {e}")),
        None => {
            if let Err(e) = std::fs::create_dir_all(&config.run_dir) {
                die(format!("{}: {e}", config.run_dir.display()));
            }
            // The socket's group is the service user's primary group.
            let gid = nix::unistd::User::from_name(&config.service_user)
                .ok()
                .flatten()
                .map(|u| u.gid.as_raw());
            let path = config.run_dir.join(SOCKET_NAME);
            server::bind(&path, 0o660, gid)
                .unwrap_or_else(|e| die(format!("{}: {e}", path.display())))
        }
    };
    tracing::info!(
        service_user = %config.service_user,
        allowed_groups = ?config.allowed_groups,
        "ready"
    );
    Arc::new(Authd::new(
        config,
        service_uid,
        Arc::new(PamAuthenticator::new(pam)),
        Arc::new(SystemAccounts),
    ))
    .serve(listener);
}

#[cfg(test)]
mod tests {
    #[test]
    fn hyperstors_defaults_without_a_file() {
        let c = super::config(std::path::Path::new("/nonexistent/authd.json")).unwrap();
        assert_eq!(c.service_user, "hyperstor");
        assert_eq!(c.allowed_groups, vec!["hyperstor-users"]);
        assert!(!c.allow_root);
        if std::env::var_os(glidex_authd::config::ENV_RUN_DIR).is_none() {
            assert_eq!(c.run_dir, std::path::Path::new("/run/hyperstor-authd"));
        }
    }

    #[test]
    fn a_file_sets_what_it_names() {
        let d = tempfile::tempdir().unwrap();
        let p = d.path().join("authd.json");
        std::fs::write(
            &p,
            r#"{"service_user":"hyperstor","allowed_groups":["ops"]}"#,
        )
        .unwrap();
        let c = super::config(&p).unwrap();
        assert_eq!(c.allowed_groups, vec!["ops"]);
        std::fs::write(&p, r#"{"typo":1}"#).unwrap();
        assert!(super::config(&p).is_err());
    }
}
