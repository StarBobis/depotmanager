fn main() {
    // Use the vendored protoc so no system install is required.
    let protoc = protoc_bin_vendored::protoc_bin_path().expect("protoc not found");
    std::env::set_var("PROTOC", protoc);

    let mut config = prost_build::Config::new();
    config
        .compile_protos(&["proto/steam.proto"], &["proto/"])
        .expect("failed to compile steam protos");

    tauri_build::build();
}
