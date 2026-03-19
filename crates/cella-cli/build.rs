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

        // Watch the output binary: if it doesn't exist (previous compilation
        // failed), cargo re-runs build.rs on the next build, giving us a
        // chance to succeed after the user installs musl targets.
        println!("cargo::rerun-if-changed={out_bin}");

        // Try cross-compilation with rustc + rust-lld
        let output = Command::new("rustc")
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
            .output();

        match output {
            Ok(o) if o.status.success() => {
                writeln!(
                    consts,
                    "pub const TUNNEL_SERVER_{const_name}: &[u8] = include_bytes!(\"{out_bin}\");"
                )
                .unwrap();
            }
            Ok(o) => {
                let stderr = String::from_utf8_lossy(&o.stderr);
                // Show first line of error for diagnostics
                let first_line = stderr.lines().next().unwrap_or("unknown error");
                println!("cargo::warning=cross-compile {triple}: {first_line}");
                writeln!(consts, "pub const TUNNEL_SERVER_{const_name}: &[u8] = &[];").unwrap();
            }
            Err(e) => {
                println!("cargo::warning=cross-compile {triple}: {e}");
                writeln!(consts, "pub const TUNNEL_SERVER_{const_name}: &[u8] = &[];").unwrap();
            }
        }
    }

    std::fs::write(format!("{out_dir}/tunnel_binaries.rs"), consts).unwrap();
}
