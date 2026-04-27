fn main() -> Result<(), Box<dyn std::error::Error>> {
    // Wired up properly in the REAPI vendoring task. For Phase 0 we keep
    // build.rs present so the crate compiles cleanly and so downstream
    // changes touch a single file.
    println!("cargo:rerun-if-changed=protos");
    Ok(())
}
