//! Protobuf codegen for the SFU wire protocol.
//!
//! The RTC stack is core (no `webrtc` feature), so codegen always runs. We parse
//! the vendored `.proto` sources with the pure-Rust `protox` compiler and emit
//! types with `prost-build`'s `compile_fds`, which consumes the descriptor set
//! directly and never shells out to `protoc`.

fn main() {
    generate_sfu_protocol();
}

fn generate_sfu_protocol() {
    // Paths are relative to the `proto/` include root, matching the upstream
    // `import "video/sfu/..."` statements.
    const INCLUDE_ROOT: &str = "proto";
    const PROTOS: [&str; 3] = [
        "video/sfu/models/models.proto",
        "video/sfu/signal_rpc/signal.proto",
        "video/sfu/event/events.proto",
    ];

    println!("cargo:rerun-if-changed={INCLUDE_ROOT}");
    for proto in PROTOS {
        println!("cargo:rerun-if-changed={INCLUDE_ROOT}/{proto}");
    }

    // protox parses and links the .proto files (well-known google.protobuf.*
    // types are bundled) into a FileDescriptorSet, entirely in Rust.
    let file_descriptors = protox::compile(PROTOS, [INCLUDE_ROOT])
        .expect("protox failed to compile the vendored SFU .proto sources");

    // Emit a single include file that reconstructs the package module tree, so
    // cross-package references (signal -> models, event -> models/signal) resolve.
    let mut config = prost_build::Config::new();
    config.include_file("_sfu_proto.rs");

    config
        .compile_fds(file_descriptors)
        .expect("prost-build failed to generate Rust types from the SFU descriptors");
}
