fn main() -> Result<(), Box<dyn std::error::Error>> {
    let proto = "../../proto/home/v1/home.proto";
    println!("cargo:rerun-if-changed={proto}");
    prost_build::Config::new()
        .protoc_executable(protoc_bin_vendored::protoc_bin_path()?)
        .compile_protos(&[proto], &["../../proto"])?;
    Ok(())
}
