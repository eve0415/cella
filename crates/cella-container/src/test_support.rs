//! Shared helpers for tests that mock the `container` CLI with shell scripts.

use std::path::{Path, PathBuf};

/// Write an executable `#!/bin/sh` mock script at `dir/name` and return its
/// path.
///
/// The write is skipped when the content is already current, so a script
/// file is never rewritten while another thread may be executing it (which
/// would race `ETXTBSY` on overlayfs). After a fresh write, the function
/// spins until the kernel releases the inode's write reference (deferred
/// `__fput`) and the script becomes executable.
pub fn write_mock_script(dir: &Path, name: &str, body: &str) -> PathBuf {
    use std::io::Write;

    let path = dir.join(name);
    let content = format!("#!/bin/sh\n{body}\n");

    let needs_write = std::fs::read_to_string(&path).map_or(true, |existing| existing != content);
    if needs_write {
        let mut file = std::fs::File::create(&path).unwrap();
        file.write_all(content.as_bytes()).unwrap();
        file.sync_all().unwrap();
        drop(file);
    }

    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755));
    }

    if needs_write {
        for _ in 0..50 {
            match std::process::Command::new(&path)
                .arg("--etxtbsy-probe")
                .stdout(std::process::Stdio::null())
                .stderr(std::process::Stdio::null())
                .output()
            {
                Err(e) if e.kind() == std::io::ErrorKind::ExecutableFileBusy => {
                    std::thread::sleep(std::time::Duration::from_millis(1));
                }
                _ => break,
            }
        }
    }

    path
}
