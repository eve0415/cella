use std::collections::HashMap;

use bollard::models::PortBinding;

pub(super) fn map_port_bindings(config: &serde_json::Value) -> HashMap<String, Vec<PortBinding>> {
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
            vec![PortBinding {
                host_ip: Some("0.0.0.0".to_string()),
                host_port: Some(host_port),
            }],
        );
    }

    bindings
}
