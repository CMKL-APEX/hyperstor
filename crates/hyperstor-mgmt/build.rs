//! The build mgmt reports in the web UI's footer (19-administration.md
//! §23.7): the version tag on the built commit, else the release it
//! follows and how many commits after it, the branch and the commit, from
//! git at build time. `HYPERSTOR_BUILD_TAG`, `_RELEASE`, `_BRANCH` and
//! `_COMMIT` override them for builds outside a checkout, and
//! `HYPERSTOR_BUILD_REPO` the repository the footer links to. The
//! repository is never read from the checkout's remote, whose URL can
//! carry a credential.

use std::path::Path;
use std::process::Command;

/// `git <args>` in this crate's directory, trimmed; `None` outside a
/// checkout or on failure.
fn git(args: &[&str]) -> Option<String> {
    let out = Command::new("git")
        .args(args)
        .current_dir(env!("CARGO_MANIFEST_DIR"))
        .output()
        .ok()?;
    let s = String::from_utf8(out.stdout).ok()?.trim().to_string();
    (out.status.success() && !s.is_empty()).then_some(s)
}

fn env(name: &str) -> Option<String> {
    println!("cargo:rerun-if-env-changed={name}");
    std::env::var(name).ok().filter(|v| !v.is_empty())
}

fn main() {
    // Built again when HEAD, a branch or a tag moves.
    for (flag, files) in [
        ("--git-dir", &["HEAD"][..]),
        (
            "--git-common-dir",
            &["packed-refs", "refs/heads", "refs/tags"][..],
        ),
    ] {
        if let Some(dir) = git(&["rev-parse", flag]) {
            let dir = Path::new(env!("CARGO_MANIFEST_DIR")).join(dir);
            for f in files {
                let p = dir.join(f);
                if p.exists() {
                    println!("cargo:rerun-if-changed={}", p.display());
                }
            }
        }
    }
    let commit = env("HYPERSTOR_BUILD_COMMIT").or_else(|| git(&["rev-parse", "HEAD"]));
    let tag = env("HYPERSTOR_BUILD_TAG")
        .or_else(|| git(&["describe", "--tags", "--exact-match", "HEAD"]));
    let release = tag
        .clone()
        .or_else(|| env("HYPERSTOR_BUILD_RELEASE"))
        .or_else(|| git(&["describe", "--tags", "--abbrev=0", "HEAD"]));
    let ahead = match (&tag, &release) {
        (None, Some(r)) => git(&["rev-list", "--count", &format!("{r}..HEAD")])
            .and_then(|n| n.parse::<u64>().ok())
            .unwrap_or(0),
        _ => 0,
    };
    // A detached HEAD has no branch.
    let branch = env("HYPERSTOR_BUILD_BRANCH")
        .or_else(|| git(&["rev-parse", "--abbrev-ref", "HEAD"]))
        .filter(|b| b != "HEAD");
    let repo = env("HYPERSTOR_BUILD_REPO")
        .unwrap_or_else(|| "https://github.com/CMKL-APEX/hyperstor".into());
    for (k, v) in [
        ("HS_BUILD_TAG", tag),
        ("HS_BUILD_RELEASE", release),
        ("HS_BUILD_AHEAD", Some(ahead.to_string())),
        ("HS_BUILD_BRANCH", branch),
        ("HS_BUILD_COMMIT", commit),
        (
            "HS_BUILD_REPO",
            Some(repo.trim_end_matches('/').to_string()),
        ),
    ] {
        println!("cargo:rustc-env={k}={}", v.unwrap_or_default());
    }
}
