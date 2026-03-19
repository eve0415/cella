use std::env;
use std::fmt::Write;
use std::process::Command;

fn main() {
    let manifest_dir = env::var("CARGO_MANIFEST_DIR").unwrap();
    let out_dir = env::var("OUT_DIR").unwrap();
    let src = format!("{manifest_dir}/../cella-tunnel-server/src/main.rs");

    println!("cargo::rerun-if-changed={src}");

    let targets = [
        ("x86_64-unknown-linux-musl", "X86_64"),
        ("aarch64-unknown-linux-musl", "AARCH64"),
    ];

    let mut consts = String::new();
    for (triple, const_name) in &targets {
        let out_bin = format!("{out_dir}/tunnel-server-{}", const_name.to_lowercase());

        // Try cross-compilation with rustc + rust-lld
        let ok = Command::new("rustc")
            .args([
                "--edition",
                "2024",
                "--crate-type",
                "bin",
                "--target",
                triple,
                "-C",
                "opt-level=z",
                "-C",
                "strip=symbols",
                "-C",
                "target-feature=+crt-static",
                "-C",
                "linker=rust-lld",
                "-o",
                &out_bin,
                &src,
            ])
            .status()
            .is_ok_and(|s| s.success());

        if ok {
            writeln!(
                consts,
                "pub const TUNNEL_SERVER_{const_name}: &[u8] = include_bytes!(\"{out_bin}\");"
            )
            .unwrap();
        } else {
            println!("cargo::warning=cross-compile failed for {triple} (target not installed?)");
            writeln!(consts, "pub const TUNNEL_SERVER_{const_name}: &[u8] = &[];").unwrap();
        }
    }

    std::fs::write(format!("{out_dir}/tunnel_binaries.rs"), consts).unwrap();
}
