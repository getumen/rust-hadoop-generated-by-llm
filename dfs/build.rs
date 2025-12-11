fn main() -> Result<(), Box<dyn std::error::Error>> {
    // Try using tonic_build::compile_protos directly
    // This may require specific features to be enabled
    tonic_prost_build::configure()
        .type_attribute(".", "#[derive(serde::Serialize, serde::Deserialize)]")
        .compile_protos(&["../proto/dfs.proto"], &["../proto"])?;
    Ok(())
}
