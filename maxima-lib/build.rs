fn main() -> std::io::Result<()> {
    prost_build::compile_protos(&["src/rtm/proto/rtm.proto"], &["src/rtm/proto/"])?;
    tonic_build::configure().build_client(true).compile_protos(
        &[
            "src/social/proto/eadp/social/presence/v1/presence_service.proto",
        ],
        &["src/social/proto/"],
    )?;

    Ok(())
}
