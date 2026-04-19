# cella-jsonc

> JSONC (JSON with Comments) preprocessor that strips comments and trailing commas while preserving byte offsets.

Part of the [cella](../../README.md) workspace.

## Overview

cella-jsonc converts JSONC — JSON extended with `//` line comments, `/* */` block comments, and trailing commas — into strict JSON that any `serde_json` parser can consume. It is used to read `devcontainer.json`, template manifests, and cella's own configuration files, all of which permit JSONC syntax per the [Dev Container specification](https://containers.dev/implementors/json_reference/).

The crate is a single-pass state machine. Comments are replaced by space characters (and newlines are preserved inside block comments), and trailing commas before `]` or `}` are replaced by a single space. The key invariant is `output.len() == input.len()`: byte offsets in the stripped output map one-to-one to offsets in the original source, so downstream parsers (`serde_json`, miette diagnostics) can report errors pointing back to the original JSONC.

## Architecture

### Key Types

- `strip(input: &str) -> Result<String, Error>` — the sole public entry point. Converts JSONC input into strict JSON with offsets preserved
- `Error` — error type with `message` and byte `offset`. Returned for unterminated block comments and for inputs that produce invalid UTF-8 (cannot happen for valid UTF-8 input)

### State Machine

Four states cover every byte:

| State | Behavior |
|-------|----------|
| `Normal` | Default. `"` enters `InString`, `//` enters `LineComment`, `/*` enters `BlockComment`, trailing `,` before `]`/`}` is rewritten to space |
| `InString` | Passes bytes through verbatim. Handles `\"` escapes. `"` returns to `Normal` |
| `LineComment` | Replaces bytes with spaces until `\n`, which is preserved and returns to `Normal` |
| `BlockComment` | Replaces bytes with spaces, preserves `\n`, and returns to `Normal` on `*/` |

Nested block comments are not supported (per the JSONC convention): the first `*/` always ends the comment.

## Crate Dependencies

**Depends on:** none (std only; `serde_json` is a dev-dependency for tests)

**Depended on by:** [cella-cli](../cella-cli), [cella-config](../cella-config), [cella-templates](../cella-templates)

## Testing

```sh
cargo test -p cella-jsonc
```

Unit tests cover line comments, block comments, trailing commas (object and array, nested), string contents that contain comment-like sequences, escaped quotes, multi-line block comments, consecutive comments, unterminated block comments, and the byte-length invariant (`output.len() == input.len()`) on every positive case.

## Development

The invariant `output.len() == input.len()` is enforced by `debug_assert_eq!` in debug builds and by every test. Any change that rewrites byte counts must update callers that rely on offset-preservation (source-positioned miette diagnostics in cella-config, template variable substitution in cella-templates).
