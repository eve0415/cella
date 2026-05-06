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
cella branch <name> [--base <ref>] [--label key=value]...
```

Creates a git worktree for `<name>` on the host, builds and starts a new container for it. The `--base` flag specifies which commit/branch to branch from (defaults to HEAD). `--label` adds custom metadata labels to the container.

Example:
```sh
cella branch feat/auth --base main
cella branch feat/api --label team=backend --label priority=high
```

### List branches

```sh
cella list [--json]
```

Shows all worktree branches with their container name and state (running/exited). The current container is marked with `*`. Use `--json` for programmatic output.

### Run a command in another branch's container

```sh
cella exec <branch> -- <command...>
cella exec <branch> --json -- <command...>
```

Runs a command in the specified branch's container and streams output. The exit code is propagated. Works bidirectionally — worktree containers can exec to main and vice versa. With `--json`, outputs structured `{"exit_code": N, "stdout": "...", "stderr": "..."}`.

Examples:
```sh
cella exec feat/auth -- cargo test
cella exec main -- echo "hello from main"
cella exec feat/auth --json -- cat src/auth.rs
```

### Switch to another branch's container

```sh
cella switch <branch>
```

Opens an interactive shell session in the target branch's container. Use this for exploratory work; for running specific commands, prefer `cella exec`.

### Stop a branch's container

```sh
cella down <branch> [--rm] [--volumes] [--force]
```

Stops the container for the specified branch. With `--rm`, also removes the container and worktree directory. With `--volumes`, removes associated volumes (requires `--rm`). With `--force`, overrides shutdownAction="none".

### Start/restart a branch's container

```sh
cella up <branch> [--rebuild]
```

Starts or restarts the container for the specified branch. If the container exists but is stopped, it restarts it. With `--rebuild`, rebuilds from scratch.

### Clean up worktrees

```sh
cella prune [--all] [--dry-run] [--older-than <duration>] [--missing-worktree] [--label key=value]...
```

Removes worktrees and their containers. Options:
- `--all` — removes all linked worktrees including unmerged
- `--dry-run` — preview what would be removed
- `--older-than 7d` — only prune worktrees older than duration
- `--missing-worktree` — prune entries whose worktree directory no longer exists
- `--label key=value` — only prune worktrees matching these labels

### Diagnostics

```sh
cella doctor [--json]
```

Checks connectivity to the host daemon, protocol version match, agent version, and credential helper status. Exits 1 if any check fails. With `--json`, outputs structured diagnostics.

## Troubleshooting

| Symptom | Fix |
|---------|-----|
| "No daemon connection info" | Run `cella up` on the host to start the daemon |
| "Failed to connect to host daemon" | Check `cella doctor` output; restart with `cella up` on host |
| Exec to main fails | Fixed in v0.0.48+ — main container lookup uses workspace_path fallback |
| `*` marker shows wrong branch | Fixed in v0.0.48+ — uses CELLA_CONTAINER_NAME matching |
| JSON in human output | Fixed in v0.0.48+ — daemon JSON responses are filtered; `cella branch` stdout suppressed at daemon level |
| Task ran in wrong container | Fixed — `task run` on non-existent branch now auto-creates instead of falling back to main |
| Stale task records after `down --rm` | Fixed — `down --rm` cleans up task records for the removed branch |

## When to use

- User asks to "work on a new branch" or "create a feature branch"
- User asks to "check what branches exist" or "list worktrees"
- User asks to "clean up old branches" or "remove a branch"
- User asks to "stop" or "restart" a branch's container
- User mentions a reboot or stopped containers that need restarting
- You need to run a command in a different branch's environment
- You need to verify something in another branch without leaving the current one
- You need to free resources by stopping unused branches
