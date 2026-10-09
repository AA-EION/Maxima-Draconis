fn main() -> std::io::Result<()> {
    prost_build::compile_protos(&["src/rtm/proto/rtm.proto"], &["src/rtm/proto/"])?;

    // Messages only: the gRPC service itself is driven by hand in
    // src/presence/grpc.rs, so no tonic-build (and no second prost) is needed.
    prost_build::compile_protos(
        &["src/presence/proto/eadp/social/presence/v1/presence_service.proto"],
        &["src/presence/proto/"],
    )
}
