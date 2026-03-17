fn main() {
    prost_build::Config::new()
        .compile_protos(&["proto/changeset.proto", "proto/changelog.proto"], &["proto/"])
        .expect("Failed to compile protobuf files");
}
