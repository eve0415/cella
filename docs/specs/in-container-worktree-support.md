# In-Container Worktree Support

> Full git worktree lifecycle management from inside cella containers, enabling AI agents to create branches, dispatch parallel work, and orchestrate multi-container workflows without user interaction.

## Problem

cella's "1 branch = 1 worktree = 1 container" model only works from the **host**. Inside a container:

- `cella` CLI is not installed (only `cella-agent` runs inside)
- No Docker socket access — can't create containers
- Worktree paths use a sibling pattern that assumes host filesystem layout
- `cella switch` is unimplemented
- AI agents (Claude Code, Codex, Gemini) running inside containers can't create worktrees or dispatch parallel work

This blocks the key workflow: an AI agent receiving a large task, decomposing it, and working on independent parts in parallel across isolated containers.

## Solution

**Host-delegated model**: extend the in-container `cella-agent` binary with user-facing commands (`branch`, `list`, `task`, `exec`, `switch`, `prune`) that delegate to the host daemon via the existing TCP IPC channel. The daemon executes operations using Docker and host filesystem access.

## Architecture

```
Container A (main branch)                     Host
┌─────────────────────────┐    ┌──────────────────────────────────┐
│ User/AI agent            │    │ cella-daemon                     │
│   $ cella branch feat-x  │    │                                  │
│       │                  │    │   ┌─────────────────────┐        │
│   cella-agent            │    │   │ worktree handler     │        │
│       │ BranchRequest    │────│──>│   orchestrator::     │        │
│       │                  │    │   │     branch_create()  │        │
│       │ BranchProgress   │<───│───│   streams progress   │        │
│       │ BranchResult     │<───│───│   returns result     │        │
│       │                  │    │   └─────────────────────┘        │
└─────────────────────────┘    │           │                       │
                                │     ┌─────┴─────┐                │
Container B (feat-x)           │     │ Docker API │                │
┌─────────────────────────┐    │     └───────────┘                │
│ Created by daemon        │    │           │                       │
│ Worktree mounted from    │────│───────────┘                       │
│   host filesystem        │    └──────────────────────────────────┘
└─────────────────────────┘
```

## Components

### 1. cella-orchestrator (new crate)

Extract container orchestration logic from `cella-cli/src/commands/branch.rs` and `up/mod.rs` into a shared crate. Both CLI and daemon depend on it.

**Key functions:**

- `branch_create(repo_root, branch, base, progress_tx) -> Result<BranchResult>` — creates worktree + container, streams progress via channel
- `container_up(workspace, config, progress_tx) -> Result<UpResult>` — the core up pipeline
- `worktree_list(repo_root) -> Result<Vec<WorktreeInfo>>` — list worktrees with container status
- `worktree_prune(repo_root, dry_run, progress_tx) -> Result<PruneResult>` — prune merged worktrees
- `task_run(repo_root, branch, cmd, progress_tx) -> Result<TaskHandle>` — create branch + run background command
- `container_exec(container_id, cmd, tty) -> Result<ExecHandle>` — exec in target container

**Progress reporting:** Uses `tokio::sync::mpsc` sender to stream progress events. Both CLI (renders to terminal) and daemon (serializes to TCP) consume the same events.

**Files to modify:**
- Create `crates/cella-orchestrator/` (new crate)
- Refactor `crates/cella-cli/src/commands/branch.rs` to call orchestrator
- Refactor `crates/cella-cli/src/commands/up/mod.rs` to extract shared pipeline logic
- Refactor `crates/cella-cli/src/commands/prune.rs` to call orchestrator

### 2. Protocol Extensions (cella-port)

Extend `AgentMessage` and `DaemonMessage` enums in `crates/cella-port/src/protocol.rs`.

**New agent messages:**

```rust
// Worktree operations
BranchRequest { request_id: String, branch: String, base: Option<String> }
ListRequest { request_id: String }
PruneRequest { request_id: String, dry_run: bool }

// Task operations
TaskRunRequest { request_id: String, branch: String, command: Vec<String>, base: Option<String> }
TaskListRequest { request_id: String }
TaskLogsRequest { request_id: String, branch: String }
TaskWaitRequest { request_id: String, branch: String }
TaskStopRequest { request_id: String, branch: String }

// Exec operations
ExecRequest { request_id: String, branch: String, command: Vec<String>, tty: bool }

// Interactive stream setup
StreamRequest { request_id: String, exec_id: String }
ExecResize { request_id: String, cols: u16, rows: u16 }
```

**New daemon messages:**

```rust
// Progress streaming (reused across operations)
OperationProgress { request_id: String, step: String, message: String }
OperationOutput { request_id: String, stream: StdoutOrStderr, data: String }

// Results
BranchResult { request_id: String, result: Result<BranchInfo, String> }
ListResult { request_id: String, worktrees: Vec<WorktreeEntry> }
PruneResult { request_id: String, pruned: Vec<String>, errors: Vec<String> }

// Task results
TaskRunResult { request_id: String, task_id: String, container: String }
TaskListResult { request_id: String, tasks: Vec<TaskEntry> }
TaskLogsData { request_id: String, data: String, done: bool }
TaskWaitResult { request_id: String, exit_code: i32 }

// Exec results
ExecResult { request_id: String, exit_code: i32 }

// Interactive stream
StreamReady { request_id: String, stream_port: u16 }
```

**DaemonHello extension:**

```rust
DaemonHello {
    protocol_version: u32,
    daemon_version: String,
    error: Option<String>,
    // NEW: workspace metadata for path resolution
    workspace_path: Option<String>,    // host path from container label
    parent_repo: Option<String>,       // host repo root (if worktree container)
    is_worktree: bool,
}
```

### 3. Agent CLI Commands (cella-agent)

Extend `cella-agent` to handle user-facing commands when invoked as `cella`.

**File:** `crates/cella-agent/src/main.rs`

**Command detection:** Check `argv[0]` — if it ends with `cella` (not `cella-agent`), enter CLI mode. Otherwise, existing agent mode.

**CLI mode commands:**

| Command | Behavior | Delegation |
|---------|----------|------------|
| `cella branch <name> [--base ref]` | Create worktree + container | BranchRequest -> daemon |
| `cella list` | List worktree branches + container status | ListRequest -> daemon |
| `cella prune [--dry-run]` | Remove merged worktrees + containers | PruneRequest -> daemon |
| `cella task run <branch> -- <cmd>` | Create branch + run background command | TaskRunRequest -> daemon |
| `cella task list` | List active tasks | TaskListRequest -> daemon |
| `cella task logs <branch>` | Stream task output | TaskLogsRequest -> daemon |
| `cella task wait <branch>` | Block until task completes | TaskWaitRequest -> daemon |
| `cella task stop <branch>` | Stop a running task | TaskStopRequest -> daemon |
| `cella exec <branch> -- <cmd>` | Run command in target container | ExecRequest -> daemon |
| `cella switch <branch>` | Interactive shell in target container | ExecRequest (tty) -> daemon |
| `cella doctor` | Check environment health | Runs locally, no delegation |
| `cella --help` | Show in-container help | Local |
| Other commands | Error with available command list | Local |

**Unsupported command handling:**

```
$ cella up
Error: `cella up` is not available inside a dev container.

Available commands inside containers:
  cella branch <name>   Create a worktree-backed branch
  cella list            List worktree branches
  cella task run ...    Dispatch parallel work
  cella exec ...        Run command in another branch
  cella switch <name>   Shell into another branch's container
  cella prune           Clean up merged worktrees
  cella doctor          Check environment health

Run `cella --help` on the host for all commands.
```

**Agent binary considerations:**
- Keep using manual arg parsing (no clap) to minimize binary size
- The agent is cross-compiled to static musl — must remain dependency-light
- CLI mode connects to daemon on startup (reads `CELLA_DAEMON_ADDR` + `CELLA_DAEMON_TOKEN`)
- Daemon connection failure = error for all delegated commands, but `doctor` and `--help` still work

### 4. Daemon Worktree Handler (cella-daemon)

Add a worktree request handler to the daemon that processes agent requests.

**File:** `crates/cella-daemon/src/handlers/worktree.rs` (new)

**Handler responsibilities:**
- Receive typed requests from agent TCP connection
- Look up container metadata (workspace_path, parent_repo) from Docker labels
- Call `cella-orchestrator` functions
- Stream progress back to agent via TCP
- Track task state (background execs)

**Task state management:**

```rust
struct TaskState {
    task_id: String,
    branch: String,
    container_id: String,
    exec_id: String,
    command: Vec<String>,
    status: TaskStatus,  // Running, Done, Failed
    started_at: u64,
    exit_code: Option<i32>,
    output_file: PathBuf,  // /tmp/.cella/tasks/{id}.log
}
```

- Tasks stored in daemon memory as `HashMap<String, TaskState>`
- Minimal persistence via container labels: `dev.cella.task={json}`
- On daemon restart: query Docker for containers with task labels, reconstruct active task list

**DaemonHello metadata:**
- On agent connection (AgentHello), daemon looks up the connecting agent's container by name
- Reads `dev.cella.workspace_path` and `dev.cella.parent_repo` labels
- Includes in DaemonHello response so agent caches host paths

### 5. Stream Channel (Phase 3)

For interactive commands (`switch`, `shell`, `exec --tty`):

1. Agent sends `ExecRequest { tty: true }` on JSON channel
2. Daemon starts Docker exec, opens a listener on a random port
3. Daemon responds with `StreamReady { stream_port }`
4. Agent opens second TCP connection to `daemon_host:stream_port`
5. Raw bytes flow bidirectionally: agent stdin/stdout <-> Docker exec attach
6. Terminal resize sent as `ExecResize` on JSON channel
7. Stream connection closes when exec exits

The daemon's stream listener is per-exec and short-lived. No persistent second channel.

### 6. Volume and PATH Setup

**Agent volume changes (`crates/cella-docker/src/volume.rs`):**

During `ensure_agent_volume_populated()`, also create:
- `/cella/bin/cella` — symlink to `/cella/v{version}/{arch}/cella-agent`

**PATH injection (`crates/cella-cli/src/commands/up/` post-create setup):**

Append `/cella/bin` to PATH in shell profiles:
```bash
# Already in post-create setup flow
echo 'export PATH="/cella/bin:$PATH"' >> ~/.bashrc
echo 'export PATH="/cella/bin:$PATH"' >> ~/.zshrc
```

This runs alongside existing shell profile modifications (the setup already probes user shell and environment).

### 7. Claude Code Skills

Two skills checked into `.claude/commands/` in this repo:

**`.claude/commands/cella-worktree.md`** — triggers when user asks about branches, worktrees, or switching contexts. Teaches `cella branch`, `cella switch`, `cella list`, `cella prune`.

**`.claude/commands/cella-parallel-dev.md`** — triggers when task involves multiple independent changes or parallelizable work. Teaches the decompose -> dispatch -> monitor -> merge pattern using `cella task`.

Skills are manually installed by users. Included in repo as reference.

## Phasing

### Phase 1: MVP — branch + list (this spec)

- Create `cella-orchestrator` crate, extract branch/up logic
- Extend protocol: `BranchRequest`, `ListRequest` + responses + progress
- Extend `DaemonHello` with workspace metadata
- Add daemon worktree handler (branch, list only)
- Extend agent binary with CLI mode (argv[0] detection)
- Agent CLI: `branch`, `list`, `--help`, unsupported command errors
- Volume: `/cella/bin/cella` symlink
- Post-create: PATH injection
- Tests: unit tests for protocol, orchestrator; integration test for agent CLI mode

### Phase 2: Parallel Work — task + exec + prune

- Protocol: `TaskRunRequest`, `TaskListRequest`, `TaskLogsRequest`, `TaskWaitRequest`, `TaskStopRequest`, `ExecRequest` + responses
- Daemon: task state management, background exec tracking
- Agent CLI: `task run/list/logs/wait/stop`, `exec`, `prune`
- Daemon: task label persistence + restart recovery

### Phase 3: Interactive + Skills

- Stream channel for TTY forwarding
- Agent CLI: `switch` (interactive shell), `exec --tty`
- Claude Code skills in `.claude/commands/`
- `cella doctor` in-container mode

## Verification

### Phase 1 testing:

1. **Unit tests:**
   - Protocol serialization round-trip for new messages
   - Orchestrator branch_create with mock Docker client
   - Agent argv[0] detection (cella vs cella-agent)
   - Unsupported command error messages

2. **Integration tests (Docker required):**
   - Start daemon, create container with `cella up`
   - Inside container: verify `/cella/bin/cella` exists and is executable
   - Inside container: verify `cella --help` shows in-container commands
   - Inside container: `cella branch test-branch` creates worktree + container on host
   - Inside container: `cella list` shows the new branch
   - Verify rollback: if container creation fails, worktree is cleaned up

3. **Manual verification:**
   - Build cella, run `cella up` on this repo
   - Inside container: `which cella` returns `/cella/bin/cella`
   - Inside container: `cella branch feat-test` succeeds
   - Inside container: `cella list` shows main + feat-test
   - Inside container: `cella up` shows helpful error
   - Verify new container has correct labels (dev.cella.worktree, dev.cella.branch, dev.cella.parent_repo)

## Critical Files

| File | Change |
|------|--------|
| `crates/cella-orchestrator/` | New crate — extracted orchestration logic |
| `crates/cella-port/src/protocol.rs` | New message variants, DaemonHello extension |
| `crates/cella-agent/src/main.rs` | CLI mode via argv[0], new subcommands |
| `crates/cella-daemon/src/handlers/` | New worktree handler module |
| `crates/cella-daemon/src/agent_session.rs` | Route new message types to handler |
| `crates/cella-docker/src/volume.rs` | Add /cella/bin/cella symlink |
| `crates/cella-cli/src/commands/branch.rs` | Refactor to use orchestrator |
| `crates/cella-cli/src/commands/up/mod.rs` | Extract shared pipeline to orchestrator |
| `crates/cella-cli/src/commands/prune.rs` | Refactor to use orchestrator |
| `.claude/commands/cella-worktree.md` | Claude Code skill (Phase 3) |
| `.claude/commands/cella-parallel-dev.md` | Claude Code skill (Phase 3) |
| `Cargo.toml` (workspace) | Add cella-orchestrator member |
