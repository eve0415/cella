# cella-config

JSONC parsing, devcontainer config discovery, validation, and layer merging.

## Build

`build.rs` generates typed structs from the devcontainer JSON Schema via `cella-codegen`.

## Config merge order

global (~/.config/cella/global.jsonc) → workspace (devcontainer.json) → local (devcontainer.local.jsonc)

## Key internals

- **JSONC preprocessor**: single-pass state machine, preserves byte offsets for diagnostics
- **Diagnostics**: source-positioned errors via `miette` spans
