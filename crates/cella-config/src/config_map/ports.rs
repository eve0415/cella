use std::collections::HashMap;

use cella_backend::PortForward;
use cella_protocol::{OnAutoForward, PortAttributes, PortPattern};

pub(super) fn map_port_bindings(config: &serde_json::Value) -> HashMap<String, Vec<PortForward>> {
    let Some(ports) = config.get("forwardPorts").and_then(|v| v.as_array()) else {
        return HashMap::new();
    };

    let ports_attrs = config.get("portsAttributes").and_then(|v| v.as_object());
    let mut bindings = HashMap::new();

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
        let host_port = port.clone();

        bindings.insert(
            container_port,
            vec![PortForward {
                host_ip: Some("0.0.0.0".to_string()),
                host_port: Some(host_port),
            }],
        );
    }

    bindings
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
}
