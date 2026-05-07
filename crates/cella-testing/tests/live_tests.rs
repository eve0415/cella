use cella_testing::runtime_test;

#[runtime_test(docker)]
async fn macro_works_with_docker() {
    let docker = bollard::Docker::connect_with_local_defaults().unwrap();
    docker.ping().await.unwrap();
}

#[runtime_test]
async fn macro_works_any_runtime() {}

#[runtime_test(network)]
async fn macro_works_with_network() {
    let _stream = tokio::net::TcpStream::connect("ghcr.io:443").await.unwrap();
}
