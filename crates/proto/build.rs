//! Generates gRPC bindings at build time via `protox` (a pure-Rust protobuf
//! compiler, so no external `protoc` binary is required) piped into
//! `tonic-prost-build` for client + server codegen.

fn main() -> Result<(), Box<dyn std::error::Error>> {
    println!("cargo:rerun-if-changed=proto/argus.proto");

    let file_descriptors = protox::compile(["proto/argus.proto"], ["proto"])?;

    tonic_prost_build::configure()
        .build_server(true)
        .build_client(true)
        .compile_fds(file_descriptors)?;

    Ok(())
}
