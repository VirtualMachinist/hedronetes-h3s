fn main() -> Result<(), Box<dyn std::error::Error>> {
    // The pinned Cargo dependency carries protoc for each build host; generation
    // does not fetch tools or require an ambient system protoc installation.
    std::env::set_var("PROTOC", protoc_bin_vendored::protoc_bin_path()?);
    tonic_prost_build::configure()
        .build_server(true)
        .generate_default_stubs(true)
        .compile_protos(&["proto/api.proto"], &["proto"])?;
    println!("cargo:rerun-if-changed=proto/api.proto");
    Ok(())
}
