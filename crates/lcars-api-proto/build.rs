fn main() -> Result<(), Box<dyn std::error::Error>> {
    let protos = [
        "../../proto/lcars/v1/calendar.proto",
        "../../proto/lcars/v1/cameras.proto",
        "../../proto/lcars/v1/reminders.proto",
        "../../proto/lcars/v1/training.proto",
        "../../proto/lcars/v1/weather.proto",
    ];
    for proto in protos {
        println!("cargo:rerun-if-changed={proto}");
    }
    prost_build::Config::new()
        .protoc_executable(protoc_bin_vendored::protoc_bin_path()?)
        .compile_protos(&protos, &["../../proto"])?;
    Ok(())
}
