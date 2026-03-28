# Devcontainer Spec Compliance Audit

Audit of cella against the official devcontainer specification (containers.dev) and reference CLI (devcontainers/cli).

Date: 2026-03-28

## Legend

- PASS: cella matches spec
- FAIL: cella diverges from spec (needs fix)
- MISSING: cella doesn't implement this at all
- PARTIAL: cella partially implements this

---

## 1. CLI Commands

| Command | Spec | Cella | Status |
|---------|------|-------|--------|
| `up` | Create and run dev container | Implemented | PARTIAL (missing ~25 flags) |
| `build` | Build dev container image | Implemented | PARTIAL (missing buildx flags) |
| `exec` | Execute command in container | Implemented | PARTIAL (missing some flags) |
| `read-configuration` | Read resolved config | Implemented | PARTIAL (missing some flags) |
| `set-up` | Set up existing container | Not implemented | MISSING |
| `run-user-commands` | Run lifecycle commands | Not implemented | MISSING |
| `outdated` | Show feature versions | Not implemented | MISSING |
| `upgrade` | Upgrade feature lockfile | Not implemented | MISSING |
| `features test` | Test features | Not implemented | MISSING |
| `features package` | Package features | Not implemented | MISSING |
| `features publish` | Publish features | Not implemented | MISSING |
| `features info` | Feature metadata | Not implemented | MISSING |
| `features resolve-dependencies` | Resolve deps | Not implemented | MISSING |
| `features generate-docs` | Generate docs | Not implemented | MISSING |
| `templates apply` | Apply template | Partially via `init` | PARTIAL |
| `templates publish` | Publish templates | Not implemented | MISSING |
| `templates metadata` | Template metadata | Not implemented | MISSING |
| `templates generate-docs` | Generate docs | Not implemented | MISSING |

### Cella-Specific Commands (beyond spec, keep as-is)
`shell`, `list`, `logs`, `doctor`, `branch`, `switch`, `prune`, `nvim`, `ports`, `credential`, `init`, `config`, `down`, `daemon`

---

## 2. CLI Flags

### `up` Command Flags

| Flag | Spec Default | Cella | Status |
|------|-------------|-------|--------|
| `--workspace-folder` | cwd | Has it | PASS |
| `--config` | - | Has as `--file` | FAIL (wrong name) |
| `--override-config` | - | Not implemented | MISSING |
| `--id-label` (repeatable) | - | Not implemented | MISSING |
| `--docker-path` | - | Not implemented | MISSING |
| `--docker-compose-path` | - | Not implemented | MISSING |
| `--container-data-folder` | - | Not implemented | MISSING |
| `--container-system-data-folder` | - | Not implemented | MISSING |
| `--workspace-mount-consistency` | `cached` | Not implemented | MISSING |
| `--gpu-availability` | `detect` | Not implemented | MISSING |
| `--mount-workspace-git-root` | `true` | Not implemented | MISSING |
| `--mount-git-worktree-common-dir` | `false` | Not implemented | MISSING |
| `--log-level` | `info` | Has as `--verbose` | PARTIAL |
| `--log-format` | `text` | Not implemented | MISSING |
| `--terminal-columns/rows` | - | Not implemented | MISSING |
| `--default-user-env-probe` | `loginInteractiveShell` | Not implemented | MISSING |
| `--update-remote-user-uid-default` | `on` | Not implemented | MISSING |
| `--remove-existing-container` | `false` | Has as `--remove-existing-container` | PASS |
| `--build-no-cache` | `false` | Has as `--build-no-cache` | PASS |
| `--expect-existing-container` | `false` | Not implemented | MISSING |
| `--skip-post-create` | `false` | Not implemented | MISSING |
| `--skip-non-blocking-commands` | `false` | Not implemented | MISSING |
| `--prebuild` | `false` | Not implemented | MISSING |
| `--user-data-folder` | - | Not implemented | MISSING |
| `--mount` (repeatable) | - | Not implemented | MISSING |
| `--remote-env` (repeatable) | - | Not on `up` | MISSING |
| `--cache-from` | - | Not implemented | MISSING |
| `--cache-to` | - | Not implemented | MISSING |
| `--buildkit` | `auto` | Not implemented | MISSING |
| `--additional-features` | - | Not implemented | MISSING |
| `--skip-post-attach` | `false` | Not implemented | MISSING |
| `--dotfiles-repository` | - | Not implemented | MISSING |
| `--dotfiles-install-command` | - | Not implemented | MISSING |
| `--dotfiles-target-path` | `~/dotfiles` | Not implemented | MISSING |
| `--container-session-data-folder` | - | Not implemented | MISSING |
| `--secrets-file` | - | Not implemented | MISSING |
| `--include-configuration` | `false` | Not implemented | MISSING |
| `--include-merged-configuration` | `false` | Not implemented | MISSING |

### `build` Command Flags

| Flag | Spec | Cella | Status |
|------|------|-------|--------|
| `--no-cache` | `false` | Has it | PASS |
| `--image-name` (repeatable) | - | Not implemented | MISSING |
| `--platform` | - | Not implemented | MISSING |
| `--push` | `false` | Not implemented | MISSING |
| `--label` (repeatable) | - | Not implemented | MISSING |
| `--output` | - | Not implemented | MISSING |
| `--cache-from` | - | Not implemented | MISSING |
| `--cache-to` | - | Not implemented | MISSING |
| `--buildkit` | `auto` | Not implemented | MISSING |
| `--additional-features` | - | Not implemented | MISSING |

---

## 3. Behavioral Divergences

### 3.1 devcontainerId Computation
- **Spec**: SHA-256 of sorted JSON label object, base-32 encoded, left-padded to 52 chars
- **Cella**: SHA-256 of workspace path
- **Status**: FAIL
- **Impact**: Variable substitution `${devcontainerId}` produces wrong value; container identity differs

### 3.2 Docker Compose Defaults
- **Spec**: `overrideCommand` defaults `false`, `shutdownAction` defaults `stopCompose`, `workspaceFolder` defaults `"/"`
- **Cella**: Needs audit -- may use image/Dockerfile defaults for compose mode
- **Status**: NEEDS AUDIT
- **Impact**: Compose containers may have wrong command override and shutdown behavior

### 3.3 updateRemoteUserUID Timing
- **Spec**: Build-time (Dockerfile layer before container creation)
- **Cella**: Runtime (exec after container creation)
- **Status**: FAIL
- **Impact**: First container start has wrong UID until exec completes; image not cached with correct UID

### 3.4 Lifecycle Failure Cascading
- **Spec**: If any phase fails, ALL subsequent phases are skipped
- **Cella**: Needs verification
- **Status**: NEEDS AUDIT
- **Impact**: Subsequent lifecycle commands may run after a failure

### 3.5 Parallel Command Failure
- **Spec**: All parallel commands must succeed; implementations should cancel siblings on failure
- **Cella**: Needs verification
- **Status**: NEEDS AUDIT
- **Impact**: Sibling commands may continue running after one fails

### 3.6 JSON Output Format
- **Spec**: stdout = JSON only, stderr = logs only
- **Cella**: `--output text|json` flag controls format
- **Status**: FAIL
- **Impact**: Tools parsing stdout may get logs mixed with JSON

### 3.7 containerEnv vs remoteEnv
- **Spec**: `containerEnv` at Docker create (immutable), `remoteEnv` per-process
- **Cella**: Needs audit for correct separation
- **Status**: NEEDS AUDIT
- **Impact**: Environment variable lifecycle may be incorrect

### 3.8 Feature dependsOn
- **Spec**: Hard recursive dependency resolution with auto-pull
- **Cella**: Only `installsAfter` (soft, non-recursive)
- **Status**: MISSING
- **Impact**: Features with hard dependencies may not install correctly

### 3.9 Feature Option Validation
- **Spec**: Unknown options rejected, enum values validated
- **Cella**: Needs audit
- **Status**: NEEDS AUDIT
- **Impact**: Typos in feature options silently ignored

### 3.10 Container Labels
- **Spec**: `devcontainer.local_folder`, `devcontainer.config_file`, etc.
- **Cella**: `dev.cella.*` labels
- **Status**: FAIL
- **Impact**: VS Code cannot discover cella-created containers

### 3.11 Config Discovery Flag
- **Spec**: `--config`
- **Cella**: `--file`
- **Status**: FAIL
- **Impact**: Tools using `--config` flag fail

### 3.12 hostRequirements Merge
- **Spec**: Maximum value wins across metadata layers
- **Cella**: Needs audit
- **Status**: NEEDS AUDIT

### 3.13 Port String Format
- **Spec**: Supports `"host:container"` strings in `forwardPorts`
- **Cella**: Numeric only
- **Status**: PARTIAL

### 3.14 initializeCommand Re-run
- **Spec**: Runs during initialization including subsequent starts
- **Cella**: Needs verification for restart behavior
- **Status**: NEEDS AUDIT

---

## 4. Missing Features

| Feature | Description | Priority |
|---------|-------------|----------|
| Dotfiles | `--dotfiles-*` flags + config.toml support | High |
| Secrets | `--secrets-file` with phase restrictions | High |
| Additional features | `--additional-features` + config.toml defaults | High |
| Feature lockfile | `.devcontainer/devcontainer-lock.json` | Medium |
| Feature dependsOn | Hard recursive dependency resolution | High |
| Buildx support | `--platform`, `--push`, `--output` | Medium |
| set-up command | Devcontainerize existing container | High |
| run-user-commands | Re-run lifecycle commands | High |
| outdated command | Feature version checking | Medium |
| upgrade command | Feature lockfile update | Medium |
| Feature authoring | test/package/publish/info/resolve-deps/generate-docs | Low |
| Template commands | apply/publish/metadata/generate-docs | Low |
| Host requirements | CPU/memory/storage/GPU validation with warnings | Medium |
| Git root mount | `--mount-workspace-git-root` | Medium |

---

## 5. Design Decisions

| Decision | Choice | Rationale |
|----------|--------|-----------|
| devcontainerId migration | No migration | No prior releases |
| Lifecycle failure | Kill siblings on first failure | Fail-fast, spec spirit |
| Dotfiles config | CLI flags + config.toml | Configure once, apply everywhere |
| Secrets handling | Inject at exec time only | Never persist sensitive data |
| Build approach | Shell out to buildx for advanced, bollard for simple | Buildx CLI is the stable interface |
| Command structure | Shared lifecycle engine | set-up and run-user-commands share logic with up |
| Feature lockfile | Stable from day one | No experimental flags needed |
| Container naming | `cella-<basename>-<hash>` with reference CLI labels | Own identity + interop via labels |
| Error output | JSON to stdout, miette to stderr | Tools parse stdout, humans read stderr |
| Host requirements | Warn, don't block | Informational, let Docker handle limits |
| Override config | Support both --override-config AND local.jsonc | Maximum flexibility |
| Extensions config | customizations.cella + cella.toml + config.toml | Spec-standard + cella-specific paths |
| Test strategy | Spec-compliance suite + unit tests, test-first | Write all tests first (failing), then implement |
| Implementation order | Test-first per phase | TDD: spec tests -> correctness -> compat -> features -> polish |
| Clippy/unused vars | Fix properly, never suppress | No `_` prefixes, no `#[allow(...)]` |
| `init` command | Wraps `templates apply` | User-friendly alias over spec plumbing |
| Multi-config repos | Error + list configs, prompt for `--config` | Non-interactive: first alphabetically |
| customizations.cella merge | Deep merge, last wins per key | Consistent with env var merge strategy |
| Default additional feature | `ghcr.io/devcontainers/features/github-cli:1` | Opt-out via config |
