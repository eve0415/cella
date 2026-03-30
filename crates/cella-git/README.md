# cella-git

> Git worktree management, branch resolution, and repository operations.

Part of the [cella](../../README.md) workspace.

## Overview

cella-git provides the git operations that power cella's "1 branch = 1 worktree = 1 container" model. It manages the full worktree lifecycle (create, list, remove), resolves branch state (new, existing, merged, tracking-gone), discovers repository information, and computes content hashes for change detection.

The crate wraps the git CLI rather than using a git library. All commands run through a central command runner (`cmd` module) that handles output parsing and retries git operations with exponential backoff when lock contention is detected (`.git/index.lock`). Branch names are sanitized into directory-safe names for worktree paths.

Content hashing (git HEAD + dirty file status) enables `updateContentCommand` — cella stores the hash in a container label and re-runs lifecycle commands when the workspace changes between `cella up` invocations.

## Architecture

### Key Types

- `WorktreeInfo` — a discovered worktree (path, branch, HEAD commit)
- `BranchState` — resolution result for a branch name (new, existing local, existing remote, merged)
- `RepoInfo` — repository metadata (root path, current branch, HEAD)
- `CellaGitError` — error enum with 10 variants covering git failures, lock contention, worktree conflicts, branch issues, and parse errors

### Key Functions

- `discover(path)` — find the git repository root from a path
- `default_branch(repo)` — detect the default branch (main/master)
- `resolve_branch(repo, name)` — determine if a branch is new, local, remote, or merged
- `create(repo, branch, path)` — create a new worktree for a branch
- `list(repo)` — list all worktrees
- `remove(repo, path)` — remove a worktree and optionally its branch
- `content_hash::compute(repo)` — compute hash of git HEAD + dirty files for change detection
- `branch_to_dir_name(branch)` — sanitize a branch name into a valid directory name

### Modules

| Module | Purpose |
|--------|---------|
| `branch` | Branch state resolution (`resolve_branch`), merged branch detection, tracking-gone detection |
| `cmd` | Git CLI command runner with output capture and exponential backoff retry on lock contention |
| `content_hash` | Workspace content hashing (git HEAD + dirty file status) for `updateContentCommand` change detection |
| `error` | `CellaGitError` enum (10 variants) with display formatting |
| `repo` | Repository discovery, default branch detection, container detection (`is_inside_container`) |
| `sanitize` | Branch name to directory name conversion (strips `refs/heads/`, replaces `/` with `-`) |
| `worktree` | Worktree lifecycle — create, list, remove, path resolution |

## Crate Dependencies

**Depends on:** none (only thiserror, tracing)

**Depended on by:** [cella-cli](../cella-cli), [cella-orchestrator](../cella-orchestrator)

## Testing

```sh
cargo test -p cella-git
```

Error display tests use insta snapshot assertions. Worktree and branch tests use tempfile for isolated git repositories.

```sh
cargo insta review  # after changing error messages
```

## Development

The `cmd` module is the only place that invokes `git`. All other modules call through it. Lock contention retry is important because concurrent `cella branch` invocations on the same repo can race on the git index lock.

`BranchState` is the key abstraction for the `cella branch` command — it determines whether to create a new branch, check out an existing one, or fetch from a remote tracking branch. When adding new branch operations, work through `resolve_branch` to handle all cases.

The `content_hash` module hashes both the git HEAD ref and the list of dirty files (from `git status --porcelain`). This means both committed and uncommitted changes trigger `updateContentCommand` re-execution — matching the spec's intent that workspace content changes should re-run setup.
