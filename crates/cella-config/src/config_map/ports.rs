use std::collections::HashMap;

use cella_backend::PortForward;
use cella_protocol::{OnAutoForward, PortAttributes, PortPattern};

pub(super) fn map_port_bindings(config: &serde_json::Value) -> HashMap<String, Vec<PortForward>> {
    let ports_attrs = config.get("portsAttributes").and_then(|v| v.as_object());
    let mut bindings = HashMap::new();

    // forwardPorts: array of number | string, bound to 0.0.0.0
    if let Some(ports) = config.get("forwardPorts").and_then(|v| v.as_array()) {
        for port_value in ports {
            let port = match port_value {
                serde_json::Value::Number(n) => n.to_string(),
                serde_json::Value::String(s) => s.clone(),
                _ => continue,
            };

            let protocol = ports_attrs
                .and_then(|attrs| attrs.get(&port))
                .and_then(|attr| attr.get("protocol"))
                .and_then(|v| v.as_str())
                .unwrap_or("tcp");

            let container_port = format!("{port}/{protocol}");

            bindings
                .entry(container_port)
                .or_insert_with(Vec::new)
                .push(PortForward {
                    host_ip: Some("0.0.0.0".to_string()),
                    host_port: Some(port),
                });
        }
    }

    // appPort: number | string | array thereof, bound to 127.0.0.1 (official behaviour)
    //
    // Skip any appPort whose host port is already claimed by a forwardPorts 0.0.0.0
    // binding — Docker allocates all interfaces (including loopback) when the wildcard
    // address is used, so adding a second 127.0.0.1 binding for the same host port
    // causes "port already allocated" at container creation time.
    //
    // Collect into an owned set so the immutable borrow of `bindings` ends
    // before the mutable `entry` calls below.
    let forward_host_ports: std::collections::HashSet<String> = bindings
        .values()
        .flatten()
        .filter(|fwd| fwd.host_ip.as_deref() == Some("0.0.0.0"))
        .filter_map(|fwd| fwd.host_port.clone())
        .collect();

    let app_port_entries = collect_app_port_entries(config);
    for (container_port_key, forward) in app_port_entries {
        // Skip if the appPort's host port would collide with an existing 0.0.0.0 binding.
        let collides = forward
            .host_port
            .as_deref()
            .is_some_and(|hp| forward_host_ports.contains(hp));
        if collides {
            continue;
        }
        bindings
            .entry(container_port_key)
            .or_insert_with(Vec::new)
            .push(forward);
    }

    bindings
}

/// Collect `PortForward` entries from the `appPort` field.
///
/// Official behaviour (devcontainers/cli `singleContainer.ts`):
/// - NUMBER `N`  → `-p 127.0.0.1:N:N`  (host 127.0.0.1:N → container N/tcp)
/// - STRING `s`  → `-p s` verbatim — `s` is a Docker `-p` spec:
///   `[ip:][hostPort:]containerPort[/proto]`
///
/// For strings we parse the Docker `-p` format ourselves so we can fill in
/// the `PortForward` struct. Bare `"N"` (single part) maps to container port N
/// with `host_port: None` so Docker assigns an ephemeral host port — matching
/// Docker's own behaviour when you write `-p N` without a host side.
fn collect_app_port_entries(config: &serde_json::Value) -> Vec<(String, PortForward)> {
    let Some(raw) = config.get("appPort") else {
        return Vec::new();
    };

    let items: Vec<&serde_json::Value> = match raw {
        serde_json::Value::Array(arr) => arr.iter().collect(),
        other => vec![other],
    };

    items
        .into_iter()
        .filter_map(app_port_item_to_forward)
        .collect()
}

/// Convert a single `appPort` element (number or string) to a
/// `(container_port_key, PortForward)` pair.
fn app_port_item_to_forward(value: &serde_json::Value) -> Option<(String, PortForward)> {
    match value {
        serde_json::Value::Number(n) => {
            let port = n.as_u64()?.to_string();
            let key = format!("{port}/tcp");
            Some((
                key,
                PortForward {
                    host_ip: Some("127.0.0.1".to_string()),
                    host_port: Some(port),
                },
            ))
        }
        serde_json::Value::String(s) => parse_docker_p_spec(s),
        _ => None,
    }
}

/// Parse a Docker `-p` spec string: `[ip:][hostPort:]containerPort[/proto]`
///
/// Returns `(container_port_key, PortForward)` where the key is
/// `"containerPort/proto"` (defaulting to tcp).
///
/// Examples:
/// - `"3000"`             → key `"3000/tcp"`, `host_ip` None, `host_port` None
/// - `"8080:80"`          → key `"80/tcp"`, `host_ip` None, `host_port` Some("8080")
/// - `"127.0.0.1:3000:3000"` → key `"3000/tcp"`, `host_ip` Some("127.0.0.1"), `host_port` Some("3000")
/// - `"[::1]:8080:80"`    → key `"80/tcp"`, `host_ip` `Some("::1")`, `host_port` `Some("8080")`
/// - `"5000/udp"`         → key `"5000/udp"`, `host_ip` None, `host_port` None
fn parse_docker_p_spec(spec: &str) -> Option<(String, PortForward)> {
    // Split off optional /proto suffix.
    // An empty proto (e.g. "80/") is not a valid Docker -p spec — reject it.
    let (addr_part, proto) = match spec.rsplit_once('/') {
        Some((_, "")) => return None,
        Some((a, p)) => (a, p),
        None => (spec, "tcp"),
    };

    let (host_ip, host_port, container_port) = if addr_part.starts_with('[') {
        // Bracketed IPv6: "[ip]:hostPort:containerPort" (hostPort may be empty).
        parse_bracketed_ipv6(addr_part)?
    } else {
        match addr_part.splitn(3, ':').collect::<Vec<_>>().as_slice() {
            [cp] => (None, None, (*cp).to_string()),
            [hp, cp] => (None, Some((*hp).to_string()), (*cp).to_string()),
            [ip, hp, cp] => (
                Some((*ip).to_string()),
                Some((*hp).to_string()),
                (*cp).to_string(),
            ),
            _ => return None,
        }
    };

    if container_port.is_empty() {
        return None;
    }

    // An empty host IP / host port means "let Docker choose" — normalize to None
    // so the binding is structurally unambiguous.
    let host_ip = host_ip.filter(|s| !s.is_empty());
    let host_port = host_port.filter(|s| !s.is_empty());

    let key = format!("{container_port}/{proto}");
    Some((key, PortForward { host_ip, host_port }))
}

/// Parse a bracketed-IPv6 Docker `-p` address part: `[ip]:hostPort:containerPort`.
/// Returns the bare IP (no brackets), host port, and container port.
fn parse_bracketed_ipv6(addr_part: &str) -> Option<(Option<String>, Option<String>, String)> {
    let inner = addr_part.strip_prefix('[')?;
    let (ip, after) = inner.split_once(']')?;
    // Docker requires the host-port slot when an IP is given (it may be empty):
    // the tail is ":hostPort:containerPort".
    let tail = after.strip_prefix(':')?;
    let (host_port, container_port) = tail.split_once(':')?;
    Some((
        Some(ip.to_string()),
        Some(host_port.to_string()),
        container_port.to_string(),
    ))
}

/// Parse `portsAttributes` from devcontainer.json.
///
/// Returns a list of `PortAttributes` for the daemon to use when
/// deciding how to handle auto-detected ports.
///
/// Example devcontainer.json:
/// ```json
/// {
///   "portsAttributes": {
///     "3000": {"label": "Web App", "onAutoForward": "openBrowser"},
///     "5432": {"onAutoForward": "silent"},
///     "9229": {"onAutoForward": "ignore"}
///   },
///   "otherPortsAttributes": {"onAutoForward": "silent"}
/// }
/// ```
pub fn parse_ports_attributes(config: &serde_json::Value) -> Vec<PortAttributes> {
    let Some(attrs_obj) = config.get("portsAttributes").and_then(|v| v.as_object()) else {
        return Vec::new();
    };

    attrs_obj
        .iter()
        .filter_map(|(key, value)| parse_single_port_attributes(key, value))
        .collect()
}

/// Parse `otherPortsAttributes` (default for unmatched ports).
pub fn parse_other_ports_attributes(config: &serde_json::Value) -> Option<PortAttributes> {
    let value = config.get("otherPortsAttributes")?;
    let obj = value.as_object()?;

    let on_auto_forward = obj
        .get("onAutoForward")
        .and_then(|v| v.as_str())
        .map(parse_on_auto_forward)
        .unwrap_or_default();

    let label = obj.get("label").and_then(|v| v.as_str()).map(String::from);

    let protocol = obj
        .get("protocol")
        .and_then(|v| v.as_str())
        .map(String::from);

    let require_local_port = obj
        .get("requireLocalPort")
        .and_then(serde_json::Value::as_bool)
        .unwrap_or(false);

    let elevate_if_needed = obj
        .get("elevateIfNeeded")
        .and_then(serde_json::Value::as_bool)
        .unwrap_or(false);

    Some(PortAttributes {
        port: PortPattern::Range(0, u16::MAX), // matches all ports
        on_auto_forward,
        label,
        protocol,
        require_local_port,
        elevate_if_needed,
    })
}

/// Parse a single port key + attributes object.
fn parse_single_port_attributes(key: &str, value: &serde_json::Value) -> Option<PortAttributes> {
    let obj = value.as_object()?;
    let port = parse_port_pattern(key)?;

    let on_auto_forward = obj
        .get("onAutoForward")
        .and_then(|v| v.as_str())
        .map(parse_on_auto_forward)
        .unwrap_or_default();

    let label = obj.get("label").and_then(|v| v.as_str()).map(String::from);

    let protocol = obj
        .get("protocol")
        .and_then(|v| v.as_str())
        .map(String::from);

    let require_local_port = obj
        .get("requireLocalPort")
        .and_then(serde_json::Value::as_bool)
        .unwrap_or(false);

    let elevate_if_needed = obj
        .get("elevateIfNeeded")
        .and_then(serde_json::Value::as_bool)
        .unwrap_or(false);

    Some(PortAttributes {
        port,
        on_auto_forward,
        label,
        protocol,
        require_local_port,
        elevate_if_needed,
    })
}

/// Parse a port key into a `PortPattern`.
///
/// Supports:
/// - `"3000"` -> Single(3000)
/// - `"3000-3010"` -> Range(3000, 3010)
fn parse_port_pattern(key: &str) -> Option<PortPattern> {
    if let Some((start, end)) = key.split_once('-') {
        let start: u16 = start.trim().parse().ok()?;
        let end: u16 = end.trim().parse().ok()?;
        Some(PortPattern::Range(start, end))
    } else {
        let port: u16 = key.trim().parse().ok()?;
        Some(PortPattern::Single(port))
    }
}

/// Parse `onAutoForward` string value to enum.
fn parse_on_auto_forward(value: &str) -> OnAutoForward {
    match value {
        "openBrowser" => OnAutoForward::OpenBrowser,
        "openBrowserOnce" => OnAutoForward::OpenBrowserOnce,
        "openPreview" => OnAutoForward::OpenPreview,
        "silent" => OnAutoForward::Silent,
        "ignore" => OnAutoForward::Ignore,
        _ => OnAutoForward::Notify,
    }
}

/// Serialize port attributes to a JSON string for storage in container labels.
///
/// The daemon reads these labels to know how to handle auto-detected ports
/// without re-resolving the devcontainer.json config.
pub fn serialize_ports_attributes_label(
    attrs: &[PortAttributes],
    other: Option<&PortAttributes>,
) -> String {
    let value = serde_json::json!({
        "portsAttributes": attrs,
        "otherPortsAttributes": other,
    });
    serde_json::to_string(&value).unwrap_or_default()
}

/// Deserialize port attributes from a container label value.
///
/// Parses the JSON produced by [`serialize_ports_attributes_label`] back into
/// `(Vec<PortAttributes>, Option<PortAttributes>)`.
///
/// Returns `(vec![], None)` if the label is empty or unparseable.
pub fn deserialize_ports_attributes_label(
    label: &str,
) -> (Vec<PortAttributes>, Option<PortAttributes>) {
    let Ok(value) = serde_json::from_str::<serde_json::Value>(label) else {
        return (Vec::new(), None);
    };

    let ports_attributes: Vec<PortAttributes> = value
        .get("portsAttributes")
        .and_then(|v| serde_json::from_value(v.clone()).ok())
        .unwrap_or_default();

    let other_ports_attributes: Option<PortAttributes> =
        value.get("otherPortsAttributes").and_then(|v| {
            if v.is_null() {
                None
            } else {
                serde_json::from_value(v.clone()).ok()
            }
        });

    (ports_attributes, other_ports_attributes)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn parse_single_port() {
        let pattern = parse_port_pattern("3000").unwrap();
        assert_eq!(pattern, PortPattern::Single(3000));
    }

    #[test]
    fn parse_range_port() {
        let pattern = parse_port_pattern("3000-3010").unwrap();
        assert_eq!(pattern, PortPattern::Range(3000, 3010));
    }

    #[test]
    fn parse_invalid_port() {
        assert!(parse_port_pattern("abc").is_none());
    }

    #[test]
    fn parse_ports_attributes_basic() {
        let config = json!({
            "portsAttributes": {
                "3000": {"label": "Web", "onAutoForward": "openBrowser"},
                "9229": {"onAutoForward": "ignore"},
            }
        });

        let attrs = parse_ports_attributes(&config);
        assert_eq!(attrs.len(), 2);

        let web = attrs
            .iter()
            .find(|a| a.label.as_deref() == Some("Web"))
            .unwrap();
        assert_eq!(web.on_auto_forward, OnAutoForward::OpenBrowser);
        assert!(matches!(web.port, PortPattern::Single(3000)));

        let debug = attrs
            .iter()
            .find(|a| matches!(a.port, PortPattern::Single(9229)))
            .unwrap();
        assert_eq!(debug.on_auto_forward, OnAutoForward::Ignore);
    }

    #[test]
    fn parse_other_ports_attributes_basic() {
        let config = json!({
            "otherPortsAttributes": {
                "onAutoForward": "silent",
            }
        });

        let other = parse_other_ports_attributes(&config).unwrap();
        assert_eq!(other.on_auto_forward, OnAutoForward::Silent);
    }

    #[test]
    fn parse_other_ports_attributes_missing() {
        let config = json!({"image": "ubuntu"});
        assert!(parse_other_ports_attributes(&config).is_none());
    }

    #[test]
    fn on_auto_forward_all_values() {
        assert_eq!(parse_on_auto_forward("notify"), OnAutoForward::Notify);
        assert_eq!(
            parse_on_auto_forward("openBrowser"),
            OnAutoForward::OpenBrowser
        );
        assert_eq!(
            parse_on_auto_forward("openBrowserOnce"),
            OnAutoForward::OpenBrowserOnce
        );
        assert_eq!(
            parse_on_auto_forward("openPreview"),
            OnAutoForward::OpenPreview
        );
        assert_eq!(parse_on_auto_forward("silent"), OnAutoForward::Silent);
        assert_eq!(parse_on_auto_forward("ignore"), OnAutoForward::Ignore);
        assert_eq!(parse_on_auto_forward("unknown"), OnAutoForward::Notify);
    }

    #[test]
    fn require_local_port_parsed() {
        let config = json!({
            "portsAttributes": {
                "5432": {"requireLocalPort": true},
            }
        });

        let attrs = parse_ports_attributes(&config);
        assert_eq!(attrs.len(), 1);
        assert!(attrs[0].require_local_port);
    }

    #[test]
    fn serialize_roundtrip() {
        let attrs = vec![PortAttributes {
            port: PortPattern::Single(3000),
            on_auto_forward: OnAutoForward::OpenBrowser,
            label: Some("Web".to_string()),
            ..PortAttributes::default()
        }];
        let label = serialize_ports_attributes_label(&attrs, None);
        assert!(label.contains("portsAttributes"));
        assert!(label.contains("3000"));
    }

    #[test]
    fn deserialize_roundtrip() {
        let attrs = vec![PortAttributes {
            port: PortPattern::Single(3000),
            on_auto_forward: OnAutoForward::OpenBrowser,
            label: Some("Web".to_string()),
            ..PortAttributes::default()
        }];
        let other = PortAttributes {
            port: PortPattern::Range(0, u16::MAX),
            on_auto_forward: OnAutoForward::Silent,
            ..PortAttributes::default()
        };

        let label = serialize_ports_attributes_label(&attrs, Some(&other));
        let (deserialized_attrs, deserialized_other) = deserialize_ports_attributes_label(&label);

        assert_eq!(deserialized_attrs.len(), 1);
        assert!(matches!(
            deserialized_attrs[0].port,
            PortPattern::Single(3000)
        ));
        assert_eq!(
            deserialized_attrs[0].on_auto_forward,
            OnAutoForward::OpenBrowser
        );

        let other = deserialized_other.unwrap();
        assert_eq!(other.on_auto_forward, OnAutoForward::Silent);
    }

    #[test]
    fn deserialize_empty_label() {
        let (attrs, other) = deserialize_ports_attributes_label("");
        assert!(attrs.is_empty());
        assert!(other.is_none());
    }

    #[test]
    fn deserialize_no_other() {
        let attrs = vec![PortAttributes {
            port: PortPattern::Single(8080),
            ..PortAttributes::default()
        }];
        let label = serialize_ports_attributes_label(&attrs, None);
        let (deserialized, other) = deserialize_ports_attributes_label(&label);
        assert_eq!(deserialized.len(), 1);
        assert!(other.is_none());
    }

    // ── appPort tests ─────────────────────────────────────────────────────────

    #[test]
    fn app_port_number_binds_loopback() {
        let config = json!({"appPort": 3000});
        let bindings = map_port_bindings(&config);
        let fwd = &bindings["3000/tcp"];
        assert_eq!(fwd.len(), 1);
        assert_eq!(fwd[0].host_ip.as_deref(), Some("127.0.0.1"));
        assert_eq!(fwd[0].host_port.as_deref(), Some("3000"));
    }

    #[test]
    fn app_port_string_host_container() {
        // "8080:80" → host 8080, container 80
        let config = json!({"appPort": "8080:80"});
        let bindings = map_port_bindings(&config);
        let fwd = &bindings["80/tcp"];
        assert_eq!(fwd.len(), 1);
        assert_eq!(fwd[0].host_ip, None);
        assert_eq!(fwd[0].host_port.as_deref(), Some("8080"));
    }

    #[test]
    fn app_port_string_with_ip() {
        // "127.0.0.1:3000:3000" → ip 127.0.0.1, host 3000, container 3000
        let config = json!({"appPort": "127.0.0.1:3000:3000"});
        let bindings = map_port_bindings(&config);
        let fwd = &bindings["3000/tcp"];
        assert_eq!(fwd.len(), 1);
        assert_eq!(fwd[0].host_ip.as_deref(), Some("127.0.0.1"));
        assert_eq!(fwd[0].host_port.as_deref(), Some("3000"));
    }

    #[test]
    fn app_port_string_bracketed_ipv6() {
        // "[::1]:8080:80" → ip ::1 (brackets stripped), host 8080, container 80
        let config = json!({"appPort": "[::1]:8080:80"});
        let bindings = map_port_bindings(&config);
        let fwd = &bindings["80/tcp"];
        assert_eq!(fwd.len(), 1);
        assert_eq!(fwd[0].host_ip.as_deref(), Some("::1"));
        assert_eq!(fwd[0].host_port.as_deref(), Some("8080"));
    }

    #[test]
    fn app_port_string_ipv6_empty_host_port_is_ephemeral() {
        // "[::1]::80" → ip ::1, empty host port normalized to None
        let config = json!({"appPort": "[::1]::80"});
        let bindings = map_port_bindings(&config);
        let fwd = &bindings["80/tcp"];
        assert_eq!(fwd.len(), 1);
        assert_eq!(fwd[0].host_ip.as_deref(), Some("::1"));
        assert_eq!(fwd[0].host_port, None);
    }

    #[test]
    fn app_port_string_empty_host_port_is_ephemeral() {
        // ":80" → empty host port normalized to None
        let config = json!({"appPort": ":80"});
        let bindings = map_port_bindings(&config);
        let fwd = &bindings["80/tcp"];
        assert_eq!(fwd.len(), 1);
        assert_eq!(fwd[0].host_port, None);
    }

    #[test]
    fn app_port_string_bare_port_ephemeral_host() {
        // "9000" → container 9000, no host port (docker assigns ephemeral)
        let config = json!({"appPort": "9000"});
        let bindings = map_port_bindings(&config);
        let fwd = &bindings["9000/tcp"];
        assert_eq!(fwd.len(), 1);
        assert_eq!(fwd[0].host_ip, None);
        assert_eq!(fwd[0].host_port, None);
    }

    #[test]
    fn app_port_string_udp_protocol() {
        // "5000/udp" → container 5000/udp
        let config = json!({"appPort": "5000/udp"});
        let bindings = map_port_bindings(&config);
        assert!(bindings.contains_key("5000/udp"), "key should be 5000/udp");
        assert!(
            !bindings.contains_key("5000/tcp"),
            "should not create tcp key"
        );
        let fwd = &bindings["5000/udp"];
        assert_eq!(fwd[0].host_ip, None);
        assert_eq!(fwd[0].host_port, None);
    }

    #[test]
    fn app_port_array_mixed() {
        // [3000, "8080:80"] → two bindings
        let config = json!({"appPort": [3000, "8080:80"]});
        let bindings = map_port_bindings(&config);
        assert!(bindings.contains_key("3000/tcp"));
        assert!(bindings.contains_key("80/tcp"));
        assert_eq!(
            bindings["3000/tcp"][0].host_ip.as_deref(),
            Some("127.0.0.1")
        );
        assert_eq!(bindings["80/tcp"][0].host_port.as_deref(), Some("8080"));
    }

    #[test]
    fn app_port_and_forward_ports_merged() {
        // Both appPort and forwardPorts present — entries end up in the same map
        let config = json!({
            "forwardPorts": [5432],
            "appPort": 3000,
        });
        let bindings = map_port_bindings(&config);
        // forwardPorts binds to 0.0.0.0
        let fwd_5432 = &bindings["5432/tcp"];
        assert_eq!(fwd_5432[0].host_ip.as_deref(), Some("0.0.0.0"));
        // appPort binds to 127.0.0.1
        let fwd_3000 = &bindings["3000/tcp"];
        assert_eq!(fwd_3000[0].host_ip.as_deref(), Some("127.0.0.1"));
    }

    #[test]
    fn app_port_ignored_on_non_number_non_string_elements() {
        // Arrays may contain booleans or null — those must be silently skipped
        let config = json!({"appPort": [3000, true, null, "4000"]});
        let bindings = map_port_bindings(&config);
        assert_eq!(bindings.len(), 2);
        assert!(bindings.contains_key("3000/tcp"));
        assert!(bindings.contains_key("4000/tcp"));
    }

    // ── regression: empty protocol suffix ─────────────────────────────────────

    #[test]
    fn app_port_string_trailing_slash_rejected() {
        // "80/" has an empty protocol — not a valid Docker -p spec, must be dropped.
        let config = json!({"appPort": "80/"});
        let bindings = map_port_bindings(&config);
        assert!(
            bindings.is_empty(),
            "trailing slash with empty proto should produce no binding"
        );
    }

    // ── regression: appPort/forwardPorts deduplication ────────────────────────

    #[test]
    fn app_port_skipped_when_forward_ports_owns_same_host_port() {
        // forwardPorts: [3000] binds 0.0.0.0:3000 (owns all interfaces).
        // appPort: 3000 would add 127.0.0.1:3000 — Docker would fail with
        // "port already allocated". The appPort entry must be silently dropped.
        let config = json!({
            "forwardPorts": [3000],
            "appPort": 3000,
        });
        let bindings = map_port_bindings(&config);
        let fwd = &bindings["3000/tcp"];
        // Only one binding — the forwardPorts one.
        assert_eq!(fwd.len(), 1, "appPort duplicate must be dropped");
        assert_eq!(fwd[0].host_ip.as_deref(), Some("0.0.0.0"));
    }

    #[test]
    fn app_port_not_skipped_when_forward_ports_uses_different_host_port() {
        // forwardPorts: [5432] binds 0.0.0.0:5432 → container 5432.
        // appPort: 3000 binds 127.0.0.1:3000 → container 3000.
        // Different host ports — no conflict, both should be present.
        let config = json!({
            "forwardPorts": [5432],
            "appPort": 3000,
        });
        let bindings = map_port_bindings(&config);
        assert!(
            bindings.contains_key("5432/tcp"),
            "forwardPorts entry must be present"
        );
        assert!(
            bindings.contains_key("3000/tcp"),
            "appPort on different host port must be kept"
        );
        assert_eq!(bindings["5432/tcp"][0].host_ip.as_deref(), Some("0.0.0.0"));
        assert_eq!(
            bindings["3000/tcp"][0].host_ip.as_deref(),
            Some("127.0.0.1")
        );
    }
}
