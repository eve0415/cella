# Design: `cella features` Subcommand + Init Gaps

**Date:** 2026-04-01
**Branch:** feat/init
**Delivery:** Single PR, tiny atomic commits

---

## Context

The `feat/init` branch implements `cella init` with interactive wizard and non-interactive mode, OCI template/feature fetching with 24h cache, option substitution, feature merging, and JSONC generation (2,900+ LOC, 65+ tests). However:

1. **No post-creation feature editing** — users can't add/remove/modify features after init
2. **Several init gaps** — `optionalPaths` unused, no `--output-format` flag, no `--workspace-folder`, no search filter, feature cache fallback missing, `cella up` stubbed

This spec covers both the new `cella features` subcommand and all init gap fixes.

---

## 1. `cella features` Subcommand

### 1.1 Command Structure

```
cella features
  edit    [--add <OCI_REF>]... [--remove <REF>]... [--set-option REF=KEY=VALUE]...
          [-f/--file <PATH>] [-w/--workspace-folder <PATH>] [--registry <REG>]
  list    [--available] [--json] [--refresh]
          [-f/--file <PATH>] [-w/--workspace-folder <PATH>] [--registry <REG>]
  update  [--yes] [--check] [--json]
          [-f/--file <PATH>] [-w/--workspace-folder <PATH>] [--registry <REG>]
```

### 1.2 `cella features edit`

**Interactive mode** (no `--add/--remove/--set-option` flags):
1. Discover devcontainer.json via `-f` flag or `cella_config::discover::config()`
2. Read raw JSONC content
3. Parse features object, fetch display names from OCI metadata (graceful fallback to raw ref)
4. Loop:
   - Show current features with names and options
   - Prompt: `Add feature / Remove feature / Edit options / Done`
   - **Add**: fetch collection, prompt selection (with fuzzy search), prompt options
   - **Remove**: select from current features
   - **Edit options**: select feature, prompt options with current values as defaults; fall back to text input if metadata fetch fails
5. Apply all accumulated edits via `jsonc-parser` (comment-preserving)
6. Write modified JSONC to config path

**Non-interactive mode** (when any `--add/--remove/--set-option` flags present):
1. Discover config, read raw JSONC
2. Parse `--add` flags → `FeatureEdit::Add` per ref
3. Parse `--remove` flags → match ref (full OCI ref or short ID) → `FeatureEdit::Remove`
4. Parse `--set-option` flags (`REF=KEY=VALUE`) → match ref → `FeatureEdit::SetOption`
5. Apply edits, write file

**Error: no config** → `"No devcontainer.json found. Run 'cella init' first."`

**Short ID matching**: reuse `rsplit('/').next().split(':').next()` pattern from `noninteractive.rs:103-107`. Error on zero matches. Error on ambiguous matches listing all candidates.

### 1.3 `cella features list`

**Default (configured features)**:
```
$ cella features list
Configured features:
  ghcr.io/.../node:1      Node.js      version=lts
  ghcr.io/.../python:1    Python       version=3.12
```

**`--available` (registry browse)**:
```
$ cella features list --available
Available features:
  Node.js      Installs Node.js...   ghcr.io/.../node:1
  Python       Installs Python...    ghcr.io/.../python:1
```

**`--json`**: serialize to JSON array for machine consumption.

### 1.4 `cella features update`

1. Parse configured features' OCI references
2. For each, query OCI registry `/v2/<repo>/tags/list` for available tags
3. Compare current tag against available versions
4. Show table: `node:1 → 1.7.1, node:2 available`
5. Prompt user to select which to update (or `--yes` to auto-apply, `--check` to only report)
6. Apply updates as Remove + Add edits via `jsonc-parser`

### 1.5 JSONC Comment-Preserving Editing

**Library**: `jsonc-parser` v0.32.x (VS Code's JSONC parser ported to Rust)

**Edit operations** (`jsonc_edit.rs`):
```rust
pub enum FeatureEdit {
    Add { reference: String, options: serde_json::Value },
    Remove { reference: String },
    SetOption { reference: String, key: String, value: serde_json::Value },
    ReplaceOptions { reference: String, options: serde_json::Value },
}

pub fn apply_edits(source: &str, edits: &[FeatureEdit]) -> Result<String, ...>;
```

**Behavior**:
- Parse JSONC AST, find/create `"features"` property
- Apply edits as text changes preserving all comments and formatting
- Match indentation from existing AST node positions
- Remove `"features"` property entirely if it becomes empty after removals

### 1.6 Shared Prompt Extraction

Move from `wizard.rs` to `commands/features/prompts.rs`:
- `prompt_single_option(key, opt)` → prompt for one option value
- `prompt_feature_options(feature_id, meta)` → prompt for all feature options

Stays in `wizard.rs` (init-specific):
- `prompt_template_selection`, `prompt_all_options`, `prompt_output_format`, `prompt_feature_loop`

After extraction, `wizard.rs` imports from `super::features::prompts`.

### 1.7 Code Layout

```
crates/cella-cli/src/commands/features/
  mod.rs          FeaturesArgs, FeaturesCommand enum, execute dispatch
  edit.rs         EditArgs, interactive + non-interactive edit
  list.rs         ListArgs, configured + available listing
  update.rs       UpdateArgs, version checking + prompt
  prompts.rs      Shared prompt functions (used by init wizard too)
  jsonc_edit.rs   JSONC comment-preserving edit layer
  resolve.rs      Config discovery, feature metadata resolution, short ID matching
```

**New types** in `resolve.rs`:
```rust
pub struct CommonFeatureFlags {
    pub file: Option<PathBuf>,       // -f/--file
    pub workspace_folder: Option<PathBuf>,  // -w/--workspace-folder
    pub registry: Option<String>,    // --registry
}

pub fn discover_config(flags: &CommonFeatureFlags) -> Result<PathBuf, ...>;
pub fn extract_features(config: &serde_json::Value) -> Vec<(String, serde_json::Value)>;
pub fn match_feature_ref(short_id: &str, refs: &[(String, serde_json::Value)]) -> Option<&str>;
pub async fn resolve_feature_name(reference: &str, cache: &TemplateCache) -> String;
```

---

## 2. Init Gap Fixes

### 2.1 `optionalPaths` Support

- **Wizard**: add multi-select prompt after template options, before feature selection. All paths pre-selected by default. Excluded paths = optional_paths minus user selections.
- **apply.rs**: add `excluded_paths: &[String]` parameter to `apply_template`. Compile to `glob::Pattern`, skip matching files in `copy_and_substitute`.
- **Non-interactive**: pass `&[]` (include all, matching devcontainer CLI behavior).
- **Dependency**: `glob` (already in workspace at 0.3.3).

### 2.2 `--output-format` Flag

- Add `--output-format jsonc|json` to `InitArgs` (default: `jsonc`).
- Use `clap::ValueEnum` derive on a new `ConfigFormat` enum.
- Map to `cella_templates::types::OutputFormat` via `to_template_format()`.
- Non-interactive mode uses the flag; wizard keeps its own interactive prompt.

### 2.3 `--workspace-folder` / `-w` Flag

- Add `-w/--workspace-folder` to `InitArgs`.
- Both wizard and noninteractive call `crate::commands::resolve_workspace_folder()` (already exists at `mod.rs:188`).

### 2.4 Fuzzy Search Filter

- `inquire::Select` in v0.9.x has built-in type-to-filter (substring matching on display string).
- Add `.with_page_size(15)` to template selection (`wizard.rs:148`) and feature selection (`wizard.rs:275`) for better visibility on long lists.
- Apply same to new features edit prompts.

### 2.5 Feature Collection Stale-Cache Fallback

- In `collection.rs`, `fetch_feature_collection`'s `Err` arm: add stale-cache fallback matching `fetch_template_collection`'s pattern.
- Call `cache.get_collection_stale(collection_ref)`, log warning with age, deserialize to `FeatureCollectionIndex`.

### 2.6 Wire `cella up` via Process Exec

- Replace 2 TODOs in `wizard.rs` with `exec_cella_up()` helper.
- On Unix: `std::os::unix::process::CommandExt::exec()` to replace process.
- On non-Unix: `Command::new("cella").arg("up").spawn()?.wait()`.

---

## 3. Dependencies

| Crate | Version | Scope | Reason |
|-------|---------|-------|--------|
| `jsonc-parser` | 0.32.1 | workspace + cella-cli | Comment-preserving JSONC editing |
| `glob` | 0.3.3 | already in workspace | optionalPaths glob matching in cella-templates |
| `reqwest` | 0.13.2 | already in workspace | OCI tag listing for features update |

---

## 4. Error Handling

| Scenario | Behavior |
|----------|----------|
| No devcontainer.json | Error + `hint: run 'cella init'` |
| Ambiguous configs | Error + `hint: use -f/--file` |
| Registry unreachable (name resolution) | Graceful fallback: raw reference as name |
| Registry unreachable (--available/update) | Error with network message |
| Invalid `--set-option` format | Error with expected format |
| Short ID zero matches | Error listing configured features |
| Short ID ambiguous | Error listing all matching refs |
| JSONC parse failure | Error from jsonc-parser |

---

## 5. Testing Strategy

**Unit tests** (inline `#[cfg(test)]`):
- `jsonc_edit.rs`: add/remove/set-option edits, comment preservation, empty features cleanup. Snapshot tests with `insta`.
- `resolve.rs`: `match_feature_ref` exact/short/none/ambiguous, `extract_features`, `discover_config` error messages.
- `list.rs`: table and JSON output formatting (insta snapshots).
- `edit.rs`: flag parsing for `--set-option`.
- `update.rs`: version comparison logic, tag parsing.
- `apply.rs`: new test for `excluded_paths` filtering.

**Not tested**: interactive prompts (inquire). Prompt functions are thin glue; logic is tested.

**Integration tests** (`#[ignore]`): `list --available` and `update --check` against real registry.

---

## 6. Commit Sequence

Each commit compiles and passes all tests independently.

### Phase A: Init gap fixes

1. **`feat: add --workspace-folder flag to cella init`**
   - `init/mod.rs`: add `-w` field
   - `wizard.rs`, `noninteractive.rs`: use `resolve_workspace_folder()`

2. **`feat: add --output-format flag to cella init non-interactive mode`**
   - `init/mod.rs`: add `ConfigFormat` enum + field
   - `noninteractive.rs`: use `args.output_format.to_template_format()`

3. **`fix: add stale-cache fallback to feature collection fetch`**
   - `collection.rs`: match template collection's fallback pattern

4. **`feat: add fuzzy search filter to template and feature selection`**
   - `wizard.rs`: add `.with_page_size(15)` to Select prompts

5. **`feat: implement optionalPaths support in template application`**
   - `apply.rs`: add `excluded_paths` parameter, glob filtering
   - `wizard.rs`: add `prompt_optional_paths()` multi-select
   - `noninteractive.rs`: pass `&[]`
   - `cella-templates/Cargo.toml`: add `glob` dependency

6. **`feat: wire cella up invocation after init via process exec`**
   - `wizard.rs`: add `exec_cella_up()`, replace TODOs

### Phase B: Features subcommand

7. **`refactor: extract shared prompt functions from init wizard`**
   - Create `commands/features/mod.rs` (placeholder)
   - Create `commands/features/prompts.rs` with extracted functions
   - Update `wizard.rs` imports

8. **`feat: add JSONC comment-preserving edit layer`**
   - Add `jsonc-parser` to workspace + cella-cli
   - Create `commands/features/jsonc_edit.rs` with `FeatureEdit` + `apply_edits`
   - Comprehensive unit tests with insta snapshots

9. **`feat: add config discovery and feature resolution helpers`**
   - Create `commands/features/resolve.rs` with `CommonFeatureFlags`, discovery, matching
   - Unit tests

10. **`feat: implement cella features list subcommand`**
    - Create `commands/features/list.rs`
    - Wire into `FeaturesCommand` and `Command` enum
    - Unit tests

11. **`feat: implement cella features edit subcommand`**
    - Create `commands/features/edit.rs`
    - Interactive + non-interactive modes
    - Unit tests

12. **`feat: implement cella features update subcommand`**
    - Create `commands/features/update.rs`
    - Tag listing, version comparison, prompt
    - Unit tests

---

## 7. Verification

After implementation:
1. `cargo check --workspace` — type-check
2. `cargo clippy --workspace --all-targets -- -D warnings -D clippy::all` — lint
3. `cargo test --workspace` — all tests pass
4. `cargo insta review` — accept any new snapshots
5. Manual testing:
   - `cella init` with `-w`, `--output-format json`, optional paths prompt
   - `cella features list` / `cella features list --available` / `cella features list --json`
   - `cella features edit` interactive flow (add, remove, edit options)
   - `cella features edit --add <ref> --remove <ref>` non-interactive
   - `cella features update --check`

---

## Critical Files

| File | Purpose |
|------|---------|
| `crates/cella-cli/src/commands/features/jsonc_edit.rs` | Core JSONC editing |
| `crates/cella-cli/src/commands/features/resolve.rs` | Config discovery, feature matching |
| `crates/cella-cli/src/commands/features/prompts.rs` | Shared interactive prompts |
| `crates/cella-cli/src/commands/features/edit.rs` | Edit subcommand |
| `crates/cella-cli/src/commands/features/list.rs` | List subcommand |
| `crates/cella-cli/src/commands/features/update.rs` | Update subcommand |
| `crates/cella-cli/src/commands/features/mod.rs` | Command routing |
| `crates/cella-cli/src/commands/init/wizard.rs` | Init gap fixes |
| `crates/cella-cli/src/commands/init/noninteractive.rs` | Init gap fixes |
| `crates/cella-cli/src/commands/init/mod.rs` | New CLI flags |
| `crates/cella-templates/src/apply.rs` | optionalPaths support |
| `crates/cella-templates/src/collection.rs` | Cache fallback fix |
| `crates/cella-cli/src/commands/mod.rs` | Wire Features into Command enum |
| `Cargo.toml` (workspace) | Add jsonc-parser dependency |
