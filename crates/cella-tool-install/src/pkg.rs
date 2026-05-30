use cella_backend::{ContainerBackend, ExecOptions, ExecResult};
use tracing::{debug, warn};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PackageManager {
    Apt,
    Apk,
    Dnf,
    Pacman,
    Zypper,
}

impl PackageManager {
    pub const fn is_alpine(self) -> bool {
        matches!(self, Self::Apk)
    }
}

pub struct PackageSpec {
    pub check_binary: Option<&'static str>,
    pub names: &'static [&'static str],
    pub apk_override: Option<&'static [&'static str]>,
}

impl PackageSpec {
    pub const fn names_for(&self, manager: PackageManager) -> &[&str] {
        if let (PackageManager::Apk, Some(apk)) = (manager, self.apk_override) {
            apk
        } else {
            self.names
        }
    }
}

pub const TMUX: PackageSpec = PackageSpec {
    check_binary: Some("tmux"),
    names: &["tmux"],
    apk_override: None,
};

pub const BUBBLEWRAP: PackageSpec = PackageSpec {
    check_binary: Some("bwrap"),
    names: &["bubblewrap"],
    apk_override: None,
};

pub const NODEJS: PackageSpec = PackageSpec {
    check_binary: None,
    names: &["nodejs", "npm"],
    apk_override: None,
};

pub const LIBGCC: PackageSpec = PackageSpec {
    check_binary: None,
    names: &[],
    apk_override: Some(&["libgcc"]),
};

pub const LIBSTDCPP: PackageSpec = PackageSpec {
    check_binary: None,
    names: &[],
    apk_override: Some(&["libstdc++"]),
};

pub const RIPGREP: PackageSpec = PackageSpec {
    check_binary: Some("rg"),
    names: &["ripgrep"],
    apk_override: None,
};

pub async fn detect_package_manager(
    client: &dyn ContainerBackend,
    container_id: &str,
) -> Option<PackageManager> {
    let candidates = [
        ("apt-get", PackageManager::Apt),
        ("apk", PackageManager::Apk),
        ("dnf", PackageManager::Dnf),
        ("pacman", PackageManager::Pacman),
        ("zypper", PackageManager::Zypper),
    ];

    for (binary, mgr) in candidates {
        let check = client
            .exec_command(
                container_id,
                &ExecOptions {
                    cmd: vec!["which".to_string(), binary.to_string()],
                    user: Some("root".to_string()),
                    env: None,
                    working_dir: None,
                },
            )
            .await;
        if check.is_ok_and(|r| r.exit_code == 0) {
            debug!("Detected package manager: {binary}");
            return Some(mgr);
        }
    }
    None
}

/// # Returns
///
/// Returns `Ok(result)` even when `result.exit_code != 0` — the caller
/// decides whether a non-zero exit is fatal. Transport errors during exec
/// are returned as `Err`.
pub async fn install_packages(
    client: &dyn ContainerBackend,
    container_id: &str,
    manager: PackageManager,
    specs: &[&PackageSpec],
) -> Result<ExecResult, String> {
    let mut needed_names: Vec<&str> = Vec::new();
    for spec in specs {
        let already_present = if let Some(bin) = spec.check_binary {
            binary_exists(client, container_id, bin).await
        } else {
            false
        };
        if !already_present {
            needed_names.extend(spec.names_for(manager));
        }
    }

    if needed_names.is_empty() {
        return Ok(ExecResult {
            exit_code: 0,
            stdout: String::new(),
            stderr: String::new(),
        });
    }

    let cmd = build_install_command(manager, &needed_names);
    debug!("Installing system packages: {cmd}");

    let result = client
        .exec_command(
            container_id,
            &ExecOptions {
                cmd: vec!["sh".to_string(), "-c".to_string(), cmd],
                user: Some("root".to_string()),
                env: None,
                working_dir: None,
            },
        )
        .await
        .map_err(|e| format!("package install failed: {e}"))?;

    if result.exit_code != 0 {
        warn!(
            "Package installation failed (exit {}): {}",
            result.exit_code,
            result.stderr.trim()
        );
    }

    Ok(result)
}

fn build_install_command(manager: PackageManager, packages: &[&str]) -> String {
    let pkg_list = packages.join(" ");
    match manager {
        PackageManager::Apt => format!(
            "apt-get -o DPkg::Lock::Timeout=60 update -qq \
             && apt-get -o DPkg::Lock::Timeout=60 install -y -qq {pkg_list}"
        ),
        PackageManager::Apk => format!("apk add --no-cache {pkg_list}"),
        PackageManager::Dnf => format!("dnf install -y {pkg_list}"),
        PackageManager::Pacman => format!("pacman -S --noconfirm {pkg_list}"),
        PackageManager::Zypper => format!("zypper install -y {pkg_list}"),
    }
}

async fn binary_exists(client: &dyn ContainerBackend, container_id: &str, binary: &str) -> bool {
    client
        .exec_command(
            container_id,
            &ExecOptions {
                cmd: vec![
                    "sh".to_string(),
                    "-c".to_string(),
                    format!("command -v {binary}"),
                ],
                user: Some("root".to_string()),
                env: None,
                working_dir: None,
            },
        )
        .await
        .is_ok_and(|r| r.exit_code == 0)
}
