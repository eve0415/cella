use std::sync::OnceLock;
use std::time::Duration;
use tokio::sync::OnceCell;

const TIMEOUT: Duration = Duration::from_secs(5);

macro_rules! detect_fn {
    ($async_name:ident, $sync_name:ident, $cell_async:ident, $cell_sync:ident, $check:expr) => {
        static $cell_async: OnceCell<bool> = OnceCell::const_new();
        static $cell_sync: OnceLock<bool> = OnceLock::new();

        pub async fn $async_name() -> bool {
            *$cell_async
                .get_or_init(|| async {
                    tokio::time::timeout(TIMEOUT, async { $check.await })
                        .await
                        .unwrap_or(false)
                })
                .await
        }

        pub fn $sync_name() -> bool {
            *$cell_sync.get_or_init(|| {
                tokio::runtime::Builder::new_current_thread()
                    .enable_all()
                    .build()
                    .expect("failed to build tokio runtime for sync detection")
                    .block_on($async_name())
            })
        }
    };
}

async fn check_docker() -> bool {
    let Ok(docker) = bollard::Docker::connect_with_local_defaults() else {
        return false;
    };
    docker.ping().await.is_ok()
}

async fn check_command(cmd: &str, args: &[&str]) -> bool {
    tokio::process::Command::new(cmd)
        .args(args)
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .await
        .is_ok_and(|s| s.success())
}

async fn check_network() -> bool {
    tokio::net::TcpStream::connect("ghcr.io:443").await.is_ok()
}

detect_fn!(
    docker_available,
    docker_available_sync,
    DOCKER_ASYNC,
    DOCKER_SYNC,
    check_docker()
);
detect_fn!(
    compose_available,
    compose_available_sync,
    COMPOSE_ASYNC,
    COMPOSE_SYNC,
    check_command("docker", &["compose", "version"])
);
detect_fn!(
    buildx_available,
    buildx_available_sync,
    BUILDX_ASYNC,
    BUILDX_SYNC,
    check_command("docker", &["buildx", "version"])
);
detect_fn!(
    podman_available,
    podman_available_sync,
    PODMAN_ASYNC,
    PODMAN_SYNC,
    check_command("podman", &["version"])
);
detect_fn!(
    apple_container_available,
    apple_container_available_sync,
    APPLE_CONTAINER_ASYNC,
    APPLE_CONTAINER_SYNC,
    check_command("container", &["--version"])
);
detect_fn!(
    orbstack_available,
    orbstack_available_sync,
    ORBSTACK_ASYNC,
    ORBSTACK_SYNC,
    check_command("orbctl", &["version"])
);
detect_fn!(
    colima_available,
    colima_available_sync,
    COLIMA_ASYNC,
    COLIMA_SYNC,
    check_command("colima", &["status"])
);
detect_fn!(
    lima_available,
    lima_available_sync,
    LIMA_ASYNC,
    LIMA_SYNC,
    check_command("limactl", &["list", "--quiet"])
);
detect_fn!(
    network_available,
    network_available_sync,
    NETWORK_ASYNC,
    NETWORK_SYNC,
    check_network()
);

pub async fn container_runtime_available() -> bool {
    docker_available().await
        || podman_available().await
        || apple_container_available().await
        || orbstack_available().await
        || colima_available().await
        || lima_available().await
}

pub fn container_runtime_available_sync() -> bool {
    docker_available_sync()
        || podman_available_sync()
        || apple_container_available_sync()
        || orbstack_available_sync()
        || colima_available_sync()
        || lima_available_sync()
}

pub async fn container_runtime_available_except(exclude: &[&str]) -> bool {
    (!exclude.contains(&"docker") && docker_available().await)
        || (!exclude.contains(&"podman") && podman_available().await)
        || (!exclude.contains(&"apple_container") && apple_container_available().await)
        || (!exclude.contains(&"orbstack") && orbstack_available().await)
        || (!exclude.contains(&"colima") && colima_available().await)
        || (!exclude.contains(&"lima") && lima_available().await)
}

pub fn container_runtime_available_except_sync(exclude: &[&str]) -> bool {
    (!exclude.contains(&"docker") && docker_available_sync())
        || (!exclude.contains(&"podman") && podman_available_sync())
        || (!exclude.contains(&"apple_container") && apple_container_available_sync())
        || (!exclude.contains(&"orbstack") && orbstack_available_sync())
        || (!exclude.contains(&"colima") && colima_available_sync())
        || (!exclude.contains(&"lima") && lima_available_sync())
}
