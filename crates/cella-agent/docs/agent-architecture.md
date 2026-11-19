# Agent Architecture

## Overview

The cella agent runs inside each devcontainer, providing port detection,
credential forwarding, browser integration, bidirectional clipboard
forwarding, reverse tunnel port forwarding, and SSH agent bridging by
communicating with the host daemon.

## Modes

### Connected Mode (default)

When a daemon address is available (either from `CELLA_DAEMON_ADDR` or
from `/cella/.daemon_addr`), the agent retries the connection
**indefinitely** and operates in full mode once it succeeds:

1. **Port watcher** — polls `/proc/net/tcp` and `/proc/net/tcp6` for
   listener changes, reports `PortOpen`/`PortClosed` to the daemon,
   receives `PortMapping` responses
2. **Credential forwarder** — intercepts `git credential` requests and
   forwards them to the daemon for host-side resolution
3. **Browser forwarder** — forwards `BrowserOpen` requests to the
   host browser (for OAuth callbacks, etc.)
4. **Health reporter** — sends periodic heartbeats with uptime and
   port count
5. **Clipboard forwarder** — intercepts `xsel`/`xclip` invocations
   (agent binary is symlinked as both) and forwards clipboard
   copy/paste operations to the daemon via `ClipboardCopy`/`ClipboardPaste`
   messages
6. **Reverse tunnel handler** — responds to `TunnelRequest` messages
   from the daemon by opening a TCP connection back to the daemon and
   relaying to the local service, enabling port forwarding on runtimes
   without direct container-IP routing (Colima, Docker Desktop for Mac)
7. **SSH agent bridge** — listens on a Unix socket inside the container
   (`/run/host-services/ssh-auth.sock`) and bridges each connection
   over TCP to the daemon, which connects to the host's real
   `SSH_AUTH_SOCK`. Required for Colima where bind-mounting host Unix
   sockets fails due to virtiofs limitations
8. **Worktree/task proxy** — in CLI mode (see below) forwards
   `branch`, `exec`, `prune`, `down`, `up`, `switch`, and `task run/list/
   logs/wait/stop` commands to the daemon for host-side execution

### Standalone Mode (fallback only when no address is configured)

Entered only when there is no daemon address at all. In this mode the
agent runs localhost→all-interfaces proxies locally but does not
communicate with any daemon. This is a last-resort mode — when a
daemon address exists but the daemon is temporarily unreachable, the
agent stays in connecting state and retries until the daemon comes
back (it does **not** fall through to standalone).

## Port Watcher (`port_watcher.rs`)

Polls `/proc/net/tcp` and `/proc/net/tcp6` at a configurable interval.

### Detection loop

Each cycle:
1. Scan current listeners
2. Diff against known set
3. For new listeners: send `PortOpen`, receive `PortMapping`, start local
   proxy if localhost-bound
4. For closed listeners: send `PortClosed`, stop local proxy

### Port mapping

When the daemon responds with `PortMapping`, the agent:
- Stores the mapping in a shared `HashMap<u16, u16>` (container → host)
- Writes the map to `/tmp/cella-port-map` as JSON

This allows child processes to discover how their ports are exposed on the
host (useful for OAuth callbacks, browser-open URLs, etc.).

### Reconnection

The agent uses `ReconnectingClient` which:
- **Retries the initial connection indefinitely** (`connect_with_retry`),
  logging a progress warning every 30 s so the log isn't silent. The
  agent never gives up — if the daemon is late coming up, the agent
  will still be there when it arrives.
- Re-reads `/cella/.daemon_addr` on every attempt, so a new daemon
  address (e.g. after a `cella up` that restarted the daemon or
  replaced the binary) is picked up automatically without restarting
  the agent.
- On a send failure during normal operation, attempts reconnect; on
  success, sets a `reconnected` flag so the port watcher re-reports
  all known ports and state is restored.

## Localhost Proxy (`port_proxy.rs`)

For listeners bound to `127.0.0.1` only:

```mermaid
graph LR
    A["0.0.0.0:PORT"] --> B["127.0.0.1:PORT"]
```

Runs inside the container so the service is reachable from the container's
external network interface. The daemon's host-side proxy then reaches this.

## BrowserOpen

When a process inside the container requests a browser open (via the agent's
API), the agent sends `AgentMessage::BrowserOpen { url }` to the daemon.

The daemon:
1. Rewrites the URL if the port is remapped
2. Waits for proxy readiness (up to 2 s)
3. Opens the URL in the host browser

## Credential Forwarding

Git credential requests are handled by the agent binary via its `credential`
subcommand. Git is configured with `credential.helper = /cella/bin/cella-agent credential`,
so credential requests flow:

```mermaid
graph LR
    git --> agent["cella-agent credential"] --> daemon --> host["host git credential"]
```

The agent assigns a unique ID to each request and waits for the matching
`CredentialResponse` from the daemon.

## Startup

The daemon runs as root under a supervisor loop, so the binary can be replaced in place (for example after `cella up` upgrades the agent) and the daemon can read the root-only `/tmp/.cella/proxy-config.json`.
The loop is defined once, by `supervisor_loop()` in `cella-backend::agent`, and is always launched by `spawn_agent_supervisor()` as `sh -c '<loop>' </dev/null >/dev/null 2>&1 &`, detached from the streams of whatever started it.
That command is always the body of an `if`, never the end of a list such as `a || <launch>`, because a trailing `&` backgrounds the whole list, and bash (the `sh` of Fedora and RHEL-family images) runs that list in a subshell that keeps the starter's streams open until the supervisor exits.

```sh
: cella-agent-supervisor; l=/run/cella-agent.log; true 2>/dev/null >>"$l" || l=/dev/null; while true; do [ "$l" = /dev/null ] || [ "$(wc -c 2>/dev/null <"$l")" -le 1048576 ] 2>/dev/null || true 2>/dev/null >"$l"; "/cella/bin/cella-agent" daemon --poll-interval "${CELLA_PORT_POLL_INTERVAL:-1000}" >>"$l" 2>&1; sleep 1; done
```

The loop appends the daemon's stdout and stderr to `/run/cella-agent.log` (`cella_env::AGENT_LOG_PATH`), the same file the compose path's `nohup` launch writes to.
The log lives in root-owned `/run` because a non-root user could create a name in sticky `/tmp` first, and on a host with `fs.protected_regular` set root's append to that file would then fail and the daemon would never start.
The loop picks the log once when it starts and uses `/dev/null` instead when the log cannot be opened, and it empties the log before a respawn once it has grown past 1 MiB.
The compose launch falls back to `/dev/null` the same way.
The opens use `true` rather than `:`, because a redirection that fails on a special built-in such as `:` makes `sh` exit.

The container entrypoint starts the supervisor only when it runs as root.
A non-root entrypoint (a Dockerfile ending in `USER vscode`, for example) starts nothing and leaves the agent to cella.

Two functions in `cella-backend::agent` start the agent from outside, both as a root exec that returns at once when the agent binary is missing.
`restart_agent_in_container()` kills a running daemon, which its supervisor respawns, removes `/tmp/.cella/proxy-ready`, and starts a supervisor if none is running.
It runs on every `cella up` of an existing container, after a daemon restart, and on a new container that got a proxy config, because the entrypoint's daemon started before the config was uploaded.
`ensure_agent_running()` only starts a supervisor if none is running and leaves a running daemon and its marker alone; it runs on a new container without a proxy config.
Both run as a blocking exec that returns when the script exits, which is as soon as the scan is done, because only the supervisor launch itself is backgrounded, so under dash and bash alike nothing left running holds the exec's streams.
A non-zero exit is logged as a warning.

After restarting the daemon on a new container with a proxy config, `cella up` waits for `/tmp/.cella/proxy-ready` before it runs lifecycle commands.
The daemon writes that marker once it has finished proxy setup: when the proxy is bound, or at once when `CELLA_NO_NETWORK_RULES` turns the proxy off.
The restart removes the marker after killing the daemon and before starting any supervisor, and the wait begins only after the restart has returned, so the marker it sees is the respawned daemon's and never one the killed daemon left behind.

Both find processes by scanning `/proc` with `tr`, `read` and `kill` alone, so the image needs no procps.
Each process's NUL-separated cmdline is joined with spaces and matched by prefix: `/cella/bin/cella-agent daemon` is the daemon and `sh -c : cella-agent-supervisor` is a supervisor.
The script's own `sh -c [ -x …` argv, which its subshells share, matches neither prefix.
The agent path is quoted inside the loop, so the unanchored `pkill -f 'cella-agent daemon'` of an older cella cannot kill the supervisor either.
A process is matched only when it shares the script's mount namespace, which is checked before its cmdline is read, because with `--pid=host` or a compose `pid:` the scan also sees other containers' daemons and supervisors.
The script compares the mount ID on the first line of `/proc/<pid>/mountinfo`, the namespace's root mount, rather than the `/proc/<pid>/ns/mnt` link, because reading that link needs `CAP_SYS_PTRACE` for a process of another user, such as the non-root daemon of a container created by an older cella, and Docker does not grant that capability by default.

Each daemon holds an exclusive `flock` on `/run/cella-agent.lock` for its whole lifetime, so a container never has more than one active daemon.
A duplicate daemon, such as one started by the entrypoint loop of a container created by an older cella, waits on the lock and takes over when the active daemon exits.
`/run` is root-owned, so a non-root user can neither create the lock first nor plant a symlink there, and the lock is opened without following symlinks.
Only a root daemon may create the lock file, so a non-root daemon cannot open it and exits.
A root daemon that cannot open or take the lock logs a warning and runs without it, since it is the daemon that has to serve the proxy.

Configuration sources, in priority order:

1. **`CELLA_DAEMON_ADDR`** / **`CELLA_CONTROL_TOKEN`** env vars,
   injected at container creation time.
2. **`/cella/.daemon_addr`** file, refreshed by the CLI on every
   `cella up` so agents that were running before a daemon restart pick
   up the new address without intervention.

## Clipboard Forwarding (`clipboard.rs`)

The agent binary is symlinked as `/cella/bin/xsel` and `/cella/bin/xclip`
inside the container. When invoked under either name, it parses the
command-line arguments to determine the clipboard operation:

- **Copy** (`xsel -i` / `xclip -i`): reads stdin (up to 10 MiB),
  base64-encodes it, and sends `AgentMessage::ClipboardCopy` to the daemon
- **Paste** (`xsel -o` / `xclip -o`): sends `AgentMessage::ClipboardPaste`
  to the daemon, receives `DaemonMessage::ClipboardContent`, base64-decodes,
  and writes to stdout
- **Clear** (`xsel -c`): sends a zero-length copy

The daemon handles the actual clipboard access on the host using
platform-specific backends (pbcopy/pbpaste on macOS, wl-copy/wl-paste
on Wayland, xsel on X11).

## Reverse Tunnel (`tunnel.rs`)

For runtimes where the host cannot directly reach container IPs (Colima,
Docker Desktop for Mac), the daemon sends `DaemonMessage::TunnelRequest`
messages to the agent instead of opening a direct TCP proxy. The agent
handles each request by:

1. Opening a new TCP connection back to the daemon's control port
2. Sending a `TunnelHandshake` with the auth token and connection ID
3. Connecting to `127.0.0.1:<target_port>` inside the container
4. Bidirectionally copying bytes between the daemon tunnel and the local
   service

This reverses the connection direction: instead of the daemon connecting
to the container, the container connects back to the daemon, working
around the lack of direct IP routing.

## SSH Agent Bridge (`ssh_agent_bridge.rs`)

Bridges the host's SSH agent into the container over TCP. Needed for
Colima where bind-mounting host Unix sockets fails (`mkdir` on virtiofs
rejects socket paths).

The bridge:
1. Creates a Unix socket at `/run/host-services/ssh-auth.sock` (or the
   configured path) with mode 0666
2. Accepts connections from `ssh-add`, `git`, etc.
3. For each connection, opens a TCP connection to
   `host.docker.internal:<bridge_port>`, sends the auth token, then
   bidirectionally copies SSH agent protocol bytes

The daemon side (`ssh_proxy.rs`) accepts these TCP connections and
forwards each to the host's real `SSH_AUTH_SOCK` via a Unix socket
connection.

## CLI Mode

The agent binary is symlinked to `/cella/bin/cella` inside the container
so that invoking `cella` from a shell prompt enters CLI mode instead of
agent-daemon mode. CLI subcommands (`branch`, `list`, `exec`, `down`,
`up`, `switch`, `prune`, `task run/list/logs/wait/stop`) are parsed by
`cli.rs` and converted to `AgentMessage` requests sent over the
already-established control connection. Each response is streamed back
from the host daemon (progress, stdout/stderr chunks, final result) and
rendered on the caller's terminal. Manual argument parsing keeps the
binary small (no clap dependency).
