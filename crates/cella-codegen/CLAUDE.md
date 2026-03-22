# cella-codegen

- Build-time only — not a runtime dependency
- After any codegen change: `cargo insta review`
- Output is `include!()`'d by cella-config's build.rs
