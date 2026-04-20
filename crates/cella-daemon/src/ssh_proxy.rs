//! Per-workspace SSH-agent proxy.
//!
//! Bridges the host's `$SSH_AUTH_SOCK` into a path under `~/.cella/run/` that
//! cella mounts into containers on colima. On colima the VM-side magic socket
//! `/run/host-services/ssh-auth.sock` is created by lima's OpenSSH agent
//! forwarding, which silently degenerates with sandboxed agents (1Password)
//! and can route to a connectable-but-empty agent. Owning the bridge here
//! removes that fragility — the daemon runs in the user's macOS context and
//! has full access to the real agent socket regardless of sandboxing.
//!
//! Lifecycle: refcounted per workspace folder. First `cella up` for a
//! workspace creates the proxy socket; subsequent ups for the same workspace
//! reuse it. The socket is unlinked when the refcount reaches zero.
