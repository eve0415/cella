# cella-codegen

> Build-time code generator: JSON Schema to typed Rust structs.

Part of the [cella](../../README.md) workspace.

## Overview

cella-codegen generates typed Rust structs and validators from a JSON Schema string. It is used exclusively at build time by cella-config's `build.rs` to produce the `DevContainer` type and related types from the [devcontainer JSON Schema](https://containers.dev/implementors/json_reference/).

This is not a runtime dependency. It runs during `cargo build` and produces formatted Rust source suitable for `include!()`.

### Pipeline

```
JSON Schema string
    -> parse (extract types, resolve $ref)
    -> lower to IR (intermediate representation)
    -> emit tokens (syn/quote)
    -> format (prettyplease)
    -> Rust source string
```

## Architecture

### Key Types

- `CodegenConfig` — controls root type name, doc emission, and deprecated field handling
- `generate(schema_json, config) -> Result<String>` — the sole public entry point

### Modules

| Module | Purpose |
|--------|---------|
| `schema/parse` | Parses root JSON Schema into internal representation (including inline `$ref` resolution) |
| `ir/lower/` | Lowers parsed schema into an intermediate type representation (submodules: `primitives`, `composite`) |
| `ir/naming` | Naming conventions for generated types (case conversion, deduplication) |
| `emit/types` | Emits Rust type definitions as token streams |
| `emit/validate` | Emits validation logic for schema constraints |
| `emit/format` | Formats token streams via prettyplease |
| `error` | `CellaCodegenError` type |

## Crate Dependencies

**Depends on:** none (standalone; uses serde_json, syn, quote, proc-macro2, prettyplease, heck, indexmap)

**Depended on by:** [cella-config](../cella-config) (as a build dependency in `[build-dependencies]`)

## Testing

```sh
cargo test -p cella-codegen
```

Tests use snapshot assertions via insta. The main test generates the full devcontainer schema and compares against a stored snapshot. After any codegen change:

```sh
cargo insta review
```

## Development

Changes to this crate directly affect the config API used throughout the entire workspace. The generated output is `include!()`'d by cella-config, so a codegen change can break downstream compilation.

The pipeline is designed to be extended: to support additional JSON Schema features, add handling in the parse/resolve phase, lower it to an IR type, then emit the corresponding Rust tokens. Each stage is independent and testable.
