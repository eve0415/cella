# Architecture

## System Overview

```
┌─────────────────────────────────────────────────┐
│                   cella-cli                      │
│         (command parsing, user output)           │
├──────────┬──────────┬──────────┬────────────────┤
│cella-git │cella-dock│cella-port│  cella-agent   │
│(worktree)│(container│(port     │  (AI sandbox   │
│          │ runtime) │ mgmt)    │   lifecycle)   │
├──────────┴──────────┴──────────┴────────────────┤
│                 cella-config                     │
│        (devcontainer.json, templates)            │
└─────────────────────────────────────────────────┘
```

## Crate Responsibilities

### cella-cli

The binary entry point. Handles argument parsing via clap, initializes tracing, and dispatches to the appropriate command handler. Contains no business logic — it delegates everything to the library crates.

### cella-config

Parses and manages devcontainer.json configuration files. Handles JSONC (comments + trailing commas), merges configuration layers (workspace, user, defaults), and provides type-safe access to schema fields. Uses typify for codegen from the devcontainer JSON Schema where possible.

### cella-docker

Abstracts container runtime operations. Manages the full container lifecycle (create, start, stop, remove), image building, and runtime detection. Designed as a trait-based abstraction to allow future support for alternative runtimes.

### cella-git

Handles git worktree operations and branch management. Creates, lists, and removes worktrees, manages the relationship between branches and their worktree directories, and coordinates with cella-docker to bind worktrees to containers.

### cella-port

Manages port allocation for dev containers. Handles auto-allocation to avoid conflicts between multiple concurrent containers, port forwarding setup, and configurable port ranges.

### cella-agent

Manages AI agent sandboxes. Handles agent preset configuration, sandbox creation and lifecycle management, resource isolation, and coordinates with other crates to provide agents with their own isolated worktree + container environments.

## Dependency Graph

```
cella-cli ──┬── cella-agent ──┬── cella-docker ── cella-config
            │                 ├── cella-git
            │                 ├── cella-port
            │                 └── cella-config
            ├── cella-docker
            ├── cella-git
            ├── cella-port
            └── cella-config
```

## Config Layer Merge Order

Configuration is resolved by merging layers from lowest to highest priority:

1. **Defaults** — built-in cella defaults
2. **Template** — values from the selected template
3. **Workspace** — `.devcontainer/devcontainer.json` in the repo
4. **User** — user-level overrides (`~/.config/cella/`)

Later layers override earlier ones for scalar values. Arrays and objects follow devcontainer spec merge semantics.

## Worktree-Container Binding

Each git worktree is bound to its own dev container instance. When you create a branch with `cella branch`, cella:

1. Creates a git worktree for the new branch
2. Resolves the devcontainer.json config for that worktree
3. Builds or pulls the container image
4. Starts the container with the worktree mounted
5. Allocates non-conflicting ports

This binding is tracked so that `cella switch` can stop/start the correct containers, and `cella prune` can clean up both the worktree and its container.
