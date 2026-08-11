//! Drives the real `arboard` client against `cella-wayland`'s server.
//!
//! `wl-clipboard-rs` alone is not enough: it never runs `arboard`'s
//! primary-selection probe, which is where the `NoSeats` trap lives. If that
//! probe fails, `arboard` silently falls back to X11 and the original bug
//! returns with a green test suite.
//!
//! `arboard` reads `WAYLAND_DISPLAY` from the environment, and edition 2024
//! makes `env::set_var` unsafe while this workspace denies `unsafe_code`. Each
//! parent test therefore binds a server and re-execs this same test binary,
//! running one named child test, with the env var set on the child.
//!
//! Linux-only: the server it drives is a Wayland endpoint, and `arboard`'s
//! `SetExtLinux` does not exist on macOS. The workspace clippy job builds every
//! target on macOS too, so this has to compile away rather than fail there.
#![cfg(target_os = "linux")]

use std::path::PathBuf;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use arboard::SetExtLinux as _;
use cella_wayland::{ClipboardSource, ServerHandle, SourceError, WaylandClipboardServer};

/// Names which child test the re-execed process should run. Also the flag the
/// child uses to tell it is a child at all.
const CHILD_MARKER: &str = "CELLA_WAYLAND_TEST_CHILD";

/// A 2x2 RGBA PNG: red, green / blue, white.
const TINY_PNG: &[u8] = &[
    0x89, 0x50, 0x4e, 0x47, 0x0d, 0x0a, 0x1a, 0x0a, 0x00, 0x00, 0x00, 0x0d, 0x49, 0x48, 0x44, 0x52,
    0x00, 0x00, 0x00, 0x02, 0x00, 0x00, 0x00, 0x02, 0x08, 0x06, 0x00, 0x00, 0x00, 0x72, 0xb6, 0x0d,
    0x24, 0x00, 0x00, 0x00, 0x12, 0x49, 0x44, 0x41, 0x54, 0x78, 0x9c, 0x63, 0xf8, 0xcf, 0xc0, 0xf0,
    0x1f, 0x0c, 0x81, 0x34, 0x18, 0x00, 0x00, 0x49, 0xc8, 0x09, 0xf7, 0xf9, 0xab, 0xb6, 0x0d, 0x00,
    0x00, 0x00, 0x00, 0x49, 0x45, 0x4e, 0x44, 0xae, 0x42, 0x60, 0x82,
];

/// Stands in for the host clipboard. Integration tests cannot reach the crate's
/// internal stubs, so this is its own small copy.
#[derive(Default)]
struct StubSource {
    entries: Vec<(String, Vec<u8>)>,
    published: Mutex<Vec<(String, Vec<u8>)>>,
}

impl StubSource {
    fn with(entries: &[(&str, &[u8])]) -> Self {
        Self {
            entries: entries
                .iter()
                .map(|(m, d)| ((*m).to_string(), (*d).to_vec()))
                .collect(),
            published: Mutex::new(Vec::new()),
        }
    }

    fn published(&self) -> Vec<(String, Vec<u8>)> {
        self.published.lock().unwrap().clone()
    }
}

impl ClipboardSource for StubSource {
    fn targets(&self) -> Result<Vec<String>, SourceError> {
        Ok(self.entries.iter().map(|(m, _)| m.clone()).collect())
    }

    fn fetch(&self, mime_type: &str) -> Result<Vec<u8>, SourceError> {
        Ok(self
            .entries
            .iter()
            .find(|(m, _)| m == mime_type)
            .map(|(_, d)| d.clone())
            .unwrap_or_default())
    }

    fn publish(&self, mime_type: &str, data: &[u8]) -> Result<(), SourceError> {
        self.published
            .lock()
            .unwrap()
            .push((mime_type.to_string(), data.to_vec()));
        Ok(())
    }
}

fn is_child() -> bool {
    std::env::var(CHILD_MARKER).is_ok()
}

fn bind_test_server(source: Arc<StubSource>) -> (ServerHandle, PathBuf) {
    // Parent tests run in parallel, and `bind_absolute` holds an flock, so the
    // path has to be unique per server or the second one is refused.
    static NEXT: AtomicUsize = AtomicUsize::new(0);
    let dir = std::env::temp_dir().join("cella-wayland-arboard");
    std::fs::create_dir_all(&dir).unwrap();
    let path = dir.join(format!(
        "wayland-{}-{}",
        std::process::id(),
        NEXT.fetch_add(1, Ordering::SeqCst)
    ));
    let server = WaylandClipboardServer::bind(&path, source).unwrap();
    (server.spawn().unwrap(), path)
}

/// Re-execs this test binary to run exactly one child test with
/// `WAYLAND_DISPLAY` pointing at `path`.
///
/// `DISPLAY` is explicitly removed: if it leaked through, `arboard` could reach
/// a real X11 server and pass for the wrong reason.
fn run_child(child: &str, path: &PathBuf) -> bool {
    std::process::Command::new(std::env::current_exe().unwrap())
        .args(["--exact", child, "--nocapture", "--test-threads=1"])
        .env(CHILD_MARKER, child)
        .env("WAYLAND_DISPLAY", path)
        .env_remove("DISPLAY")
        .status()
        .unwrap()
        .success()
}

#[test]
fn arboard_reads_the_host_image_through_the_server() {
    if is_child() {
        return;
    }
    let (_server, path) = bind_test_server(Arc::new(StubSource::with(&[
        ("image/png", TINY_PNG),
        ("text/plain", b"ignored"),
    ])));
    assert!(
        run_child("arboard_child_image", &path),
        "arboard child failed against cella's server"
    );
}

#[test]
fn arboard_child_image() {
    if !is_child() {
        return;
    }
    // The assertion that matters most: `new()` runs the primary-selection
    // probe, and a missing or malformed seat makes it fail — at which point
    // arboard falls back to X11 and the original bug is back.
    let mut cb = arboard::Clipboard::new().expect("Clipboard::new() against cella's server");
    let img = cb
        .get_image()
        .expect("get_image() from the stubbed host clipboard");
    assert_eq!((img.width, img.height), (2, 2));
    // Top-left pixel is opaque red, proving the bytes survived the round trip.
    assert_eq!(&img.bytes[..4], &[255, 0, 0, 255]);
}

#[test]
fn arboard_reads_host_text_through_the_server() {
    if is_child() {
        return;
    }
    let (_server, path) = bind_test_server(Arc::new(StubSource::with(&[(
        "text/plain;charset=utf-8",
        b"hello from the host",
    )])));
    assert!(
        run_child("arboard_child_text", &path),
        "arboard text child failed against cella's server"
    );
}

#[test]
fn arboard_child_text() {
    if !is_child() {
        return;
    }
    let mut cb = arboard::Clipboard::new().expect("Clipboard::new() against cella's server");
    assert_eq!(cb.get_text().unwrap(), "hello from the host");
}

#[test]
fn arboard_writes_text_back_to_the_host() {
    if is_child() {
        return;
    }
    let source = Arc::new(StubSource::with(&[("text/plain", b"whatever")]));
    let (_server, path) = bind_test_server(Arc::clone(&source));
    assert!(
        run_child("arboard_child_copy", &path),
        "arboard copy child failed against cella's server"
    );

    // The child copies in the foreground, so the publish has already happened
    // by the time it exits — but give the server thread a moment regardless.
    let deadline = Instant::now() + Duration::from_secs(5);
    while source.published().is_empty() && Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(20));
    }
    let published = source.published();
    assert_eq!(published.len(), 1, "expected exactly one publish");
    assert_eq!(published[0].1, b"copied from the container");
    assert!(
        published[0].0.starts_with("text/plain") || published[0].0 == "UTF8_STRING",
        "unexpected mime {}",
        published[0].0
    );
}

#[test]
fn arboard_child_copy() {
    if !is_child() {
        return;
    }
    let mut cb = arboard::Clipboard::new().expect("Clipboard::new() against cella's server");
    // `wait()` serves the selection in the foreground, so this returns only
    // once cella has read the payload and cancelled the source. That ordering
    // is exactly what a `wl-copy --foreground` client depends on.
    cb.set()
        .wait()
        .text("copied from the container")
        .expect("set_text() against cella's server");
}
