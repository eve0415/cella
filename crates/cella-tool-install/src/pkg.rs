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
    pub apt: &'static [&'static str],
    pub apk: &'static [&'static str],
    pub dnf: &'static [&'static str],
    pub pacman: &'static [&'static str],
    pub zypper: &'static [&'static str],
}

impl PackageSpec {
    pub const fn names_for(&self, manager: PackageManager) -> &[&str] {
        match manager {
            PackageManager::Apt => self.apt,
            PackageManager::Apk => self.apk,
            PackageManager::Dnf => self.dnf,
            PackageManager::Pacman => self.pacman,
            PackageManager::Zypper => self.zypper,
        }
    }
}

pub const TMUX: PackageSpec = PackageSpec {
    check_binary: Some("tmux"),
    apt: &["tmux"],
    apk: &["tmux"],
    dnf: &["tmux"],
    pacman: &["tmux"],
    zypper: &["tmux"],
};

pub const BUBBLEWRAP: PackageSpec = PackageSpec {
    check_binary: Some("bwrap"),
    apt: &["bubblewrap"],
    apk: &["bubblewrap"],
    dnf: &["bubblewrap"],
    pacman: &["bubblewrap"],
    zypper: &["bubblewrap"],
};

pub const NODEJS: PackageSpec = PackageSpec {
    check_binary: None,
    apt: &["nodejs", "npm"],
    apk: &["nodejs", "npm"],
    dnf: &["nodejs", "npm"],
    pacman: &["nodejs", "npm"],
    zypper: &["nodejs", "npm"],
};

pub const LIBGCC: PackageSpec = PackageSpec {
    check_binary: None,
    apt: &[],
    apk: &["libgcc"],
    dnf: &[],
    pacman: &[],
    zypper: &[],
};

pub const LIBSTDCPP: PackageSpec = PackageSpec {
    check_binary: None,
    apt: &[],
    apk: &["libstdc++"],
    dnf: &[],
    pacman: &[],
    zypper: &[],
};

pub const RIPGREP: PackageSpec = PackageSpec {
    check_binary: Some("rg"),
    apt: &["ripgrep"],
    apk: &["ripgrep"],
    dnf: &["ripgrep"],
    pacman: &["ripgrep"],
    zypper: &["ripgrep"],
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
