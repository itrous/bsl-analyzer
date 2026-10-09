use std::{fs, path::PathBuf};

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let docs = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../docs/mcp/check-contract/v1");
    fs::create_dir_all(&docs)?;
    for (name, schema) in [
        ("request.schema.json", check_contract::request_schema()),
        ("response.schema.json", check_contract::response_schema()),
    ] {
        fs::write(docs.join(name), format!("{}\n", serde_json::to_string_pretty(&schema)?))?;
    }
    Ok(())
}
