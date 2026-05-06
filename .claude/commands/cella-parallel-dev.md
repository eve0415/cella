# Cella Parallel Development

Use this skill when you need to work on multiple independent changes simultaneously. Each change runs in its own isolated container with its own worktree.

> **Prerequisite:** `cella task` is available only from inside a cella container via the in-container CLI (the agent binary symlinked as `cella`). If you're on the host, use `cella branch` to create worktree containers and `cella exec` to run commands inside them; the task-dispatch pattern below assumes you are already inside a container.

## When to use

- User gives you a large task with independent parts (e.g., "implement auth, API, and UI")
- You need to parallelize work across multiple modules
- A refactor touches independent subsystems
- You want to run long-running tests in one branch while coding in another

## The Pattern: Decompose -> Dispatch -> Monitor -> Collect

### 1. Decompose

Break the task into independent units of work. Each unit should:
- Be completable without changes to the other units
- Have a clear success criterion
- Map to a logical branch name

### 2. Dispatch with `cella task run`

```sh
cella task run <branch> [--base <ref>] [--timeout <secs>] -- <command...>
```

Creates the branch + container (if needed) and runs the command in the background. The `--timeout` flag kills the task after the specified duration (status becomes `timed_out` instead of `failed`).

Examples:
```sh
# Dispatch Claude Code (--dangerously-skip-permissions required for headless)
cella task run feat/auth --timeout 300 -- claude --dangerously-skip-permissions -p "Implement OAuth2 authentication in src/auth/"

# Dispatch Codex (bash -c wrapper needed to preserve prompt quoting)
cella task run feat/api --timeout 300 -- bash -c 'codex exec "Build the REST API endpoints in src/api/"'

# Dispatch any CLI agent
cella task run feat/tests --timeout 300 -- claude --dangerously-skip-permissions -p "Write integration tests for the auth module"
```

Task environment parity: tasks get the same user, PATH, working directory, and environment variables (AI keys, SSH agent, terminal vars) as interactive `cella exec`.

### 3. Monitor with `cella task list`

```sh
cella task list [--json]
```

Shows all active tasks with status, elapsed time, and command:
```
BRANCH               STATUS     TIME     COMMAND
feat/auth            running    2m       claude --dangerously-skip-permissions -p "Implement OAuth2..."
feat/api             timed_out  5m       bash -c codex exec "Build REST API..."
feat/tests           done       5m       claude --dangerously-skip-permissions -p "Write integration..."
```

Statuses: `running`, `done`, `failed`, `timed_out`

With `--json`, outputs structured data for programmatic monitoring:
```sh
cella task list --json | jq '.[] | select(.status == "running")'
```

Elapsed time freezes at completion — a task that ran for 45s will always show 45s, not the time since it started.

### 4. Check output with `cella task logs`

```sh
cella task logs <branch> [--follow]
```

Shows captured stdout/stderr from the task. With `--follow`, streams live output.

### 5. Wait for completion

```sh
cella task wait <branch>
```

Blocks until the task finishes. Returns the exit code.

### 6. Stop if needed

```sh
cella task stop <branch>
```

Aborts a running task (sends SIGTERM to the process tree).

## Agent dispatch patterns

### Claude Code

```sh
cella task run <branch> --timeout 300 -- claude --dangerously-skip-permissions -p "your prompt here"
```

The `--dangerously-skip-permissions` flag is required for headless operation — without it Claude Code waits for interactive plan approval and the task stalls.

### Codex

```sh
cella task run <branch> --timeout 300 -- bash -c 'codex exec "your prompt here"'
```

Multi-word prompts must be wrapped in `bash -c` because `cella task run` shell-splits args after `--`. Without the wrapper, each word becomes a separate arg and Codex interprets them as a command name + arguments instead of a single prompt.

### Polling for completion

```sh
# Poll until all tasks complete
while cella task list --json | jq -e '.[] | select(.status == "running")' > /dev/null 2>&1; do
  sleep 10
done
echo "All tasks complete"
```

## Failure handling

- One task's failure does NOT affect other running tasks
- Timed-out tasks exit with code 124 and status `timed_out`
- Stopped tasks exit with code 130
- After failure: inspect logs, fix the issue, re-run the task (previous entry is replaced)

## Example: Full parallel workflow

```sh
# 1. Dispatch with timeouts
cella task run feat/auth --timeout 300 -- claude --dangerously-skip-permissions -p "Add JWT auth middleware"
cella task run feat/rate-limit --timeout 300 -- claude --dangerously-skip-permissions -p "Add rate limiting"
cella task run feat/logging --timeout 300 -- claude --dangerously-skip-permissions -p "Add structured logging"

# 2. Monitor
cella task list

# 3. Wait for all
cella task wait feat/auth
cella task wait feat/rate-limit
cella task wait feat/logging

# 4. Verify each
cella exec feat/auth -- cargo test -p middleware
cella exec feat/rate-limit -- cargo test -p middleware
cella exec feat/logging -- cargo test

# 5. Report results to user
```

## Lifecycle management

```sh
# Stop a branch's container to free resources
cella down feat/auth

# Restart a stopped container
cella up feat/auth

# Stop and remove container + worktree
cella down feat/auth --rm

# Clean up all worktrees older than a week
cella prune --older-than 7d

# Clean up all worktrees
cella prune --all
```

## Performance expectations

- Branch creation: ~80s (full container build including image pull and postStartCommand)
- Exec latency: sub-second for simple commands
- Claude Code task: 20-30s for simple file operations
- Codex task: 10-15s for simple file operations
- Cross-container exec roundtrip: sub-second

## Best practices

- 3-4 parallel agents is the sweet spot for most repos
- Use `--timeout` on all agent dispatches to prevent runaway tasks
- Use `cella task list --json` for programmatic monitoring
- Use `cella exec` for quick verification commands after tasks complete
- Containers persist until explicitly removed with `cella down --rm` or `cella prune`
- `cella down --rm` automatically cleans up stale task records for the removed branch
- The host filesystem is shared — git operations are coordinated through the worktree mechanism
