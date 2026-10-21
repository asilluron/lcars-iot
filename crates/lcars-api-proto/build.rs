fn main() -> Result<(), Box<dyn std::error::Error>> {
    let proto = "../../proto/lcars/v1/calendar.proto";
    println!("cargo:rerun-if-changed={proto}");
    prost_build::Config::new()
        .protoc_executable(protoc_bin_vendored::protoc_bin_path()?)
        .compile_protos(&[proto], &["../../proto"])?;
    Ok(())
}
