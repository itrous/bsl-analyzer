use ide::config_finder;
use std::path::PathBuf;

#[test]
#[ignore]
fn test_load_doc3_metadata() {
    let Some(home) = std::env::var_os("HOME").or_else(|| std::env::var_os("USERPROFILE")) else {
        println!("home directory not found, skipping test");
        return;
    };
    let doc3_root = PathBuf::from(home).join("src/doc3");

    if !doc3_root.exists() {
        println!("doc3 project not found at {:?}, skipping test", doc3_root);
        return;
    }

    let config_path = config_finder::find_configuration_path(&doc3_root)
        .expect("Failed to find configuration in doc3");

    println!("Found configuration at: {:?}", config_path);

    let config =
        bsl_metadata::load_from_directory(&config_path).expect("Failed to load doc3 metadata");

    let metadata_objects = config.metadata_objects();

    println!("Loaded {} metadata objects", metadata_objects.len());

    let catalogs: Vec<_> = metadata_objects
        .iter()
        .filter(|obj| obj.mdo_type == bsl_metadata::MdoType::Catalog)
        .collect();

    let documents: Vec<_> = metadata_objects
        .iter()
        .filter(|obj| obj.mdo_type == bsl_metadata::MdoType::Document)
        .collect();

    let registers: Vec<_> = metadata_objects
        .iter()
        .filter(|obj| obj.mdo_type == bsl_metadata::MdoType::InformationRegister)
        .collect();

    println!("Catalogs: {}", catalogs.len());
    println!("Documents: {}", documents.len());
    println!("Registers: {}", registers.len());

    println!("\nFirst 5 catalogs:");
    for catalog in catalogs.iter().take(5) {
        println!("  - {}", catalog.name);
    }

    println!("\nFirst 5 documents:");
    for doc in documents.iter().take(5) {
        println!("  - {}", doc.name);
    }

    assert!(!metadata_objects.is_empty(), "Should have metadata objects");
    assert!(!catalogs.is_empty(), "Should have catalogs");
    assert!(!documents.is_empty(), "Should have documents");
}
