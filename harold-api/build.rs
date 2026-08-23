fn main() -> Result<(), Box<dyn std::error::Error>> {
    println!("cargo:rerun-if-changed=proto/harold.proto");

    let descriptor_path =
        std::path::PathBuf::from(std::env::var("OUT_DIR")?).join("harold_descriptor.bin");
    tonic_prost_build::configure()
        .file_descriptor_set_path(descriptor_path)
        .compile_protos(&["proto/harold.proto"], &["proto"])?;
    Ok(())
}
