---
name: cella-ops
description: Use when inside a cella dev container and you need to manage branches, dispatch tasks, run parallel agents, or interact with other containers. Also use when you see CELLA_DAEMON_ADDR in the environment or /cella/bin/cella exists.
---

# Cella Operations

Complete reference for operating the cella worktree-container system from inside a running container. Covers branch management, task dispatch, agent integration, and parallel development.

## Mental Model

**1 branch = 1 container = 1 agent.**

Cella extends dev containers with git worktrees. Each worktree branch gets its own isolated container with its own filesystem, packages, ports, and environment. The host git repo is shared via bind mount, so git operations (commits, merges) are coordinated through the worktree mechanism.

**Architecture**: A cella daemon runs on the host and manages all containers. Inside each container, a cella agent binary (symlinked as `cella`) communicates with the host daemon over gRPC. The commands in this skill are agent-side commands — they talk to the daemon to create branches, dispatch tasks, etc. Host-side commands (like `cella up` without a branch argument to start the daemon) are different and not covered here.

```
  main (container A)          feat/auth (container B)       feat/api (container C)
  ┌──────────────────┐        ┌──────────────────┐         ┌──────────────────┐
  │  You are here    │        │  AI agent        │         │  AI agent        │
  │  (human)         │◄──────►│  (autonomous)    │◄───────►│  (autonomous)    │
  │  ports: 3000     │  exec  │  ports: 3001     │  exec   │  ports: 3002     │
  └────────┬─────────┘        └────────┬─────────┘         └────────┬─────────┘
           └──────────────────────────┴────────────────────────────┘
                                      │
                              ┌───────┴───────┐
                              │  .git (shared) │
                              └───────────────┘
```

Containers can communicate bidirectionally via `cella exec`. The primary worktree container can exec into worktrees and vice versa. Worktree-to-worktree exec also works. Note: `cella exec main` targets the primary/original worktree container, which may be running any branch (not necessarily a branch named `main`).

## Detection

You are inside a cella container if BOTH of these are true:

- `/cella/bin/cella` exists (the agent binary)
- `CELLA_DAEMON_ADDR` environment variable is set (connection to host daemon)

Quick check: `cella doctor` exits 0.

## Choosing: `cella exec` vs `cella task run`

Both run commands in another branch's container. The key difference:

- **`cella exec`** — synchronous, foreground. Blocks until the command finishes. Output streams to your terminal. Use for quick commands, verification, and interactive work.
- **`cella task run`** — asynchronous, background. Returns immediately. Output is captured for later retrieval with `cella task logs`. Use for long-running work like agent dispatches.

## Commands Reference

### Container Diagnostics

```sh
cella doctor [--json]
```

Checks connectivity to the host daemon, protocol version match, agent version, and credential helper status. Exits 0 if all checks pass, 1 if any fail. With `--json`, outputs structured diagnostics including the current version number.

To check your version: `cella doctor --json | jq '.daemon_version'`

### List Branches

```sh
cella list [--json]
```

Shows all worktree branches with their container name and state (running/exited). The current container is marked with `*`. Use `--json` for programmatic output.

### Create a Branch

```sh
cella branch <name> [--base <ref>] [--label key=value]...
```

Creates a git worktree for `<name>` on the host, builds and starts a new container for it. Takes ~80s (full container build including image pull and devcontainer lifecycle commands).

- `--base <ref>` — branch from a specific commit/branch (defaults to HEAD)
- `--label key=value` — add custom metadata labels to the container (repeatable)

On success, prints: `Ready: <worktree_path> (container: <container_name>)`

```sh
cella branch feat/auth --base main
cella branch feat/api --label team=backend --label priority=high
```

### Run a Command in Another Branch (Synchronous)

```sh
cella exec <branch> [--json] -- <command...>
```

Runs a command in the specified branch's container and streams stdout/stderr to your terminal. The process exit code matches the remote command's exit code. Works bidirectionally — worktree containers can exec to main and vice versa. Worktree-to-worktree also works.

The `--` separator before the command is mandatory.

- `--json` — outputs structured `{"exit_code": N, "stdout": "...", "stderr": "..."}`

```sh
cella exec feat/auth -- cargo test
cella exec main -- echo "hello from main"
cella exec feat/auth --json -- cat src/auth.rs
```

### Switch to Another Branch

```sh
cella switch <branch>
```

Opens an interactive shell session in the target branch's container. For running specific commands, prefer `cella exec`.

### Stop a Branch Container

```sh
cella down <branch> [--rm] [--volumes] [--force]
```

Stops the container for the specified branch.

- `--rm` — also removes the container, worktree directory, and associated task records. On success, prints: `Removed branch '<name>' (container: <container_name>)`. May fail with `ContainerNotFound` if the container was already removed — in that case, use `cella prune --missing-worktree` to clean up the orphaned entry
- `--volumes` — removes associated volumes (requires `--rm`)
- `--force` — overrides the devcontainer.json `shutdownAction` setting (e.g., when `shutdownAction` is set to `"none"` to keep the container running)

### Start/Restart a Branch Container

```sh
cella up <branch> [--rebuild]
```

Starts or restarts the container for the specified branch. If the container exists but is stopped, it restarts it. With `--rebuild`, rebuilds from scratch.

Note: this is the in-container `cella up <branch>` command. The host-side `cella up` (no branch argument) starts the daemon and main container — that's a different binary.

### Clean Up Worktrees

```sh
cella prune [--all] [--dry-run] [--older-than <duration>] [--missing-worktree] [--label key=value]...
```

Removes worktrees and their containers in bulk.

- `--all` — removes all linked worktrees including unmerged
- `--dry-run` — preview what would be removed (lists individual branches)
- `--older-than 7d` — only prune worktrees older than duration
- `--missing-worktree` — prune entries whose worktree directory no longer exists
- `--label key=value` — only prune worktrees matching these labels

## Task Dispatch

> `cella task` is available only from inside a cella container. If you're on the host, use `cella branch` + `cella exec` instead.

### Run a Task (Asynchronous)

```sh
cella task run <branch> [--base <ref>] [--timeout <secs>] -- <command...>
```

Creates the branch + container (if the branch doesn't already exist) and runs the command in the background. Returns immediately. If the branch already exists and has a previous task entry, the new task replaces it.

The `--` separator before the command is mandatory.

- `--base <ref>` — branch from a specific ref when creating a new branch
- `--timeout <secs>` — kills the task after the specified duration (status becomes `timed_out` instead of `failed`)

Task environment parity: tasks get the same user, PATH, working directory, and environment variables (API keys, SSH agent, terminal vars) as interactive `cella exec`.

### List Tasks

```sh
cella task list [--json]
```

Shows all tasks with status, elapsed time, and command:

```
BRANCH               STATUS     TIME     COMMAND
feat/auth            running    2m       claude --dangerously-skip-permissions -p "..."
feat/api             timed_out  5m       bash -c codex exec "..."
feat/tests           done       45s      claude --dangerously-skip-permissions -p "..."
```

Statuses: `running`, `done`, `failed`, `timed_out`

Elapsed time freezes at completion — a task that ran for 45s will always show 45s.

With `--json`, outputs structured data:
```sh
cella task list --json | jq '.[] | select(.status == "running")'
```

### View Task Logs

```sh
cella task logs <branch> [--follow]
```

Shows captured stdout/stderr from the task. With `--follow`, streams live output.

### Wait for Task Completion

```sh
cella task wait <branch>
```

Blocks until the task finishes. The process exit code (`$?`) matches the task's exit code (see table below). Prints a summary line to stdout (`Task <branch> exited with code N`) when it observes the exit — if the task already completed before `wait` is called, it may return silently with just the exit code.

### Stop a Task

```sh
cella task stop <branch>
```

Aborts a running task (sends SIGTERM to the process tree).

### Exit Codes

| Code | Meaning |
|------|---------|
| 0 | Success (`done`) |
| 124 | Timed out (`timed_out`) |
| 130 | Stopped by user (`failed`) |
| Non-zero | Command failed (`failed`) |

Note: `cella task list --json` does not include an exit code field — it only reports `status`. To get the numeric exit code, use `cella task wait <branch>` (via `$?`).

## Agent Dispatch Patterns

For headless agent dispatch across containers, `cella task run` wraps the agent CLI. Always include `--timeout` to prevent runaway agents.

**Claude Code:**
```sh
cella task run <branch> --timeout 300 -- claude --dangerously-skip-permissions -p "your prompt here"
```

`--dangerously-skip-permissions` is required for headless operation — without it Claude Code waits for interactive approval and the task stalls. The "no stdin data received in 3s" warning is cosmetic and does not block execution.

**Codex:**
```sh
cella task run <branch> --timeout 300 -- bash -c 'codex exec "your prompt here"'
```

Multi-word prompts must be wrapped in `bash -c` because `cella task run` shell-splits args after `--`. Without the wrapper, each word becomes a separate arg and Codex interprets them as a command name instead of a single prompt. Add `--skip-git-repo-check` if Codex doesn't recognize the worktree as a git repo.

### Polling for Completion

```sh
# Poll all tasks
while cella task list --json | jq -e '.[] | select(.status == "running")' > /dev/null 2>&1; do
  sleep 10
done

# Or filter by specific branches to avoid blocking on unrelated tasks
while cella task list --json | jq -e '.[] | select(.branch == "feat/auth" or .branch == "feat/api") | select(.status == "running")' > /dev/null 2>&1; do
  sleep 10
done
```

## Native Agent Features Inside Containers

Cella is the **container isolation layer** underneath native agent features. Native tools handle orchestration (spawning agents, coordinating work). Cella provides isolated environments, port de-confliction, Docker sandboxing, and lifecycle management.

### Claude Code Subagents (Agent Tool)

Claude Code's Agent tool spawns subagents to work in parallel. When called with `isolation: "worktree"`, the subagent gets its own git worktree inside the container for safe parallel file editing:

```
Agent({
  description: "Implement auth module",
  prompt: "Add JWT auth to src/auth/",
  isolation: "worktree"
})
```

These Claude-level worktrees are **inside the container's filesystem** — separate from cella's host-level worktrees:

- **Cella worktrees**: host-side, each with its own container (full environment isolation — different packages, ports, deps)
- **Claude worktrees**: in-container directory copies (lightweight — same packages, ports, deps, just separate file trees)

Both coexist. Use cella worktrees (`cella branch`) when you need full environment isolation. Use Claude worktrees (`isolation: "worktree"`) when you need lightweight file-edit parallelism within a single container.

### Claude Code Agent Teams

Agent teams let a team lead spawn teammates as independent Claude Code instances within the same container. They coordinate via shared files in the workspace.

**Enabling**: set the environment variable `CLAUDE_CODE_EXPERIMENTAL_AGENT_TEAMS=1` before starting Claude Code.

**How it works inside cella**:
- The team lead spawns teammates; each teammate is a separate Claude Code process
- Coordination uses file-based mechanisms: the team lead writes task descriptions to markdown files, teammates pick them up, and report results back via the same files
- All teammates share the same container — same packages, deps, environment, and mounted git repo
- The team lead orchestrates: assigns work, monitors progress, and collects results

**When to use teams vs `cella task run`:**

| Use teams when... | Use `cella task run` when... |
|---|---|
| Agents need to coordinate on interrelated files | Agents work on independent features |
| Changes must stay in sync (e.g., API + client) | Each agent needs its own branch |
| Single environment is sufficient | Agents need separate deps/ports |
| You want native Claude Code orchestration | You want full container isolation |

**Limitation**: all teammates must run in the same container. For cross-container agent work, dispatch each via `cella task run` in separate worktree containers.

### Codex Inside Cella Containers

**Subagents**: Codex's native subagent system (capped at 6 concurrent threads) works inside cella containers. In the Codex CLI, use `/agent` to switch between active subagents. All subagents share the same container environment.

**Cross-container Codex work**: dispatch separate `cella task run` commands into different branch containers. Each gets its own worktree, container, and environment. This is how you parallelize Codex work with full isolation.

**Configuration**:
- Custom agents: place TOML config files in `.codex/agents/` within the workspace
- Per-workspace instructions: use `AGENTS.md` at the repo root
- Git worktree recognition: add `--skip-git-repo-check` if Codex doesn't recognize the worktree directory

### Bridge Pattern: Native Orchestration + Cella Isolation

```
┌─────────────────────────────────────────────────┐
│  Native orchestration layer                      │
│  (Claude Code teams/subagents, Codex threads)    │
│  Decides WHAT to do                              │
└───────────────────────┬─────────────────────────┘
                        │ spawns/coordinates
┌───────────────────────▼─────────────────────────┐
│  Cella container isolation layer                 │
│  (per-branch containers, port allocation,        │
│   env isolation, lifecycle management)           │
│  Provides WHERE to do it                         │
└─────────────────────────────────────────────────┘
```

- **In-container parallelism**: native subagents (Agent tool with `isolation: "worktree"`, Codex `/agent` threads, team-agents) within a single container
- **Cross-container parallelism**: `cella task run` to dispatch agents in separate containers with full isolation
- **Hybrid**: native orchestration decides what to do; cella provides where to do it

## Parallel Development Pattern

### Decompose -> Dispatch -> Monitor -> Collect

**1. Decompose** — break the task into independent units. Each should be completable without changes to the other units and have a clear success criterion.

**2. Dispatch** — create branches and dispatch tasks:
```sh
cella task run feat/auth --timeout 300 -- claude --dangerously-skip-permissions -p "Add JWT auth middleware"
cella task run feat/rate-limit --timeout 300 -- claude --dangerously-skip-permissions -p "Add rate limiting"
cella task run feat/logging --timeout 300 -- claude --dangerously-skip-permissions -p "Add structured logging"
```

**3. Monitor** — check status:
```sh
cella task list
```

**4. Collect** — check results and verify:
```sh
cella task logs feat/auth
cella exec feat/auth -- cargo test -p middleware
```

### Failure Handling

- One task's failure does NOT affect other running tasks
- Timed-out tasks: exit code 124, status `timed_out`
- Stopped tasks: exit code 130
- After failure: inspect logs (`cella task logs <branch>`), fix the issue, re-run the task (previous entry is replaced)

## Performance Expectations

| Operation | Typical Time |
|-----------|-------------|
| Branch creation | ~80s (full container build) |
| Exec latency | Sub-second |
| Cross-container exec roundtrip | Sub-second |
| Claude Code task (simple) | 20-30s |
| Codex task (simple) | 10-15s |

## Troubleshooting

| Symptom | Fix |
|---------|-----|
| "No daemon connection info" | Daemon not running on host. Ask the user to run `cella up` on the host |
| "Failed to connect to host daemon" | Run `cella doctor` for details; daemon may need restart on host |
| Exec to main fails | Fixed in v0.0.48+ — main container lookup uses workspace_path fallback |
| `*` marker shows wrong branch | Fixed in v0.0.48+ — uses CELLA_CONTAINER_NAME matching |
| JSON in human output | Mostly fixed in v0.0.48+ — rare JSON fragments may still appear during branch creation |
| `down --rm` fails with ContainerNotFound | Container was already removed. Use `cella prune --missing-worktree` to clean up the orphaned worktree entry |
| "bind source path does not exist" on branch creation | Transient race condition. Retry `cella branch` — typically succeeds on second attempt |
| Claude Code waits for approval | Add `--dangerously-skip-permissions` for headless execution |
| "no stdin data received in 3s" | Cosmetic Claude Code warning; task proceeds normally |
| Codex treats prompt as command name | Wrap in `bash -c 'codex exec "prompt"'` |
| Codex doesn't recognize git repo | Add `--skip-git-repo-check` flag |
| Packages missing in new container | Packages are per-container; install in each branch or use a shared base image |
| Agent can't reach API endpoints | Cella containers share the host network by default; check DNS/firewall |
| Agent teams can't communicate | All teammates must run in the same container, not across containers |
| Task shows "timed_out" | Increase `--timeout` or break the task into smaller pieces |
| `task wait` returns unexpected exit code after re-dispatch | If you re-dispatch a task on a branch that already had a completed/timed-out task, a concurrent `task wait` from the old dispatch may race with the replacement. Use `cella task list --json` to verify the current task status instead of relying solely on `task wait` exit codes |
| Plan/task files not visible in worktree | Place shared files in `~/.claude/plans/` — this volume is mounted in ALL containers. Don't rely on git-tracked files for cross-worktree sharing |

## Best Practices

- **3-4 parallel containers** is the sweet spot for most repos and machines
- **Always use `--timeout`** on `cella task run` to prevent runaway agents
- Use `isolation: "worktree"` on Claude Code subagents to prevent file conflicts within a container
- Use `cella task list --json` for programmatic monitoring
- Use `cella exec` for quick verification after tasks complete
- Containers persist until explicitly removed with `cella down --rm` or `cella prune`
- File-based coordination works across containers because the git repo is shared via bind mount
- Use native team/subagent features for orchestration; cella handles isolation only
