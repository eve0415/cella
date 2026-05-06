# Cella Agent Integration

Use this skill when running AI coding agents (Claude Code, Codex, or others) inside cella containers. Covers how cella's container isolation complements native agent features like teams, subagents, and worktrees.

## Positioning

Cella is the **container isolation layer under native agent features**. Native tools handle orchestration (spawning agents, coordinating work, merging results). Cella provides:
- Isolated environments (packages, dependencies, system state)
- Port de-confliction across parallel agents
- Docker sandboxing for untrusted agent operations
- Unified lifecycle management (start, stop, prune)

## Claude Code inside cella containers

### Subagents (Agent tool)

Claude Code subagents with `isolation: "worktree"` create worktrees **inside** the container's filesystem. These are separate from cella's host-level worktrees:

- Cella worktrees: host-side git worktrees, each with its own container
- Claude worktrees: in-container directory copies for parallel file editing

Both can coexist. Use cella worktrees for full environment isolation; use Claude worktrees for lightweight file-edit parallelism within a single environment.

### Agent Teams (experimental)

Claude Code agent teams (`CLAUDE_CODE_EXPERIMENTAL_AGENT_TEAMS=1`) work inside cella containers. The team lead spawns teammates as independent instances communicating via shared files.

Requirements for teams inside cella:
- The shared git repo is accessible (mounted via cella's bind mount)
- File-based coordination (markdown task lists, inbox messaging) works because the workspace is shared
- Each teammate runs in the same container — they share packages/deps

To run teams across multiple containers, dispatch each agent via `cella task run` in separate worktree containers instead.

### Native worktrees (`claude --worktree`)

`claude --worktree <name>` creates directories under `.claude/worktrees/` inside the container. This is independent of cella's host worktree system and works normally within containers.

## Codex inside cella containers

### `codex exec` for non-interactive dispatch

```sh
cella task run <branch> --timeout 300 -- codex exec "your prompt here"
```

Notes:
- Use `--skip-git-repo-check` if Codex doesn't recognize the worktree directory as a git repo
- Custom agents: place TOML files in `.codex/agents/` within the workspace
- Per-workspace instructions: use `AGENTS.md` at the repo root

### Codex subagents

Codex's native subagent system (capped at 6 threads) works inside cella containers. Switch between them with `/agent` in the Codex CLI.

### Codex as MCP server

When using Codex orchestrated via Agents SDK, the two MCP tools (`codex()` and `codex-reply()`) work inside containers as long as the MCP server process can reach the Codex binary.

## Bridge pattern

```
┌─────────────────────────────────────────────────┐
│  Native orchestration layer                      │
│  (Claude Code teams/subagents, Codex threads)    │
└───────────────────────┬─────────────────────────┘
                        │ spawns/coordinates
┌───────────────────────▼─────────────────────────┐
│  Cella container isolation layer                 │
│  (per-branch containers, port allocation,        │
│   env isolation, lifecycle management)           │
└─────────────────────────────────────────────────┘
```

- **In-session parallelism**: Use native subagents (Agent tool, Codex threads) within a single container
- **Cross-environment parallelism**: Use `cella task run` to dispatch agents in separate containers
- **Hybrid**: Native orchestration decides what to do; cella provides where to do it

## Best practices

- **3-4 parallel containers** is the sweet spot for most repos and machines
- **Always use `--timeout`** on `cella task run` dispatches to prevent runaway agents
- **Use `isolation: "worktree"` on every code-writing Claude subagent** — prevents file conflicts between parallel agents in the same container
- **File-based coordination works across containers** because the git repo is shared via bind mount
- **Don't reinvent messaging** — use native team/subagent communication; cella handles isolation only
- **Monitor with `cella task list --json`** for programmatic status checking

## Troubleshooting

| Issue | Solution |
|-------|----------|
| Codex doesn't recognize git repo in worktree | Add `--skip-git-repo-check` flag |
| Claude Code can't find tools/packages | Packages are per-container; install in each branch's container or use a shared base image |
| Agent can't reach API endpoints | Check container network config; cella containers share the host network by default |
| Port conflict between agents | Cella auto-allocates ports per container; check `cella list` for port mappings |
| Agent teams can't communicate | Ensure all teammates run in the same container (not across containers) |
| Task shows "timed_out" | Increase `--timeout` or break the task into smaller pieces |

## When to use

- Running Claude Code or Codex inside cella dev containers
- Setting up parallel AI agent workflows across multiple branches
- Debugging why an agent feature doesn't work inside a container
- Choosing between native parallelism (subagents) vs cella parallelism (separate containers)
- Configuring agent coordination patterns within the cella ecosystem
