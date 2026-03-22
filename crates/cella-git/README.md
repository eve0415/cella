# cella-git

> Git operations and error handling for cella.

Part of the [cella](../../README.md) workspace.

## Overview

cella-git provides git-related error types and will serve as the home for git worktree operations. Currently, the crate defines error types for common git failures (missing git binary, command failures, non-repository directories) used by other crates when invoking git commands.

This crate is intentionally minimal today. The worktree integration — creating, listing, and removing worktrees, binding worktrees to containers, and managing worktree-based branches — is a major planned feature that will make this a central crate in the workspace.

## Architecture

### Key Types

- `CellaGitError` — error enum with variants:
  - `GitNotFound` — git binary not found in PATH
  - `CommandFailed(String)` — git command returned an error
  - `NotARepository` — current directory is not a git repository

### Modules

| Module | Purpose |
|--------|---------|
| `error` | `CellaGitError` definition |

## Crate Dependencies

**Depends on:** none (only thiserror)

**Depended on by:** [cella-cli](../cella-cli)

## Testing

```sh
cargo test -p cella-git
```

Minimal test surface currently, matching the crate's minimal implementation.

## Planned

- Git worktree creation, listing, and removal
- Branch management tied to worktrees (1 branch = 1 worktree = 1 container)
- Repository state detection and validation
- Worktree-container binding coordination

## Development

When worktree support is implemented, this crate will likely depend on a git library (e.g., gix) for pure-Rust git operations. The error types are already designed to accommodate worktree-related failures.
