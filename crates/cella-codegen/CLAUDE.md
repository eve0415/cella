# cella-codegen

Build-time code generation from the devcontainer JSON Schema. Produces typed Rust structs via `syn`/`quote`/`prettyplease`.

Used as a **build dependency** by `cella-config` (not a runtime crate).

## Testing

Snapshot tests via `insta`. After changing codegen output:

```sh
cargo test -p cella-codegen
cargo insta review
```
