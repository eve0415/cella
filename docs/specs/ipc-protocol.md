# IPC Protocol Specification

cella uses three IPC protocols for communication between host and container components:

1. **Agent <-> Daemon** -- TCP, for runtime communication between the in-container agent and the host daemon
2. **CLI <-> Daemon Management** -- Unix socket (`~/.cella/daemon.sock`), for CLI tooling to query and control the daemon
3. **Git Credential Helper** -- stdin/stdout, for forwarding git credentials from host to container

## Wire Format

Layers 1 and 2 use **newline-delimited JSON** (NDJSON): each message is a single JSON object terminated by `\n`. Messages must not contain embedded newlines. Enum messages are internally tagged via `#[serde(tag = "type", rename_all = "snake_case")]`, meaning every JSON object contains a `"type"` field that identifies the variant. Field names use `snake_case`.

Layer 3 (Git Credential Helper) does **not** use JSON. It uses the git credential helper protocol: `key=value` lines terminated by a blank line over stdin/stdout.

```
PROTOCOL_VERSION = 1
```

---

## Layer 1: Agent <-> Daemon (TCP)

### Connection Lifecycle

1. Agent opens a TCP connection to the daemon's control port.
2. Agent sends `AgentHello` as the first message.
3. Daemon responds with `DaemonHello`.
4. If `DaemonHello.error` is set, the daemon is rejecting the connection -- agent must disconnect.
5. On success, bidirectional `AgentMessage` / `DaemonMessage` exchange begins.

### Handshake Messages

#### AgentHello

Sent by the agent as the first message after connecting. Not internally tagged -- it is a plain struct.

| Field | Type | Description |
|---|---|---|
| `protocol_version` | `u32` | Must match `PROTOCOL_VERSION` (1) |
| `agent_version` | `string` | Agent binary version |
| `container_name` | `string` | Container name for routing (agent self-identifies) |
| `auth_token` | `string` | Auth token for validating the connection |

```json
{"protocol_version":1,"agent_version":"0.1.0","container_name":"cella-myapp-main","auth_token":"abc123"}
```

#### DaemonHello

Sent by the daemon in response to `AgentHello`. Not internally tagged.

| Field | Type | Description |
|---|---|---|
| `protocol_version` | `u32` | Daemon's protocol version |
| `daemon_version` | `string` | Daemon binary version |
| `error` | `string?` | If set, the daemon is rejecting the connection |
| `workspace_path` | `string?` | Host-side workspace path (from container label `dev.cella.workspace_path`) |
| `parent_repo` | `string?` | Host-side parent repo root (set when this container is a worktree) |
| `is_worktree` | `bool` | Whether this container is a worktree-backed branch container (default: `false`) |

```json
{"protocol_version":1,"daemon_version":"0.1.0","workspace_path":"/home/user/project","parent_repo":null,"is_worktree":false}
```

### Agent -> Daemon Messages (`AgentMessage`)

All variants are tagged with `"type"` in snake_case.

#### Port Management

**`port_open`** -- A new port listener was detected.

| Field | Type | Description |
|---|---|---|
| `port` | `u16` | Detected port number |
| `protocol` | `PortProtocol` | `"tcp"` or `"udp"` |
| `process` | `string?` | Process name (from `/proc/<pid>/cmdline`), if readable |
| `bind` | `BindAddress` | `"localhost"` or `"all"` |
| `proxy_port` | `u16?` | Agent-side localhost proxy port; when set, daemon connects to `container_ip:proxy_port` instead of `container_ip:port` |

```json
{"type":"port_open","port":3000,"protocol":"tcp","process":"node","bind":"localhost"}
```

**`port_closed`** -- A previously detected port listener has closed.

| Field | Type | Description |
|---|---|---|
| `port` | `u16` | Port number |
| `protocol` | `PortProtocol` | `"tcp"` or `"udp"` |

```json
{"type":"port_closed","port":3000,"protocol":"tcp"}
```

#### Browser Integration

**`browser_open`** -- Request to open a URL in the host browser.

| Field | Type | Description |
|---|---|---|
| `url` | `string` | URL to open |

```json
{"type":"browser_open","url":"https://github.com/login"}
```

#### Git Credentials

**`credential_request`** -- Git credential request forwarded from the container.

| Field | Type | Description |
|---|---|---|
| `id` | `string` | Unique request ID for correlating responses |
| `operation` | `string` | Git credential operation (e.g. `"get"`, `"store"`, `"erase"`) |
| `fields` | `map<string, string>` | Key-value credential fields (protocol, host, username, password, etc.) |

```json
{"type":"credential_request","id":"cred-1","operation":"get","fields":{"protocol":"https","host":"github.com"}}
```

#### Health

**`health`** -- Periodic health heartbeat.

| Field | Type | Description |
|---|---|---|
| `uptime_secs` | `u64` | Agent uptime in seconds |
| `ports_detected` | `usize` | Number of currently detected port listeners |

```json
{"type":"health","uptime_secs":120,"ports_detected":2}
```

#### Worktree Operations

These messages are sent from the in-container CLI (via agent) to the daemon for host-side worktree management.

**`branch_request`** -- Create a worktree-backed branch and its container.

| Field | Type | Description |
|---|---|---|
| `request_id` | `string` | Unique request ID |
| `branch` | `string` | Branch name to create |
| `base` | `string?` | Base branch/commit (omitted to use current HEAD) |

```json
{"type":"branch_request","request_id":"req-1","branch":"feature-x","base":"main"}
```

**`list_request`** -- List worktree branches and their container status.

| Field | Type | Description |
|---|---|---|
| `request_id` | `string` | Unique request ID |

```json
{"type":"list_request","request_id":"req-2"}
```

**`exec_request`** -- Execute a command in another branch's container.

| Field | Type | Description |
|---|---|---|
| `request_id` | `string` | Unique request ID |
| `branch` | `string` | Branch name whose container to exec in |
| `command` | `string[]` | Command and arguments to execute |

```json
{"type":"exec_request","request_id":"req-3","branch":"feature-x","command":["cargo","test"]}
```

**`prune_request`** -- Remove worktrees and their containers.

| Field | Type | Description |
|---|---|---|
| `request_id` | `string` | Unique request ID |
| `dry_run` | `bool` | Preview only, do not actually prune (default: `false`) |
| `all` | `bool` | Include unmerged worktrees, not just merged ones (default: `false`) |

```json
{"type":"prune_request","request_id":"req-4","dry_run":true,"all":false}
```

**`down_request`** -- Stop (and optionally remove) a worktree branch's container.

| Field | Type | Description |
|---|---|---|
| `request_id` | `string` | Unique request ID |
| `branch` | `string` | Branch name whose container to stop |
| `rm` | `bool` | Remove the container and worktree directory after stopping (default: `false`) |
| `volumes` | `bool` | Remove associated volumes (only with `rm`) (default: `false`) |
| `force` | `bool` | Force stop even when `shutdownAction` is `"none"` (default: `false`) |

```json
{"type":"down_request","request_id":"req-5","branch":"feature-x","rm":true,"volumes":false,"force":false}
```

**`up_request`** -- Start or restart a worktree branch's container.

| Field | Type | Description |
|---|---|---|
| `request_id` | `string` | Unique request ID |
| `branch` | `string` | Branch name whose container to start |
| `rebuild` | `bool` | Rebuild the container from scratch (default: `false`) |

```json
{"type":"up_request","request_id":"req-6","branch":"feature-x","rebuild":false}
```

#### Background Tasks

**`task_run_request`** -- Create a branch and run a background command in its container.

| Field | Type | Description |
|---|---|---|
| `request_id` | `string` | Unique request ID |
| `branch` | `string` | Branch name |
| `command` | `string[]` | Command and arguments to run |
| `base` | `string?` | Base branch/commit for branch creation |

```json
{"type":"task_run_request","request_id":"req-7","branch":"ci-run","command":["cargo","test"],"base":"main"}
```

**`task_list_request`** -- List active background tasks.

| Field | Type | Description |
|---|---|---|
| `request_id` | `string` | Unique request ID |

```json
{"type":"task_list_request","request_id":"req-8"}
```

**`task_logs_request`** -- Stream output from a background task.

| Field | Type | Description |
|---|---|---|
| `request_id` | `string` | Unique request ID |
| `branch` | `string` | Branch name |
| `follow` | `bool` | Keep streaming as new output arrives (default: `false`) |

```json
{"type":"task_logs_request","request_id":"req-9","branch":"ci-run","follow":true}
```

**`task_wait_request`** -- Block until a background task completes.

| Field | Type | Description |
|---|---|---|
| `request_id` | `string` | Unique request ID |
| `branch` | `string` | Branch name |

```json
{"type":"task_wait_request","request_id":"req-10","branch":"ci-run"}
```

**`task_stop_request`** -- Stop a running background task.

| Field | Type | Description |
|---|---|---|
| `request_id` | `string` | Unique request ID |
| `branch` | `string` | Branch name |

```json
{"type":"task_stop_request","request_id":"req-11","branch":"ci-run"}
```

#### Terminal

**`switch_request`** -- Switch to another branch's container (run default shell).

| Field | Type | Description |
|---|---|---|
| `request_id` | `string` | Unique request ID |
| `branch` | `string` | Branch name |

```json
{"type":"switch_request","request_id":"req-12","branch":"feature-x"}
```

### Daemon -> Agent Messages (`DaemonMessage`)

All variants are tagged with `"type"` in snake_case.

#### Control

**`ack`** -- Acknowledgment of a received message.

| Field | Type | Description |
|---|---|---|
| `id` | `string?` | ID of the acknowledged message, if applicable |

```json
{"type":"ack","id":"cred-1"}
```

**`config`** -- Configuration update from the daemon.

| Field | Type | Description |
|---|---|---|
| `poll_interval_ms` | `u64` | Port scanning poll interval in milliseconds |
| `proxy_localhost` | `bool` | Whether the agent should proxy localhost-bound ports |

```json
{"type":"config","poll_interval_ms":2000,"proxy_localhost":true}
```

#### Git Credentials

**`credential_response`** -- Response to a credential request.

| Field | Type | Description |
|---|---|---|
| `id` | `string` | Request ID (matches `CredentialRequest.id`) |
| `fields` | `map<string, string>` | Credential fields (protocol, host, username, password, etc.) |

```json
{"type":"credential_response","id":"cred-1","fields":{"protocol":"https","host":"github.com","username":"user","password":"token"}}
```

#### Port Mapping

**`port_mapping`** -- Tells the agent which host port was allocated for a container port.

| Field | Type | Description |
|---|---|---|
| `container_port` | `u16` | Port inside the container |
| `host_port` | `u16` | Allocated port on the host |

```json
{"type":"port_mapping","container_port":3000,"host_port":3001}
```

#### Operation Results

**`operation_progress`** -- Progress update for a long-running operation.

| Field | Type | Description |
|---|---|---|
| `request_id` | `string` | Correlates to the originating request |
| `step` | `string` | Current step identifier |
| `message` | `string` | Human-readable progress message |

```json
{"type":"operation_progress","request_id":"req-1","step":"creating_worktree","message":"Creating worktree for branch feature-x..."}
```

**`operation_output`** -- Streamed stdout/stderr from a long-running operation.

| Field | Type | Description |
|---|---|---|
| `request_id` | `string` | Correlates to the originating request |
| `stream` | `OutputStream` | `"stdout"` or `"stderr"` |
| `data` | `string` | Output chunk |

```json
{"type":"operation_output","request_id":"req-3","stream":"stdout","data":"running 42 tests\n"}
```

**`branch_result`** -- Result of a branch creation request. Uses flattened `WorktreeOperationResult`.

| Field | Type | Description |
|---|---|---|
| `request_id` | `string` | Correlates to the originating request |
| `status` | `string` | `"success"` or `"error"` |
| `container_name` | `string` | (success only) Container name of the new branch container |
| `worktree_path` | `string` | (success only) Host-side path to the worktree directory |
| `message` | `string` | (error only) Error description |

```json
{"type":"branch_result","request_id":"req-1","status":"success","container_name":"cella-myapp-feature-x","worktree_path":"/home/user/project-feature-x"}
```

**`list_result`** -- Result of a worktree list request.

| Field | Type | Description |
|---|---|---|
| `request_id` | `string` | Correlates to the originating request |
| `worktrees` | `WorktreeEntry[]` | List of worktree entries |

```json
{"type":"list_result","request_id":"req-2","worktrees":[{"branch":"main","worktree_path":"/home/user/project","is_main":true,"container_name":"cella-myapp-main","container_state":"running"}]}
```

**`exec_result`** -- Result of an exec request.

| Field | Type | Description |
|---|---|---|
| `request_id` | `string` | Correlates to the originating request |
| `exit_code` | `i32` | Exit code of the executed command |

```json
{"type":"exec_result","request_id":"req-3","exit_code":0}
```

**`prune_result`** -- Result of a prune request.

| Field | Type | Description |
|---|---|---|
| `request_id` | `string` | Correlates to the originating request |
| `pruned` | `string[]` | Branch names that were pruned |
| `errors` | `string[]` | Error messages for branches that failed to prune |

```json
{"type":"prune_result","request_id":"req-4","pruned":["old-feature"],"errors":[]}
```

**`down_result`** -- Result of a down (stop/remove) request. Uses flattened `DownOperationResult`.

| Field | Type | Description |
|---|---|---|
| `request_id` | `string` | Correlates to the originating request |
| `status` | `string` | `"success"` or `"error"` |
| `outcome` | `DownOutcome` | (success only) `"stopped"` or `"removed"` |
| `container_name` | `string` | (success only) Container name |
| `message` | `string` | (error only) Error description |

```json
{"type":"down_result","request_id":"req-5","status":"success","outcome":"removed","container_name":"cella-myapp-feature-x"}
```

**`up_result`** -- Result of an up (start/restart) request. Uses flattened `WorktreeOperationResult`.

| Field | Type | Description |
|---|---|---|
| `request_id` | `string` | Correlates to the originating request |
| `status` | `string` | `"success"` or `"error"` |
| `container_name` | `string` | (success only) Container name |
| `worktree_path` | `string` | (success only) Host-side worktree path |
| `message` | `string` | (error only) Error description |

```json
{"type":"up_result","request_id":"req-6","status":"success","container_name":"cella-myapp-feature-x","worktree_path":"/home/user/project-feature-x"}
```

#### Task Results

**`task_run_result`** -- A background task was started. Uses flattened `TaskRunOperationResult`.

| Field | Type | Description |
|---|---|---|
| `request_id` | `string` | Correlates to the originating request |
| `status` | `string` | `"success"` or `"error"` |
| `task_id` | `string` | (success only) Task identifier |
| `container_name` | `string` | (success only) Container running the task |
| `message` | `string` | (error only) Error description |

```json
{"type":"task_run_result","request_id":"req-7","status":"success","task_id":"ci-run","container_name":"cella-myapp-ci-run"}
```

**`task_list_result`** -- List of active background tasks.

| Field | Type | Description |
|---|---|---|
| `request_id` | `string` | Correlates to the originating request |
| `tasks` | `TaskEntry[]` | List of task entries |

```json
{"type":"task_list_result","request_id":"req-8","tasks":[{"task_id":"ci-run","branch":"ci-run","container_name":"cella-myapp-ci-run","status":"running","command":["cargo","test"],"elapsed_secs":45}]}
```

**`task_logs_data`** -- Background task output chunk (streaming).

| Field | Type | Description |
|---|---|---|
| `request_id` | `string` | Correlates to the originating request |
| `data` | `string` | Output chunk |
| `done` | `bool` | Whether this is the final chunk |

```json
{"type":"task_logs_data","request_id":"req-9","data":"test result: ok. 42 passed\n","done":true}
```

**`task_wait_result`** -- Background task completed.

| Field | Type | Description |
|---|---|---|
| `request_id` | `string` | Correlates to the originating request |
| `exit_code` | `i32` | Exit code of the completed task |

```json
{"type":"task_wait_result","request_id":"req-10","exit_code":0}
```

**`task_stop_result`** -- Background task stopped.

| Field | Type | Description |
|---|---|---|
| `request_id` | `string` | Correlates to the originating request |

```json
{"type":"task_stop_result","request_id":"req-11"}
```

#### Terminal

**`stream_ready`** -- Stream channel is ready for TTY forwarding.

| Field | Type | Description |
|---|---|---|
| `request_id` | `string` | Correlates to the originating request |
| `port` | `u16` | TCP port for the raw TTY stream |

```json
{"type":"stream_ready","request_id":"req-12","port":49152}
```

**`switch_result`** -- Result of a switch (shell exec in target container).

| Field | Type | Description |
|---|---|---|
| `request_id` | `string` | Correlates to the originating request |
| `exit_code` | `i32` | Exit code of the shell session |

```json
{"type":"switch_result","request_id":"req-12","exit_code":0}
```

### Supporting Types

#### Enums

**`PortProtocol`** -- Transport protocol for a port. Serialized as lowercase string.

| Variant | Wire Value |
|---|---|
| `Tcp` | `"tcp"` |
| `Udp` | `"udp"` |

**`BindAddress`** -- Whether a listener binds to localhost only or all interfaces. Serialized as lowercase string.

| Variant | Wire Value | Description |
|---|---|---|
| `Localhost` | `"localhost"` | Bound to `127.0.0.1` / `::1` only |
| `All` | `"all"` | Bound to `0.0.0.0` / `::` (all interfaces) |

**`OnAutoForward`** -- Behavior when a port is auto-detected. Matches the devcontainer spec `onAutoForward` values. Serialized as camelCase string.

| Variant | Wire Value | Description |
|---|---|---|
| `Notify` | `"notify"` | Show a notification (default) |
| `OpenBrowser` | `"openBrowser"` | Open in the default browser |
| `OpenBrowserOnce` | `"openBrowserOnce"` | Open in browser once (first detection only) |
| `OpenPreview` | `"openPreview"` | Open in a preview panel (treated as `openBrowser` in CLI context) |
| `Silent` | `"silent"` | Forward silently without notification |
| `Ignore` | `"ignore"` | Do not forward this port |

**`OutputStream`** -- Which output stream a chunk came from. Serialized as lowercase string.

| Variant | Wire Value |
|---|---|
| `Stdout` | `"stdout"` |
| `Stderr` | `"stderr"` |

**`TaskStatus`** -- Status of a background task. Serialized as snake_case string.

| Variant | Wire Value |
|---|---|
| `Running` | `"running"` |
| `Done` | `"done"` |
| `Failed` | `"failed"` |

**`DownOutcome`** -- Outcome of a container stop/remove operation. Serialized as snake_case string.

| Variant | Wire Value |
|---|---|
| `Stopped` | `"stopped"` |
| `Removed` | `"removed"` |

**`WorktreeOperationResult`** -- Tagged enum (`"status"` field) used in `BranchResult` and `UpResult` via `#[serde(flatten)]`.

| Status | Fields |
|---|---|
| `"success"` | `container_name: string`, `worktree_path: string` |
| `"error"` | `message: string` |

**`DownOperationResult`** -- Tagged enum (`"status"` field) used in `DownResult` via `#[serde(flatten)]`.

| Status | Fields |
|---|---|
| `"success"` | `outcome: DownOutcome`, `container_name: string` |
| `"error"` | `message: string` |

**`TaskRunOperationResult`** -- Tagged enum (`"status"` field) used in `TaskRunResult` via `#[serde(flatten)]`.

| Status | Fields |
|---|---|
| `"success"` | `task_id: string`, `container_name: string` |
| `"error"` | `message: string` |

#### Structs

**`PortAttributes`** -- Per-port attributes from devcontainer.json `portsAttributes`.

| Field | Type | Default | Description |
|---|---|---|---|
| `port` | `PortPattern` | `Single(0)` | Port number or pattern this applies to |
| `on_auto_forward` | `OnAutoForward` | `Notify` | What to do when this port is auto-detected |
| `label` | `string?` | `null` | Display label for this port |
| `protocol` | `string?` | `null` | Protocol hint for URL generation |
| `require_local_port` | `bool` | `false` | Whether the exact host port is required (fail if unavailable) |
| `elevate_if_needed` | `bool` | `false` | Whether to attempt elevated access for privileged ports |

**`PortPattern`** -- A port pattern for matching detected ports. Externally tagged enum (default serde representation).

| Variant | JSON Representation | Description |
|---|---|---|
| `Single(u16)` | `{"Single": 3000}` | Exact port number |
| `Range(u16, u16)` | `{"Range": [3000, 3005]}` | Inclusive port range `[lo, hi]` |

**`TaskEntry`** -- A background task entry.

| Field | Type | Description |
|---|---|---|
| `task_id` | `string` | Task identifier (typically the branch name) |
| `branch` | `string` | Branch this task is running in |
| `container_name` | `string` | Container running the task |
| `status` | `TaskStatus` | Task status |
| `command` | `string[]` | Command being run |
| `elapsed_secs` | `u64` | Seconds since the task started |

**`WorktreeEntry`** -- A worktree entry for list responses.

| Field | Type | Description |
|---|---|---|
| `branch` | `string?` | Branch name |
| `worktree_path` | `string` | Host-side worktree path |
| `is_main` | `bool` | Whether this is the main (non-linked) worktree |
| `container_name` | `string?` | Associated container name, if any |
| `container_state` | `string?` | Container state (running, exited, etc.), if a container exists |

---

## Layer 2: CLI <-> Daemon Management (Unix Socket)

Management messages use the same NDJSON wire format over a Unix domain socket at `~/.cella/daemon.sock`. Each request gets exactly one response.

### ManagementRequest

All variants are tagged with `"type"` in snake_case.

**`register_container`** -- Register a new container for port management.

| Field | Type | Description |
|---|---|---|
| `container_id` | `string` | Docker container ID |
| `container_name` | `string` | Container name |
| `container_ip` | `string?` | Container IP address (may be `null` during pre-registration before the container has started; see `update_container_ip`) |
| `ports_attributes` | `PortAttributes[]` | Per-port forwarding attributes from devcontainer.json |
| `other_ports_attributes` | `PortAttributes?` | Default attributes for ports not matched by `ports_attributes` |
| `forward_ports` | `u16[]` | Ports from `forwardPorts` in devcontainer.json (pre-allocate on registration, default: `[]`) |
| `shutdown_action` | `string?` | The `shutdownAction` from devcontainer.json (`"none"`, `"stopContainer"`, or `"stopCompose"` for compose workspaces) |
| `backend_kind` | `string?` | Backend that created the container (`"docker"`, `"apple-container"`). Defaults to `null` for backward compatibility with older CLIs |
| `docker_host` | `string?` | Docker host override used when the container was created. Defaults to `null` |

```json
{"type":"register_container","container_id":"abc123","container_name":"cella-myapp-main","container_ip":"172.20.0.5","ports_attributes":[],"other_ports_attributes":null,"forward_ports":[],"shutdown_action":null,"backend_kind":"docker","docker_host":null}
```

**`deregister_container`** -- Deregister a container (stop proxies, release ports).

| Field | Type | Description |
|---|---|---|
| `container_name` | `string` | Container name to deregister |

```json
{"type":"deregister_container","container_name":"cella-myapp-main"}
```

**`query_ports`** -- Query all forwarded ports across containers. No fields.

```json
{"type":"query_ports"}
```

**`query_status`** -- Query daemon status. No fields.

```json
{"type":"query_status"}
```

**`ping`** -- Health check. No fields.

```json
{"type":"ping"}
```

**`update_container_ip`** -- Update a container's IP address after it has started. Sent after pre-registration (with `container_ip: null`) once the container is running and its IP is known.

| Field | Type | Description |
|---|---|---|
| `container_id` | `string` | Docker container ID |
| `container_ip` | `string?` | Newly discovered container IP |

```json
{"type":"update_container_ip","container_id":"abc123","container_ip":"172.20.0.5"}
```

**`shutdown`** -- Request graceful shutdown of the daemon. No fields.

```json
{"type":"shutdown"}
```

### ManagementResponse

All variants are tagged with `"type"` in snake_case.

**`container_registered`** -- Container successfully registered.

| Field | Type | Description |
|---|---|---|
| `container_name` | `string` | Registered container name |

```json
{"type":"container_registered","container_name":"cella-myapp-main"}
```

**`container_deregistered`** -- Container deregistered.

| Field | Type | Description |
|---|---|---|
| `container_name` | `string` | Deregistered container name |
| `ports_released` | `usize` | Number of ports that were released |

```json
{"type":"container_deregistered","container_name":"cella-myapp-main","ports_released":3}
```

**`ports`** -- Forwarded port listing.

| Field | Type | Description |
|---|---|---|
| `ports` | `ForwardedPortDetail[]` | All forwarded ports across all containers |

```json
{"type":"ports","ports":[{"container_name":"cella-myapp-main","container_port":3000,"host_port":3000,"protocol":"tcp","process":"node","url":"localhost:3000"}]}
```

**`status`** -- Daemon status.

| Field | Type | Description |
|---|---|---|
| `pid` | `u32` | Daemon process ID |
| `uptime_secs` | `u64` | Daemon uptime in seconds |
| `container_count` | `usize` | Number of registered containers |
| `containers` | `ContainerSummary[]` | Per-container summaries |
| `is_orbstack` | `bool` | Whether the Docker runtime is OrbStack |
| `daemon_version` | `string` | Daemon binary version (default: `""`) |
| `daemon_started_at` | `u64` | Unix timestamp when daemon started (default: `0`) |
| `control_port` | `u16` | TCP control port for agent connections (default: `0`) |
| `control_token` | `string` | Auth token for agent connections (default: `""`) |

```json
{"type":"status","pid":12345,"uptime_secs":3600,"container_count":2,"containers":[],"is_orbstack":true,"daemon_version":"0.1.0","daemon_started_at":1711929600,"control_port":9876,"control_token":"secret"}
```

**`shutting_down`** -- Daemon is shutting down.

| Field | Type | Description |
|---|---|---|
| `pid` | `u32` | Daemon process ID |

```json
{"type":"shutting_down","pid":12345}
```

**`container_ip_updated`** -- Container IP update acknowledged.

| Field | Type | Description |
|---|---|---|
| `container_id` | `string` | Docker container ID the update was applied to |

```json
{"type":"container_ip_updated","container_id":"abc123"}
```

**`pong`** -- Pong response. No fields.

```json
{"type":"pong"}
```

**`error`** -- Error response.

| Field | Type | Description |
|---|---|---|
| `message` | `string` | Error description |

```json
{"type":"error","message":"container not found: cella-myapp-main"}
```

### Supporting Types

**`ForwardedPortDetail`** -- Detail about a single forwarded port.

| Field | Type | Description |
|---|---|---|
| `container_name` | `string` | Container this port belongs to |
| `container_port` | `u16` | Port inside the container |
| `host_port` | `u16` | Port on the host |
| `protocol` | `PortProtocol` | `"tcp"` or `"udp"` |
| `process` | `string?` | Process name, if known |
| `url` | `string` | URL for accessing this port |

**`ContainerSummary`** -- Summary of a registered container.

| Field | Type | Description |
|---|---|---|
| `container_name` | `string` | Container name |
| `container_id` | `string` | Docker container ID |
| `forwarded_port_count` | `usize` | Number of currently forwarded ports |
| `agent_connected` | `bool` | Whether the agent TCP connection is active |
| `last_seen_secs` | `u64` | Seconds since last agent heartbeat (default: `0`) |
| `agent_version` | `string?` | Agent version from the `AgentHello` handshake, if connected (default: `null`) |

---

## Layer 3: Git Credential Helper

The credential helper protocol uses the standard git credential helper format over stdin/stdout. This is not JSON -- it uses a line-oriented key=value format as specified by `git-credential(1)`.

### Format

Each field is a `key=value` pair on its own line. A blank line terminates the message.

```
protocol=https
host=github.com
username=user
password=ghp_xxxx

```

### Parsing Rules

- Each non-empty line must contain `key=value` (split on the first `=`).
- Lines without `=` are silently skipped.
- A blank line (empty string or just `\n`) terminates the field set.
- Order of fields is not significant.

### Common Fields

| Field | Description |
|---|---|
| `protocol` | Transport protocol (e.g. `https`, `http`) |
| `host` | Hostname (e.g. `github.com`) |
| `username` | Username |
| `password` | Password or token |
| `path` | Repository path on the host |

### Flow

1. Git invokes the credential helper with an operation (`get`, `store`, or `erase`).
2. The helper reads key=value fields from stdin (terminated by blank line).
3. For `get`: the helper writes back key=value fields (with `username` and `password` filled in) terminated by a blank line.
4. In cella, the in-container credential helper sends a `credential_request` via the agent to the daemon, which runs the host-side git credential helper and returns the result as a `credential_response`.

### Wire Example (stdin to helper)

```
protocol=https
host=github.com

```

### Wire Example (stdout from helper)

```
protocol=https
host=github.com
username=user
password=ghp_xxxx

```
