//! The `hyperstor.v1` cluster API (spec/administration.md §23.5): messages,
//! service traits and clients generated from `proto/`.
//!
//! Messages are `buffa` types; their JSON is the proto3 JSON mapping with
//! the proto field names, so `curl` and `hyperstorctl --json` see the same
//! shapes (decisions.md D51).

#[allow(clippy::all, unused_imports, dead_code, non_camel_case_types)]
pub mod proto {
    connectrpc::include_generated!();
}

pub use proto::hyperstor::v1::*;
