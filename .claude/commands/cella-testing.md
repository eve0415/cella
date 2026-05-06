# Cella End-to-End Testing

Use when running a systematic test sweep of the cella worktree/task system from inside a container. Covers regression testing, agent dispatch validation, and stress testing.

## Prerequisites

- Inside a cella container (`cella doctor` exits 0)
- Agent version matches daemon version

## Test phases

### 1. Baseline diagnostics

```sh
cella doctor
cella doctor --json
cella list
cella list --json
```

Verify: exit 0, JSON parses, `*` on current container, version match.

### 2. Branch + exec

```sh
cella branch test/a
cella exec test/a -- echo "works"
cella exec test/a -- cella exec main -- echo "roundtrip"
cella branch test/b
cella exec test/a -- cella exec test/b -- echo "wt-to-wt"
```

Verify: branch creation completes, exec routing works bidirectionally, worktree-to-worktree routing works.

### 3. Task lifecycle

```sh
cella task run test/a -- echo "done"
cella task list
cella task run test/b --timeout 5 -- sleep 30
sleep 7
cella task list
cella task logs test/b
```

Verify: quick task shows "done" with 0s elapsed. Timeout task shows "timed_out" with elapsed frozen at 5s.

### 4. Agent dispatch

```sh
cella task run test/a --timeout 120 -- claude --dangerously-skip-permissions -p "Create /tmp/test.txt with 'hello'"
cella task run test/b --timeout 120 -- bash -c 'codex exec "Create /tmp/test.txt with hello"'
cella task wait test/a
cella task wait test/b
cella exec test/a -- cat /tmp/test.txt
cella exec test/b -- cat /tmp/test.txt
```

Verify: both agents complete, files exist, no interference.

### 5. Output quality

```sh
cella up test/a 2>&1 | grep -c '{'    # expect 0
cella down test/a 2>&1 | grep -c '{'  # expect 0
```

Verify: no JSON leak in human-readable output.

### 6. Lifecycle

```sh
cella down test/a
cella list | grep test/a    # should show "exited"
cella up test/a
cella list | grep test/a    # should show "running"
cella prune --dry-run --all # should list branches
cella down test/a --rm
cella down test/b --rm
cella task list              # stale records should be gone
```

### 7. Edge cases

```sh
cella branch test/deep/nested/name
cella exec test/deep/nested/name -- bash -c 'echo "quotes" && echo $HOME'
cella task run test/deep/nested/name --timeout 30 -- echo "rapid"
cella down test/deep/nested/name --rm
```

## Known agent dispatch requirements

- Claude Code: requires `--dangerously-skip-permissions` for headless mode
- Codex: multi-word prompts must be wrapped in `bash -c 'codex exec "prompt"'` because `cella task run` shell-splits args after `--`
- Claude Code emits "no stdin data received in 3s" warning — cosmetic, does not block

## When to use

- After modifying the daemon, agent CLI, or task manager
- Before releasing a new version
- After fixing bugs in the worktree/task system
- Validating parallel agent dispatch works end-to-end
