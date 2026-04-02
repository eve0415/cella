//! `/proc/net/tcp` and `/proc/net/tcp6` parser for port detection.
//!
//! Shared between the in-container agent (real scanning) and host-side
//! `cella ports` (displaying detected ports).

use std::collections::HashSet;
use std::path::Path;

use cella_protocol::{BindAddress, PortProtocol};

/// A detected listening socket from /proc/net/tcp or /proc/net/tcp6.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct DetectedListener {
    pub port: u16,
    pub protocol: PortProtocol,
    pub bind: BindAddress,
    /// The inode number (for cross-referencing with /proc/<pid>/fd/).
    pub inode: u64,
}

/// Parse `/proc/net/tcp` or `/proc/net/tcp6` content into detected listeners.
///
/// Each line in the file has the format:
/// ```text
///   sl  local_address rem_address   st tx_queue:rx_queue ...
///    0: 0100007F:0BB8 00000000:0000 0A ...
/// ```
///
/// We only care about entries in LISTEN state (st = 0A).
pub fn parse_proc_net_tcp(content: &str, protocol: PortProtocol) -> Vec<DetectedListener> {
    content
        .lines()
        .skip(1) // skip header
        .filter_map(|line| parse_tcp_line(line, protocol))
        .collect()
}

/// Parse a single line from /proc/net/tcp.
fn parse_tcp_line(line: &str, protocol: PortProtocol) -> Option<DetectedListener> {
    let fields: Vec<&str> = line.split_whitespace().collect();
    if fields.len() < 10 {
        return None;
    }

    // Field 3 is state — 0A = LISTEN
    let state = fields[3];
    if state != "0A" {
        return None;
    }

    // Field 1 is local_address in hex: ADDR:PORT
    let local_addr = fields[1];
    let (addr_hex, port_hex) = local_addr.rsplit_once(':')?;
    let port = u16::from_str_radix(port_hex, 16).ok()?;

    let bind = classify_bind_address(addr_hex);

    // Field 9 is the inode
    let inode = fields[9].parse::<u64>().unwrap_or(0);

    Some(DetectedListener {
        port,
        protocol,
        bind,
        inode,
    })
}

/// Classify whether an address is localhost or all-interfaces.
fn classify_bind_address(addr_hex: &str) -> BindAddress {
    match addr_hex.len() {
        8 => {
            // IPv4: 0100007F = 127.0.0.1 (little-endian)
            if addr_hex == "0100007F" {
                BindAddress::Localhost
            } else {
                BindAddress::All
            }
        }
        32 => {
            // IPv6 in /proc/net/tcp6: 4x 32-bit LE groups concatenated.
            //   ::1              = 00000000000000000000000001000000
            //   ::ffff:127.0.0.1 = 0000000000000000FFFF00000100007F
            //   ::               = 00000000000000000000000000000000
            if addr_hex == "00000000000000000000000001000000"
                || addr_hex == "0000000000000000FFFF00000100007F"
            {
                BindAddress::Localhost
            } else {
                BindAddress::All
            }
        }
        _ => BindAddress::All,
    }
}

/// Read a single proc tcp file and insert any detected listeners.
fn read_proc_tcp_listeners(
    path: &Path,
    protocol: PortProtocol,
    listeners: &mut HashSet<DetectedListener>,
    at_least_one_read: &mut bool,
    last_error: &mut Option<std::io::Error>,
) {
    match std::fs::read_to_string(path) {
        Ok(content) => {
            *at_least_one_read = true;
            for listener in parse_proc_net_tcp(&content, protocol) {
                listeners.insert(listener);
            }
        }
        Err(e) => *last_error = Some(e),
    }
}

/// Scan `/proc/net/tcp` and `/proc/net/tcp6` for listening ports.
///
/// Returns the set of all detected listeners.
///
/// # Errors
///
/// Returns error if proc files cannot be read.
pub fn scan_listeners(proc_path: &Path) -> Result<HashSet<DetectedListener>, std::io::Error> {
    let mut listeners = HashSet::new();
    let mut last_error: Option<std::io::Error> = None;
    let mut at_least_one_read = false;

    // Read /proc/net/tcp (IPv4) and /proc/net/tcp6 (IPv6)
    let tcp_path = proc_path.join("net/tcp");
    read_proc_tcp_listeners(
        &tcp_path,
        PortProtocol::Tcp,
        &mut listeners,
        &mut at_least_one_read,
        &mut last_error,
    );
    let tcp6_path = proc_path.join("net/tcp6");
    read_proc_tcp_listeners(
        &tcp6_path,
        PortProtocol::Tcp,
        &mut listeners,
        &mut at_least_one_read,
        &mut last_error,
    );

    if !at_least_one_read {
        return Err(last_error.unwrap_or_else(|| {
            std::io::Error::new(std::io::ErrorKind::NotFound, "no proc net files readable")
        }));
    }

    Ok(listeners)
}

/// Try to read the process name for a given inode by scanning /proc/<pid>/fd/.
///
/// This is best-effort — many processes may not be readable.
pub fn process_name_for_inode(inode: u64) -> Option<String> {
    let proc_dir = std::fs::read_dir("/proc").ok()?;

    for entry in proc_dir.flatten() {
        let pid_str = entry.file_name();
        let pid_str = pid_str.to_str()?;

        // Skip non-numeric entries
        if !pid_str.chars().all(|c| c.is_ascii_digit()) {
            continue;
        }

        let fd_dir = entry.path().join("fd");
        if let Ok(fds) = std::fs::read_dir(&fd_dir) {
            for fd_entry in fds.flatten() {
                if let Ok(link) = std::fs::read_link(fd_entry.path()) {
                    let link_str = link.to_string_lossy();
                    if link_str.contains(&format!("socket:[{inode}]")) {
                        // Found the process — read cmdline
                        let cmdline_path = entry.path().join("cmdline");
                        if let Ok(cmdline) = std::fs::read_to_string(&cmdline_path) {
                            let name = cmdline
                                .split('\0')
                                .next()
                                .unwrap_or("")
                                .rsplit('/')
                                .next()
                                .unwrap_or("")
                                .to_string();
                            if !name.is_empty() {
                                return Some(name);
                            }
                        }
                        return None;
                    }
                }
            }
        }
    }

    None
}

#[cfg(test)]
mod tests {
    use super::*;

    const SAMPLE_PROC_NET_TCP: &str = "\
  sl  local_address rem_address   st tx_queue rx_queue tr tm->when retrnsmt   uid  timeout inode
   0: 0100007F:0BB8 00000000:0000 0A 00000000:00000000 00:00000000 00000000  1000        0 12345 1 0000000000000000 100 0 0 10 0
   1: 00000000:1F90 00000000:0000 0A 00000000:00000000 00:00000000 00000000  1000        0 12346 1 0000000000000000 100 0 0 10 0
   2: 0100007F:7918 0100007F:0BB8 01 00000000:00000000 00:00000000 00000000  1000        0 12347 1 0000000000000000 100 0 0 10 0
";

    #[test]
    fn parse_tcp_listen_entries() {
        let listeners = parse_proc_net_tcp(SAMPLE_PROC_NET_TCP, PortProtocol::Tcp);
        assert_eq!(listeners.len(), 2);

        // 0x0BB8 = 3000, bound to 127.0.0.1
        let l1 = &listeners[0];
        assert_eq!(l1.port, 3000);
        assert_eq!(l1.bind, BindAddress::Localhost);
        assert_eq!(l1.inode, 12345);

        // 0x1F90 = 8080, bound to 0.0.0.0
        let l2 = &listeners[1];
        assert_eq!(l2.port, 8080);
        assert_eq!(l2.bind, BindAddress::All);
        assert_eq!(l2.inode, 12346);
    }

    #[test]
    fn skip_non_listen_state() {
        let content = "\
  sl  local_address rem_address   st tx_queue rx_queue tr tm->when retrnsmt   uid  timeout inode
   0: 0100007F:7918 0100007F:0BB8 01 00000000:00000000 00:00000000 00000000  1000        0 12347 1 0000000000000000 100 0 0 10 0
";
        let listeners = parse_proc_net_tcp(content, PortProtocol::Tcp);
        assert!(listeners.is_empty());
    }

    #[test]
    fn skip_malformed_lines() {
        let content = "\
  sl  local_address rem_address   st tx_queue rx_queue tr tm->when retrnsmt   uid  timeout inode
   bad line
";
        let listeners = parse_proc_net_tcp(content, PortProtocol::Tcp);
        assert!(listeners.is_empty());
    }

    #[test]
    fn classify_localhost_ipv4() {
        assert_eq!(classify_bind_address("0100007F"), BindAddress::Localhost);
    }

    #[test]
    fn classify_all_interfaces_ipv4() {
        assert_eq!(classify_bind_address("00000000"), BindAddress::All);
    }

    #[test]
    fn classify_specific_ip_as_all() {
        // Any non-localhost IP is treated as reachable from outside
        assert_eq!(classify_bind_address("0101A8C0"), BindAddress::All);
    }

    #[test]
    fn classify_ipv6_all_interfaces() {
        assert_eq!(
            classify_bind_address("00000000000000000000000000000000"),
            BindAddress::All
        );
    }

    #[test]
    fn classify_ipv6_localhost() {
        assert_eq!(
            classify_bind_address("00000000000000000000000001000000"),
            BindAddress::Localhost
        );
    }

    #[test]
    fn classify_ipv4_mapped_ipv6_localhost() {
        assert_eq!(
            classify_bind_address("0000000000000000FFFF00000100007F"),
            BindAddress::Localhost
        );
    }

    #[test]
    fn scan_listeners_returns_err_when_unreadable() {
        // Use a directory that has no net/tcp or net/tcp6 files
        let dir = tempfile::tempdir().unwrap();
        let result = scan_listeners(dir.path());
        assert!(result.is_err());
    }

    #[test]
    fn parse_tcp6_listen_entries() {
        let content = "\
  sl  local_address                         remote_address                        st tx_queue rx_queue tr tm->when retrnsmt   uid  timeout inode
   0: 00000000000000000000000000000000:0BB8 00000000000000000000000000000000:0000 0A 00000000:00000000 00:00000000 00000000  1000        0 54321 1 0000000000000000 100 0 0 10 0
   1: 00000000000000000000000001000000:1F90 00000000000000000000000000000000:0000 0A 00000000:00000000 00:00000000 00000000  1000        0 54322 1 0000000000000000 100 0 0 10 0
";
        let listeners = parse_proc_net_tcp(content, PortProtocol::Tcp);
        assert_eq!(listeners.len(), 2);

        // 0x0BB8 = 3000, bound to ::  (all interfaces)
        let l1 = &listeners[0];
        assert_eq!(l1.port, 3000);
        assert_eq!(l1.bind, BindAddress::All);
        assert_eq!(l1.inode, 54321);

        // 0x1F90 = 8080, bound to ::1 (localhost)
        let l2 = &listeners[1];
        assert_eq!(l2.port, 8080);
        assert_eq!(l2.bind, BindAddress::Localhost);
        assert_eq!(l2.inode, 54322);
    }
}
