---
name: cella-testing
description: Use when running a systematic test sweep of the cella worktree/task system from inside a container. Also use after modifying the daemon, agent CLI, or task manager, before releases, or after bug fixes.
---

# Cella End-to-End Testing

Systematic verification of the cella worktree-container-task system. Run this from inside a running cella container to validate that branch creation, exec routing, task dispatch, and lifecycle management all work correctly.

## Context

Cella gives each git branch its own isolated dev container. This test sweep exercises the full stack: daemon connectivity, branch creation (~80s each), cross-container exec, background task dispatch with timeouts, and container lifecycle (stop/start/remove/prune). It catches regressions in the daemon, agent CLI, and task manager.

## Prerequisites

1. You must be inside a cella container. Verify:
   ```sh
   cella doctor
   ```
   Must exit 0. If it fails, the daemon is not reachable — no point continuing.

2. Check version:
   ```sh
   cella doctor --json
   ```
   The JSON output contains a `daemon_version` field. The text output of `cella doctor` (without `--json`) also prints the agent version in its banner. Verify these match.

## Test Phases

### Phase 1: Baseline Diagnostics

```sh
cella doctor
cella doctor --json
cella list
cella list --json
```

**Expected results:**
- `cella doctor` exits 0
- `cella doctor --json` returns valid JSON with a `daemon_version` field
- `cella list` shows at least one container with `*` marking the current one
- `cella list --json` returns valid JSON array

### Phase 2: Branch + Exec Routing

```sh
cella branch test/a
cella exec test/a -- echo "works"
cella exec test/a -- cella exec main -- echo "roundtrip"
cella branch test/b
cella exec test/a -- cella exec test/b -- echo "wt-to-wt"
```

**Expected results:**
- `cella branch test/a` exits 0 (takes ~80s)
- `cella exec test/a -- echo "works"` exits 0, stdout contains "works"
- Roundtrip exec (test/a -> main) exits 0, stdout contains "roundtrip". Note: `main` targets the primary worktree container, which may be running any branch — not necessarily a branch named `main`
- `cella branch test/b` exits 0 (takes ~80s)
- Worktree-to-worktree exec (test/a -> test/b) exits 0, stdout contains "wt-to-wt"

### Phase 3: Task Lifecycle

```sh
cella task run test/a -- echo "done"
cella task list
cella task run test/b --timeout 5 -- sleep 30
sleep 7
cella task list
cella task logs test/b
```

**Expected results:**
- Quick task (`echo "done"`) completes almost instantly; `cella task list` shows status `done` with ~0s elapsed
- Timeout task (`sleep 30` with `--timeout 5`) shows status `timed_out` after ~7s, with elapsed frozen at ~5s
- `cella task logs test/b` may show "Task timed out after 5s" or be empty — the `timed_out` status in `cella task list` is what matters

### Phase 4: Agent Dispatch

```sh
cella task run test/a --timeout 120 -- claude --dangerously-skip-permissions -p "Create /tmp/test.txt with 'hello'"
cella task run test/b --timeout 120 -- bash -c 'codex exec "Create /tmp/test.txt with hello"'
cella task wait test/a
cella task wait test/b
cella exec test/a -- cat /tmp/test.txt
cella exec test/b -- cat /tmp/test.txt
```

**Expected results:**
- Both agents complete (status `done`)
- `/tmp/test.txt` exists in both containers with expected content
- No interference between agents

**Note:** This phase requires Claude Code and Codex to be installed in the containers. Skip if not available.

### Phase 5: Output Quality

```sh
cella up test/a 2>&1 | grep -c '{'    # expect 0
cella down test/a 2>&1 | grep -c '{'  # expect 0
```

**Expected results:**
- No JSON fragments leak into human-readable output (count should be 0)

### Phase 6: Container Lifecycle

```sh
cella down test/a
cella list | grep test/a    # should show "exited"
cella up test/a
cella list | grep test/a    # should show "running"
cella prune --dry-run --all # should list branches that would be pruned
cella down test/a --rm
cella down test/b --rm
cella list                   # test/a and test/b should be gone
cella task list              # task records for test/a and test/b should be gone
```

**Expected results:**
- After `cella down test/a`: container shows "exited" in `cella list`
- After `cella up test/a`: container shows "running" in `cella list`
- `cella prune --dry-run --all` exits 0 and lists individual branches that would be pruned
- After `cella down test/a --rm` and `cella down test/b --rm`: both containers, worktrees, and task records are removed
- If `down --rm` fails with `ContainerNotFound`, the container was already gone — use `cella prune --missing-worktree` to clean up the orphaned entry

### Phase 7: Edge Cases

```sh
cella branch test/deep/nested/name
cella exec test/deep/nested/name -- bash -c 'echo "quotes" && echo $HOME'
cella task run test/deep/nested/name --timeout 30 -- echo "rapid"
cella down test/deep/nested/name --rm
```

**Expected results:**
- Deeply nested branch names work for all operations
- Shell expansion works correctly inside exec
- Task dispatch works with nested branch names
- Cleanup removes everything

## Cleanup

**Always clean up test branches when done.** Run these even if tests fail partway through:

```sh
cella down test/a --rm 2>/dev/null
cella down test/b --rm 2>/dev/null
cella down test/deep/nested/name --rm 2>/dev/null
```

Verify cleanup:
```sh
cella list          # no test/* entries
cella task list     # no test/* entries
```

## When to Use

- After modifying the daemon, agent CLI, or task manager code
- Before releasing a new version
- After fixing bugs in the worktree/task system
- Validating parallel agent dispatch works end-to-end
- After upgrading Docker or the container runtime
- When `cella doctor` reports unexpected results
