# Cella Parallel Development

Use this skill when you need to work on multiple independent changes simultaneously. Each change runs in its own isolated container with its own worktree.

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
cella task run <branch> [--base <ref>] -- <command...>
```

Creates the branch + container (if needed) and runs the command in the background. The command typically invokes an AI agent (Claude Code, Codex, etc.).

Examples:
```sh
# Dispatch three parallel tasks
cella task run feat/auth -- claude -p "Implement OAuth2 authentication in src/auth/"
cella task run feat/api -- claude -p "Build the REST API endpoints in src/api/"
cella task run feat/tests -- claude -p "Write integration tests for the auth module"
```

### 3. Monitor with `cella task list`

```sh
cella task list
```

Shows all active tasks with status, elapsed time, and command:
```
BRANCH               STATUS     TIME     COMMAND
feat/auth            running    2m       claude -p "Implement OAuth2..."
feat/api             running    2m       claude -p "Build REST API..."
feat/tests           done       5m       claude -p "Write integration..."
```

### 4. Check output with `cella task logs`

```sh
cella task logs <branch>
```

Shows captured stdout/stderr from the task.

### 5. Wait for completion

```sh
cella task wait <branch>
```

Blocks until the task finishes. Returns the exit code.

### 6. Stop if needed

```sh
cella task stop <branch>
```

Aborts a running task.

## Additional commands for manual work

After tasks complete, you may want to inspect results:

```sh
# Run a specific command in a task's container
cella exec feat/auth -- cargo test

# Check test results
cella exec feat/api -- cat test-results.xml

# View the diff
cella exec feat/auth -- git diff
```

## Example: Full parallel workflow

```sh
# User: "Add authentication, rate limiting, and logging to the API"

# 1. Dispatch
cella task run feat/auth -- claude -p "Add JWT authentication middleware to src/middleware/auth.rs"
cella task run feat/rate-limit -- claude -p "Add rate limiting middleware to src/middleware/rate_limit.rs"
cella task run feat/logging -- claude -p "Add structured logging with tracing to all API handlers"

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

After tasks complete, you can manage the branch containers:

```sh
# Stop a branch's container to free resources
cella down feat/auth

# Restart a stopped branch's container
cella up feat/auth

# Stop and remove container + worktree
cella down feat/auth --rm

# Clean up all worktrees (including unmerged)
cella prune --all

# Clean up only merged worktrees
cella prune
```

## Important notes

- Each branch gets its own container with the full dev environment
- Tasks run independently — one failure doesn't affect others
- The host filesystem is shared, so git operations are coordinated
- Use `cella down <branch>` to stop individual branches and free resources
- Use `cella up <branch>` to restart stopped branches (e.g., after reboot)
- Use `cella prune` to clean up merged branches, `cella prune --all` for all branches
