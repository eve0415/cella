use std::{env, fs, path::Path};

fn main() {
    let schema_path = Path::new("schemas/devContainer.base.schema.json");
    println!("cargo::rerun-if-changed={}", schema_path.display());

    let schema_content = fs::read_to_string(schema_path).expect("failed to read schema file");

    let out_dir = env::var("OUT_DIR").unwrap();
    let out_path = Path::new(&out_dir).join("generated.rs");

    let config = cella_codegen::CodegenConfig {
        root_type_name: "DevContainer".to_string(),
        emit_docs: true,
        emit_deprecated: true,
    };

    match cella_codegen::generate(&schema_content, &config) {
        Ok(code) => {
            fs::write(&out_path, code).expect("failed to write generated.rs");
        }
        Err(e) => {
            eprintln!("cargo::warning=codegen failed: {e}");
            fs::write(
                &out_path,
                format!(
                    "// Code generation failed: {e}\n\
                     // This is a build-time issue. Check the schema file and cella-codegen.\n"
                ),
            )
            .expect("failed to write generated.rs");
        }
    }
}
