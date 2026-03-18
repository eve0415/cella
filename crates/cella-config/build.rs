use std::{env, fs, panic, path::Path};

fn main() {
    let schema_path = Path::new("schemas/devContainer.base.schema.json");
    println!("cargo::rerun-if-changed={}", schema_path.display());

    let schema_content = fs::read_to_string(schema_path).expect("failed to read schema file");
    let schema: serde_json::Value =
        serde_json::from_str(&schema_content).expect("failed to parse schema JSON");

    let out_dir = env::var("OUT_DIR").unwrap();
    let out_path = Path::new(&out_dir).join("generated.rs");

    // The devcontainer schema (draft 2019-09) uses constructs that typify doesn't
    // fully support yet (mixed-type enums, complex oneOf/allOf). We attempt codegen
    // but fall back gracefully if it fails.
    let result = panic::catch_unwind(|| {
        let mut type_space = typify::TypeSpace::default();
        type_space
            .add_root_schema(
                serde_json::from_value(schema).expect("failed to convert to JSON Schema"),
            )
            .expect("failed to process schema");

        let contents = type_space.to_stream().to_string();
        prettyplease::unparse(
            &syn::parse_file(&contents).expect("typify output was not valid Rust"),
        )
    });

    match result {
        Ok(formatted) => {
            fs::write(&out_path, formatted).expect("failed to write generated.rs");
        }
        Err(_) => {
            eprintln!(
                "cargo::warning=typify could not fully process the devcontainer schema; \
                 generated.rs will be empty. Core types will be defined manually."
            );
            fs::write(
                &out_path,
                "// typify could not process the devcontainer base schema.\n\
                 // The schema uses draft 2019-09 features (mixed-type enums, complex\n\
                 // oneOf/allOf) that are not yet supported. Core types will be defined\n\
                 // manually in this crate as needed.\n",
            )
            .expect("failed to write generated.rs");
        }
    }
}
