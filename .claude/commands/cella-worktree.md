# Cella Worktree Management

Use this skill when inside a cella dev container and you need to create, list, or manage git worktree-backed branches. Each branch gets its own isolated container.

## Detection

You are inside a cella container if:
- `/cella/bin/cella` exists
- `CELLA_DAEMON_ADDR` environment variable is set
- `/.dockerenv` exists

## Commands

### Create a new branch

```sh
cella branch <name> [--base <ref>]
```

Creates a git worktree for `<name>` on the host, builds and starts a new container for it. The `--base` flag specifies which commit/branch to branch from (defaults to HEAD).

Example:
```sh
cella branch feat/auth --base main
```

### List branches

```sh
cella list
```

Shows all worktree branches with their container name and state (running/exited).

### Switch to another branch's container

```sh
cella switch <branch>
```

Opens a shell session in the target branch's container. Use this for quick checks; for running specific commands, prefer `cella exec`.

### Run a command in another branch's container

```sh
cella exec <branch> -- <command...>
```

Runs a command in the specified branch's container and streams output. The exit code is propagated.

Examples:
```sh
cella exec feat/auth -- cargo test
cella exec feat/auth -- cat src/auth.rs
```

### Stop a branch's container

```sh
cella down <branch> [--rm] [--volumes] [--force]
```

Stops the container for the specified branch. With `--rm`, also removes the container and worktree directory. With `--volumes`, removes associated volumes (requires `--rm`). With `--force`, overrides shutdownAction="none".

Example:
```sh
cella down feat/auth          # Stop the container
cella down feat/auth --rm     # Stop, remove container + worktree
```

### Start/restart a branch's container

```sh
cella up <branch> [--rebuild]
```

Starts or restarts the container for the specified branch. If the container exists but is stopped, it restarts it. If the worktree exists but the container was removed, it creates a new container. With `--rebuild`, rebuilds from scratch.

Example:
```sh
cella up feat/auth            # Restart stopped container
cella up feat/auth --rebuild  # Rebuild from scratch
```

### Clean up worktrees

```sh
cella prune [--all] [--dry-run]
```

Removes worktrees and their containers. By default, only removes worktrees whose branches have been merged. With `--all`, removes all linked worktrees including unmerged ones. Use `--dry-run` to preview what would be removed.

Examples:
```sh
cella prune                   # Remove merged worktrees
cella prune --all             # Remove ALL worktrees
cella prune --all --dry-run   # Preview what would be removed
```

## When to use

- User asks to "work on a new branch" or "create a feature branch"
- User asks to "check what branches exist" or "list worktrees"
- User asks to "clean up old branches" or "remove a branch"
- User asks to "stop" or "restart" a branch's container
- User mentions a reboot or stopped containers that need restarting
- You need to run a command in a different branch's environment
- You need to verify something in another branch without leaving the current one
- You need to free resources by stopping unused branches
