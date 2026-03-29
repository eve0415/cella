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

### Clean up merged branches

```sh
cella prune [--dry-run]
```

Removes worktrees whose branches have been merged into the default branch, along with their containers. Use `--dry-run` to preview what would be removed.

## When to use

- User asks to "work on a new branch" or "create a feature branch"
- User asks to "check what branches exist" or "list worktrees"
- User asks to "clean up old branches"
- You need to run a command in a different branch's environment
- You need to verify something in another branch without leaving the current one
