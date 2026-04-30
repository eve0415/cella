# Devcontainer Spec Compliance Audit

Audit of cella against the official devcontainer specification (containers.dev) and reference CLI (devcontainers/cli).

Date: 2026-04-29

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
`shell`, `list`, `logs`, `doctor`, `branch`, `switch`, `prune`, `nvim`, `code`, `tmux`, `ports`, `credential`, `network`, `init`, `config validate`, `down`, `daemon`, `features edit`, `features list`, `features update`, `completions`

### CLI surface reserved for future work (stubs present, not yet implemented)

`cella config show`, `cella config global`, `cella config dotfiles`, `cella config agent`, `cella template new`, `cella template list`, `cella template edit` — these subcommands parse successfully at the CLI layer but return `"not yet implemented"` at runtime. They are kept visible so the flag shape and routing are stable when the implementation lands.

---

## 2. CLI Flags

### `up` Command Flags

| Flag | Spec Default | Cella | Status |
|------|-------------|-------|--------|
| `--workspace-folder` | cwd | Has it | PASS |
| `--config` | - | Has it | PASS |
| `--override-config` | - | Not implemented (use `.devcontainer/devcontainer.local.jsonc` instead) | MISSING |
| `--id-label` (repeatable) | - | Not implemented | MISSING |
| `--docker-path` | - | Not implemented | MISSING |
| `--docker-compose-path` | - | Not implemented | MISSING |
| `--container-data-folder` | - | Not implemented | MISSING |
| `--container-system-data-folder` | - | Not implemented | MISSING |
| `--workspace-mount-consistency` | `cached` | Not implemented | MISSING |
| `--gpu-availability` | `detect` | Not implemented | MISSING |
| `--mount-workspace-git-root` | `true` | Not implemented | MISSING |
| `--mount-git-worktree-common-dir` | `false` | Not implemented | MISSING |
| `--log-level` | `info` | Has as `--verbose` (boolean) | PARTIAL |
| `--log-format` | `text` | Has as `--output` (`text`, `json`) | PARTIAL (name differs) |
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
| `--include-configuration` | `false` | Not implemented (spec places it on `read-configuration`, see `read-configuration` row in §1) | MISSING |
| `--include-merged-configuration` | `false` | Implemented on `read-configuration` (spec location) | PARTIAL (spec docs also allow it on `up`; cella only supports it on `read-configuration`) |

### Cella-specific flags on `up` (not in spec)

These flags are cella-only. They do not conflict with any spec flag name and can be kept.

| Flag | Purpose |
|------|---------|
| `-v`, `--verbose` | Expanded step details in TUI progress |
| `--rebuild` | Force rebuild before start (semantically overlaps spec's `--remove-existing-container` + cache invalidation) |
| `--pull <always\|missing\|never>` | Image pull policy (independent of spec's `--buildkit`/build-cache flags) |
| `--secret` (repeatable) | BuildKit `id=X[,src=Y][,env=Z]` secret for image builds |
| `--backend <docker>` | Container backend selection (docker today; apple-container gated on macOS) |
| `--docker-host` | Override `DOCKER_HOST` for this invocation |
| `--output <text\|json>` | Output/log format — see `--log-format` row in the spec table |
| `--strict <host-requirements\|all>` | Elevate host-requirements from warn to fail |
| `--skip-checksum` | Skip agent-binary SHA256 check (for custom agent builds) |
| `--branch <BRANCH>` | Target a worktree-backed branch container |
| `--no-network-rules` | Skip network proxy block rules (proxy forwarding still active) |
| `--profile` (repeatable) | Docker Compose profile(s) to activate |
| `--env-file` (repeatable) | Extra env-file(s) for Docker Compose |
| `--pull-policy <always\|missing\|never\|build>` | Docker Compose service pull policy |

### `build` Command Flags

| Flag | Spec | Cella | Status |
|------|------|-------|--------|
| `--no-cache` | `false` | Has it | PASS |
| `--image-name` (repeatable) | - | Not implemented | MISSING |
| `--platform` | - | Not implemented | MISSING |
| `--push` | `false` | Not implemented | MISSING |
| `--label` (repeatable) | - | Not implemented | MISSING |
| `--output` | - (buildx output target, e.g. `type=image`) | Has `--output` but as log format (`text`/`json`) — name collision | FAIL (same flag name, different semantics) |
| `--cache-from` | - | Not implemented | MISSING |
| `--cache-to` | - | Not implemented | MISSING |
| `--buildkit` | `auto` | Not implemented (BuildKit is driven implicitly by `--secret`) | MISSING |
| `--additional-features` | - | Not implemented | MISSING |

### Cella-specific flags on `build` (not in spec)

| Flag | Purpose |
|------|---------|
| `-v`, `--verbose` | Expanded step details |
| `--pull <always\|missing\|never>` | Image pull policy (cella-specific; also on `up`) |
| `--workspace-folder` | Explicit workspace folder |
| `--config` | Path to devcontainer.json |
| `--backend`, `--docker-host` | Backend selection / Docker host override |
| `--secret` (repeatable) | BuildKit build secret (`id=X[,src=Y][,env=Z]`) |
| `--profile`, `--env-file`, `--pull-policy` | Docker Compose flags (when building a compose workspace) |

---

## 3. Behavioral Divergences

### 3.1 devcontainerId Computation
- **Spec**: SHA-256 of sorted JSON label object, base-32 encoded, left-padded to 52 chars
- **Cella**: SHA-256 of sorted JSON label object (`devcontainer.local_folder`, `devcontainer.config_file`), base-32 encoded, 52 chars
- **Status**: PASS

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
- **Cella**: `run_all_lifecycle_phases` propagates errors via `?`. The `run_lifecycle_phases_with_wait_for` background path uses `set -e` and `entry_to_shell_command` to handle all command forms (string, array, object). Parallel commands in the background script track PIDs individually and fail if any process exits non-zero. Status written to `/tmp/.cella/lifecycle_status.json`.
- **Status**: PASS

### 3.5 Parallel Command Failure
- **Spec**: All parallel commands must succeed; implementations should cancel siblings on failure
- **Cella**: Object-form lifecycle commands use `try_join_all` which cancels remaining futures on first failure by dropping them.
- **Status**: PASS

### 3.6 JSON Output Format
- **Spec**: stdout = JSON only, stderr = logs only
- **Cella**: `--output json` routes tracing to stderr; JSON result to stdout via `println!`
- **Status**: PASS

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
- **Cella**: Emits both `dev.cella.*` and spec-standard `devcontainer.*` labels
- **Status**: PASS

### 3.11 Config Discovery Flag
- **Spec**: `--config`
- **Cella**: `--config` on `up`, `build`, and `read-configuration`
- **Status**: PASS

### 3.12 hostRequirements Merge
- **Spec**: Maximum value wins across metadata layers
- **Cella**: `merge_host_requirements` in `cella-config/src/devcontainer/merge.rs` applies per-key max semantics during layer merge. `host_requirements.rs` in `cella-orchestrator` validates the merged values against the detected host
- **Status**: PASS

### 3.13 Port String Format
- **Spec**: Supports `"host:container"` strings in `forwardPorts`
- **Cella**: Numeric only
- **Status**: PARTIAL

### 3.14 initializeCommand Re-run
- **Spec**: Runs during initialization including subsequent starts
- **Cella**: Runs on container creation, rebuild, and when the container is already running (`handle_running` fast path)
- **Status**: PASS

### 3.15 Feature Mount Format
- **Spec**: Feature `mounts` is `Mount[]` (objects with `type`, `source?`, `target`); `devcontainer.json` mounts accept both objects and strings
- **Cella**: Feature mounts parsed as objects, normalized to CSV strings (`type=X,source=Y,target=Z`) during metadata parsing
- **Status**: PASS

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
| Git root mount | `--mount-workspace-git-root` | Medium |
| Variable substitution | `${devcontainerId}`, `${localEnv:...}`, `${localWorkspaceFolder}`, etc. | High |

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
| CLI enum flags | `ValueEnum` (clap strict parsing) | Invalid values fail at parse time with a helpful list rather than slipping through to a late runtime error |
| `--pull` semantics | Uniform `always`/`missing`/`never` on `up` and `build` | Match Docker's own pull policy vocabulary rather than invent a new one |
| BuildKit secrets | `--secret id=X[,src=Y][,env=Z]` (repeatable) | Reuse BuildKit's established secret syntax so existing `Dockerfile` `RUN --mount=type=secret` works unchanged |
| Prebuilt image lifecycle | Run lifecycle hooks baked into the image's `devcontainer.metadata` label | Prebuilds skip building but still need postCreate/postStart to run; follows spec metadata semantics |
