//! Common filesystem paths for cella data and sockets.

use std::path::PathBuf;

/// Get the cella data directory (`~/.cella/`).
pub fn cella_data_dir() -> Option<PathBuf> {
    std::env::var("HOME")
        .ok()
        .map(|h| PathBuf::from(h).join(".cella"))
}

/// Get the daemon management socket path (`~/.cella/daemon.sock`).
pub fn daemon_socket_path() -> Option<PathBuf> {
    cella_data_dir().map(|d| d.join("daemon.sock"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cella_data_dir_uses_home() {
        if let Ok(home) = std::env::var("HOME") {
            let dir = cella_data_dir().unwrap();
            assert_eq!(dir, PathBuf::from(home).join(".cella"));
        }
    }

    #[test]
    fn daemon_socket_path_format() {
        if let Ok(home) = std::env::var("HOME") {
            let path = daemon_socket_path().unwrap();
            assert_eq!(path, PathBuf::from(home).join(".cella/daemon.sock"));
        }
    }
}
