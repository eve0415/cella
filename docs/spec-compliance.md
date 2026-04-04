# Devcontainer Spec Compliance Audit

Audit of cella against the official devcontainer specification (containers.dev) and reference CLI (devcontainers/cli).

Date: 2026-04-01

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
| `templates apply` | Apply template | Implemented via `init` | PARTIAL (unknown template options silently accepted) |
| `templates publish` | Publish templates | Not implemented | MISSING |
| `templates metadata` | Template metadata | Not implemented | MISSING |
| `templates generate-docs` | Generate docs | Not implemented | MISSING |

### Cella-Specific Commands (beyond spec, keep as-is)
`shell`, `list`, `logs`, `doctor`, `branch`, `switch`, `prune`, `nvim`, `code`, `tmux`, `ports`, `credential`, `network`, `init`, `config`, `down`, `daemon`, `features edit`, `features list`, `features update`, `completions`

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
- **Cella**: SHA-256 of workspace path (hex-encoded, 64 chars). A spec-compliant `spec_devcontainer_id()` function exists in `cella-config/src/devcontainer/resolve.rs` but is not yet used in the main path
- **Status**: FAIL (spec-compliant function exists but unused)
- **Impact**: Variable substitution `${devcontainerId}` produces wrong value; container identity differs

### 3.2 Docker Compose Defaults
- **Spec**: `overrideCommand` defaults `false`, `shutdownAction` defaults `stopCompose`, `workspaceFolder` defaults `"/"`
- **Cella**: `overrideCommand` defaults `false` (correct), `shutdownAction` defaults `StopCompose` (correct), `workspaceFolder` is required instead of defaulting to `"/"`
- **Status**: PARTIAL
- **Impact**: Missing `workspaceFolder` default may cause errors when config omits it in compose mode

### 3.3 updateRemoteUserUID Timing
- **Spec**: Build-time (Dockerfile layer before container creation)
- **Cella**: Build-time (separate `Dockerfile.uid-remap` layer, matching devcontainer CLI's `updateUID.Dockerfile`)
- **Status**: PASS

### 3.4 Lifecycle Failure Cascading
- **Spec**: If any phase fails, ALL subsequent phases are skipped
- **Cella**: `run_all_lifecycle_phases` propagates errors via `?`, but the default `up` path uses `run_lifecycle_phases_with_wait_for` which backgrounds later phases as a detached shell script. Two problems: (1) failures in backgrounded phases are not propagated or surfaced to the caller, and (2) the background path only handles string-valued lifecycle commands — array/object `postCreateCommand`/`postStartCommand`/`postAttachCommand` forms are silently skipped in background mode.
- **Status**: FAIL
- **Impact**: Default `waitFor` path swallows failures and silently skips non-string lifecycle command shapes

### 3.5 Parallel Command Failure
- **Spec**: All parallel commands must succeed; implementations should cancel siblings on failure
- **Cella**: Object-form lifecycle commands are parsed into `ParsedLifecycle::Parallel` and executed with `join_all`. When one sibling fails, the others are not cancelled — they continue running before the phase is reported as failed.
- **Status**: PARTIAL
- **Impact**: Parallel lifecycle commands (object-form) can leave partial side effects from siblings that continue after a failure

### 3.6 JSON Output Format
- **Spec**: stdout = JSON only, stderr = logs only
- **Cella**: `--output text|json` flag controls format
- **Status**: FAIL
- **Impact**: Tools parsing stdout may get logs mixed with JSON

### 3.7 containerEnv vs remoteEnv
- **Spec**: `containerEnv` at Docker create (immutable), `remoteEnv` per-process
- **Cella**: `containerEnv` mapped to `CreateContainerOptions.env` (Docker create time), `remoteEnv` kept separate for per-process injection via `map_remote_env()`
- **Status**: PASS
- **Impact**: None — matches spec

### 3.8 Feature dependsOn
- **Spec**: Hard recursive dependency resolution with auto-pull
- **Cella**: Only `installsAfter` (soft, non-recursive)
- **Status**: MISSING
- **Impact**: Features with hard dependencies may not install correctly

### 3.9 Feature Option Validation
- **Spec**: Unknown options rejected, enum values validated
- **Cella**: `merge/validation.rs` detects unknown options, type mismatches, and invalid enum values but only emits warnings — invalid options are passed through and never rejected
- **Status**: PARTIAL
- **Impact**: Typos in feature options are logged but silently accepted; spec requires rejection

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
- **Cella**: Validation exists in `host_requirements.rs` but merge strategy across layers not confirmed
- **Status**: NEEDS AUDIT

### 3.13 Port String Format
- **Spec**: Supports `"host:container"` strings in `forwardPorts`
- **Cella**: Numeric only
- **Status**: PARTIAL

### 3.14 initializeCommand Re-run
- **Spec**: Runs during initialization including subsequent starts
- **Cella**: Runs on container creation and rebuild. Skipped when the container is already running (fast path returns before `create_and_start()`). Compose mode runs it on every invocation.
- **Status**: PARTIAL
- **Impact**: Repeated `cella up` on a running container skips host-side initialization; spec requires it on every start

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
