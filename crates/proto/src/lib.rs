//! Generated gRPC contract for the Argus agent protocol.
//!
//! The source of truth is `proto/argus.proto`; `build.rs` regenerates this module
//! at compile time via `protox` + `tonic-prost-build` (no `protoc` required).

pub mod v1 {
    tonic::include_proto!("argus.v1");
}

pub use v1::*;
