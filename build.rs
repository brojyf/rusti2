fn main() -> Result<(), Box<dyn std::error::Error>> {
    tonic_build::compile_protos("proto/rusti2/v1/object_storage.proto")?;
    Ok(())
}
