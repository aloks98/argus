//! Generated gRPC contract for the Argus agent protocol. Source of truth is
//! `proto/argus.proto`; `build.rs` regenerates this at compile time (no
//! `protoc` required).

// This crate is almost entirely tonic/prost-generated code; don't hold generated
// doc comments to clippy's doc-formatting lints.
#![allow(clippy::doc_lazy_continuation)]

pub mod v1 {
    tonic::include_proto!("argus.v1");
}

pub use v1::*;
