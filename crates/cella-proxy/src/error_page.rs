//! HTML error page generation for proxy error responses.

use std::fmt::Write;

use crate::router::{BackendTarget, RouteKey, RouteTable};

/// Generate an HTML page for when no route matches the requested hostname.
pub fn no_route_found(requested_host: &str, route_table: &RouteTable) -> String {
    let mut services = String::new();
    let mut routes: Vec<_> = route_table.all_routes().collect();
    routes.sort_by(|a, b| a.0.project.cmp(&b.0.project).then(a.0.port.cmp(&b.0.port)));

    for (key, target) in &routes {
        let hostname = format!("{}.{}.{}.localhost", key.port, key.branch, key.project);
        let _ = writeln!(
            services,
            "  <li><a href=\"http://{hostname}\">{hostname}</a> → {}:{}</li>",
            target.container_name, target.target_port
        );
    }

    if services.is_empty() {
        services = "  <li>No services registered</li>\n".to_string();
    }

    format!(
        r#"<!DOCTYPE html>
<html>
<head>
<meta charset="utf-8">
<title>No service found</title>
<style>{CSS}</style>
</head>
<body>
<div class="container">
<h1>No service found</h1>
<p>Requested: <code>{requested_host}</code></p>

<h2>Available services</h2>
<ul>
{services}</ul>

<h2>Troubleshooting</h2>
<p>If your service is running but returns 4xx errors, your dev server may be
rejecting the <code>Host</code> header. Configure it to accept <code>*.localhost</code> hostnames.</p>
</div>
</body>
</html>"#
    )
}

/// Generate an HTML page for when the route exists but the backend is unreachable.
pub fn backend_unreachable(key: &RouteKey, target: &BackendTarget) -> String {
    format!(
        r#"<!DOCTYPE html>
<html>
<head>
<meta charset="utf-8">
<title>Service unavailable</title>
<style>{CSS}</style>
</head>
<body>
<div class="container">
<h1>Service unavailable</h1>
<p>Container <code>{}</code> is not responding on port {}.</p>
</div>
</body>
</html>"#,
        target.container_name, key.port
    )
}

const CSS: &str = r"
body { font-family: -apple-system, BlinkMacSystemFont, 'Segoe UI', Roboto, sans-serif;
       margin: 0; padding: 2rem; background: #f5f5f5; color: #333; }
.container { max-width: 640px; margin: 0 auto; background: #fff;
             padding: 2rem; border-radius: 8px; box-shadow: 0 1px 3px rgba(0,0,0,0.1); }
h1 { color: #e53e3e; margin-top: 0; }
h2 { color: #555; font-size: 1.1rem; }
code { background: #f0f0f0; padding: 2px 6px; border-radius: 3px; font-size: 0.9em; }
a { color: #3182ce; }
ul { padding-left: 1.5rem; }
li { margin: 0.3rem 0; }
";

#[cfg(test)]
mod tests {
    use std::net::{IpAddr, Ipv4Addr};

    use super::*;
    use crate::router::ProxyMode;

    #[test]
    fn no_route_page_contains_requested_host() {
        let rt = RouteTable::new();
        let html = no_route_found("3000.nonexistent.myapp.localhost", &rt);
        assert!(html.contains("3000.nonexistent.myapp.localhost"));
        assert!(html.contains("No services registered"));
    }

    #[test]
    fn no_route_page_lists_available_services() {
        let mut rt = RouteTable::new();
        rt.insert(
            RouteKey {
                project: "myapp".to_string(),
                branch: "main".to_string(),
                port: 3000,
            },
            BackendTarget {
                container_id: "c1".to_string(),
                container_name: "cella-myapp-main".to_string(),
                target_port: 3000,
                mode: ProxyMode::DirectIp(IpAddr::V4(Ipv4Addr::LOCALHOST)),
            },
        );

        let html = no_route_found("3000.nonexistent.myapp.localhost", &rt);
        assert!(html.contains("3000.main.myapp.localhost"));
        assert!(html.contains("cella-myapp-main:3000"));
    }

    #[test]
    fn backend_unreachable_page_shows_container_info() {
        let key = RouteKey {
            project: "myapp".to_string(),
            branch: "main".to_string(),
            port: 3000,
        };
        let target = BackendTarget {
            container_id: "c1".to_string(),
            container_name: "cella-myapp-main".to_string(),
            target_port: 3000,
            mode: ProxyMode::DirectIp(IpAddr::V4(Ipv4Addr::LOCALHOST)),
        };

        let html = backend_unreachable(&key, &target);
        assert!(html.contains("cella-myapp-main"));
        assert!(html.contains("port 3000"));
    }

    #[test]
    fn pages_are_valid_html() {
        let rt = RouteTable::new();
        let html = no_route_found("test.localhost", &rt);
        assert!(html.starts_with("<!DOCTYPE html>"));
        assert!(html.contains("</html>"));
    }
}
