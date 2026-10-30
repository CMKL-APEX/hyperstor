//! Compiles the protos with protox (no protoc or buf needed) and generates
//! messages, services and clients with connectrpc-build, as glidex-api does
//! (spec/19-administration.md §23.5).

use std::path::PathBuf;

const FILES: &[&str] = &["hyperstor/v1/cluster.proto", "hyperstor/v1/admin.proto"];

fn main() {
    println!("cargo:rerun-if-changed=proto");
    let mut compiler = protox::Compiler::new(["proto"]).unwrap_or_else(|e| fail(&e.to_string()));
    compiler.include_imports(true).include_source_info(true);
    compiler
        .open_files(FILES)
        .unwrap_or_else(|e| fail(&e.to_string()));
    // JSON names are the proto names (glidex grpc.md G-D11): every field
    // with an underscore must say so.
    for f in compiler.file_descriptor_set().file {
        for m in &f.message_type {
            for fld in &m.field {
                let name = fld.name();
                if name.contains('_') && fld.json_name() != name {
                    fail(&format!(
                        "{}.{name}: add [json_name = \"{name}\"]",
                        m.name()
                    ));
                }
            }
        }
    }
    let out = PathBuf::from(std::env::var("OUT_DIR").unwrap());
    let fds = out.join("hyperstor.fds");
    std::fs::write(&fds, compiler.encode_file_descriptor_set()).unwrap();
    connectrpc_build::Config::new()
        .descriptor_set(&fds)
        .files(FILES)
        .include_file("_connectrpc.rs")
        .emit_rerun_directives(false)
        .compile()
        .unwrap_or_else(|e| fail(&format!("{e:#}")));
}

fn fail(msg: &str) -> ! {
    eprintln!("hyperstor-api: {msg}");
    std::process::exit(1)
}
