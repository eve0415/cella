//! Integration test: `tag_image` adds a new name to an existing image.
//!
//! This is the backend primitive behind `cella build --image-name` (the
//! official build-then-tag flow): after `tag_image(source, target)` the image
//! is reachable under `target`. The test pulls a small image, tags it, and
//! asserts the new tag exists via `image_exists`.

use bollard::query_parameters::RemoveImageOptions;

use crate::client::DockerClient;

const SOURCE_IMAGE: &str = "busybox:latest";

/// `tag_image` makes the source image reachable under a new name. Uses an
/// explicit tag so the no-tag → `latest` defaulting does not affect the
/// `image_exists` lookup. Cleans up the created tag afterward.
#[cella_testing::runtime_test(docker)]
async fn tag_image_adds_reachable_name() {
    let Ok(client) = DockerClient::connect() else {
        return;
    };

    if client.pull_image(SOURCE_IMAGE).await.is_err() {
        return;
    }

    let target = format!("cella-it-tag-image-{}:tagtest", std::process::id());

    // The new name must not exist before tagging.
    let before = client
        .image_exists(&target)
        .await
        .expect("image_exists before tag");
    assert!(!before, "target tag {target} must not exist before tagging");

    client
        .tag_image(SOURCE_IMAGE, &target)
        .await
        .expect("tag_image");

    let after = client
        .image_exists(&target)
        .await
        .expect("image_exists after tag");

    // Best-effort cleanup of the tag we created (don't fail the test on it).
    // `force` drops just this tag; the shared busybox image id is untouched
    // because `busybox:latest` still references it.
    let cleanup_opts = RemoveImageOptions {
        force: true,
        ..Default::default()
    };
    let _ = client
        .inner()
        .remove_image(&target, Some(cleanup_opts), None)
        .await;

    assert!(
        after,
        "image must be reachable under the new tag {target} after tag_image"
    );
}
