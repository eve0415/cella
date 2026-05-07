use cella_testing::runtime_test;

#[runtime_test(docker, flavor = "multi_thread")]
async fn test_flavor() {}

fn main() {}
