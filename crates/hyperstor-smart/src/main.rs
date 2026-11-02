//! `hyperstor-smart`: the root helper that reads NVMe drives' SMART /
//! health logs for `hyperstord` (27-performance.md §30.3, D218), which runs
//! as the unprivileged `hyperstor` user while the log needs
//! `CAP_SYS_ADMIN`.
//!
//! Socket-activated, one process per connection (`Accept=yes`), with
//! `--serve`: it reads
//! one line, `health PATH`, from standard input and answers one line on
//! standard output, `ok HEX WCTEMP CCTEMP` (the 512-byte log in hex, the
//! controller's warning and critical temperatures in kelvin) or `error
//! WHY`. `PATH` must be a device a storage node of this host is
//! configured with (`--shard ...=PATH:SIZE` in `/etc/hyperstor/node-*.env`)
//! and an NVMe namespace: it reads nothing else, and writes nothing.
//!
//! By hand, as root: `hyperstor-smart health PATH`.

mod nvme;

use std::io::{BufRead, Read as _, Write};
use std::path::{Path, PathBuf};

const USAGE: &str = "usage: hyperstor-smart [--config-dir DIR] --serve|health PATH";

/// The device paths the storage nodes' options name.
fn configured(dir: &Path) -> Vec<String> {
    let mut out = Vec::new();
    let Ok(rd) = std::fs::read_dir(dir) else {
        return out;
    };
    for e in rd.flatten() {
        let name = e.file_name().to_string_lossy().into_owned();
        if !(name.starts_with("node-") && name.ends_with(".env")) {
            continue;
        }
        let Ok(text) = std::fs::read_to_string(e.path()) else {
            continue;
        };
        out.extend(shard_paths(&text));
    }
    out
}

/// `--shard ADDR=PATH:SIZE` arguments' paths.
fn shard_paths(text: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut words = text.split_whitespace();
    while let Some(w) = words.next() {
        if w == "--shard"
            && let Some(spec) = words.next()
            && let Some((_, rest)) = spec.split_once('=')
            && let Some((path, _)) = rest.rsplit_once(':')
        {
            out.push(path.to_string());
        }
    }
    out
}

/// Whether `path` is an NVMe namespace's block device.
fn is_nvme_namespace(path: &Path) -> bool {
    use std::os::unix::fs::FileTypeExt;
    let block = std::fs::metadata(path).is_ok_and(|m| m.file_type().is_block_device());
    let name = std::fs::canonicalize(path)
        .ok()
        .and_then(|p| p.file_name().map(|n| n.to_string_lossy().into_owned()))
        .unwrap_or_default();
    // nvme<ctrl>n<ns>, not a partition (…p1).
    let ns = name
        .strip_prefix("nvme")
        .and_then(|r| r.split_once('n'))
        .is_some_and(|(c, n)| {
            !c.is_empty()
                && c.bytes().all(|b| b.is_ascii_digit())
                && !n.is_empty()
                && n.bytes().all(|b| b.is_ascii_digit())
        });
    block && ns
}

/// The answer to one request line.
fn answer(line: &str, allowed: &[String]) -> String {
    let Some(path) = line.trim().strip_prefix("health ") else {
        return "error the request is health PATH".into();
    };
    if !allowed.iter().any(|a| a == path) {
        return format!("error {path} isn't a device of this host's storage nodes");
    }
    let p = PathBuf::from(path);
    if !is_nvme_namespace(&p) {
        return format!("error {path} isn't an NVMe namespace");
    }
    match nvme::health(&p) {
        Ok((log, wc, cc)) => {
            let hex: String = log.iter().map(|b| format!("{b:02x}")).collect();
            format!("ok {hex} {wc} {cc}")
        }
        Err(e) => format!("error {path}: {e}"),
    }
}

fn main() {
    let mut dir = PathBuf::from("/etc/hyperstor");
    let mut by_hand: Option<String> = None;
    let mut serve = false;
    let mut args = std::env::args().skip(1);
    while let Some(a) = args.next() {
        match a.as_str() {
            "--config-dir" => dir = args.next().map(PathBuf::from).unwrap_or_else(|| usage()),
            "health" => by_hand = Some(args.next().unwrap_or_else(|| usage())),
            "--serve" => serve = true,
            "--help" | "-h" => {
                println!("{USAGE}");
                return;
            }
            _ => usage(),
        }
    }
    if serve == by_hand.is_some() {
        usage();
    }
    let allowed = configured(&dir);
    let line = match by_hand {
        Some(p) => format!("health {p}"),
        None => {
            let mut l = String::new();
            // One short line; a peer that sends more is cut off.
            let _ = std::io::stdin().lock().take(4096).read_line(&mut l);
            l
        }
    };
    let mut out = std::io::stdout().lock();
    let _ = writeln!(out, "{}", answer(&line, &allowed));
}

fn usage() -> ! {
    eprintln!("{USAGE}");
    std::process::exit(2)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_nodes_options_name_the_devices() {
        let env = "HS_ARGS=--node 0 --meta 10.0.0.1:8849 --shard 10.0.0.1:9100=/var/lib/hyperstor/node-0-0.img:64 \
                   --shard 10.0.0.1:9102=/dev/disk/by-id/nvme-X_Y:0 --admin 10.0.0.1:8853\n";
        assert_eq!(
            shard_paths(env),
            [
                "/var/lib/hyperstor/node-0-0.img",
                "/dev/disk/by-id/nvme-X_Y"
            ]
        );
    }

    #[test]
    fn only_configured_nvme_namespaces_are_read() {
        let allowed = vec!["/var/lib/x.img".to_string()];
        assert!(
            answer("health /dev/nvme0n1", &allowed)
                .starts_with("error /dev/nvme0n1 isn't a device")
        );
        assert!(answer("health /var/lib/x.img", &allowed).ends_with("isn't an NVMe namespace"));
        assert!(answer("smart /dev/nvme0n1", &allowed).starts_with("error the request"));
        assert!(!is_nvme_namespace(Path::new("/nonexistent/nvme0n1")));
    }
}
