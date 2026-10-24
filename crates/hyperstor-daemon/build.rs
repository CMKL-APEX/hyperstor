//! `HYPERSTOR_DIAG=1 cargo build …` builds a storage node that says what
//! it does on standard error where it otherwise says nothing (the
//! `hs_diag` cfg; `src/diag.rs`): backfill attempts and why they fail, and
//! waits that run long. For finding out why a cluster is stuck; not for
//! production, as it logs a line per event (throttled).

fn main() {
    println!("cargo::rerun-if-env-changed=HYPERSTOR_DIAG");
    println!("cargo::rustc-check-cfg=cfg(hs_diag)");
    if std::env::var("HYPERSTOR_DIAG").is_ok_and(|v| !v.is_empty() && v != "0") {
        println!("cargo::rustc-cfg=hs_diag");
    }
}
