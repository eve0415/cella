/// Extract a `u16` port from a `forwardPorts` entry.
///
/// The devcontainer JSON schema constrains string items to the `"host:port"`
/// form (`^([a-z0-9-]+):(\d{1,5})$`). Bare numeric strings like `"9000"` are
/// not spec-defined but are accepted here as a compatibility extension — VS
/// Code and the official CLI silently coerce them to port numbers.
///
/// `"host:port"` strings (e.g. `"db:5432"`) are intentionally NOT reduced to
/// their port number: that port lives on another container reached via the
/// workspace container's network, so registering the bare number would forward
/// the wrong target. Such entries are left for full `host:port` support and
/// ignored by this number-only path.
pub fn forward_port_number(value: &serde_json::Value) -> Option<u16> {
    let n = value
        .as_u64()
        .or_else(|| value.as_str().and_then(|s| s.parse::<u64>().ok()))?;
    u16::try_from(n).ok()
}
