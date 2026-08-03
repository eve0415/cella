//! Benchmarks for the per-entry OCI layer extraction path.
//!
//! `unpack_archive` runs path-traversal and link-target validation on every
//! entry of every layer, so a feature or image layer with thousands of files
//! pays that cost thousands of times. These benches drive [`extract_layer`]
//! over synthetic layers shaped like real ones and report allocation counts
//! alongside wall time, so a change that removes a per-entry allocation shows
//! up as a number rather than an assertion.

use std::io::Write as _;
use std::path::Path;

use cella_oci::extract::{DEVCONTAINERS_LAYER_MEDIA_TYPE, extract_layer};
use flate2::Compression;
use flate2::write::GzEncoder;
use tempfile::TempDir;

/// Allocation counting is the point of these benches — a per-entry `String`
/// that never escapes is invisible in wall time on a warm tmpfs but obvious
/// here.
#[global_allocator]
static ALLOC: divan::AllocProfiler = divan::AllocProfiler::system();

fn main() {
    divan::main();
}

/// Entry counts spanning a small feature layer through a fat base-image layer.
const ENTRY_COUNTS: &[usize] = &[64, 512, 4096];

/// Build a plain tar shaped like a real layer: nested directories, small
/// files, and a symlink every 16th entry (`bin/` layouts are link-heavy).
fn build_layer(entries: usize) -> Vec<u8> {
    let mut builder = tar::Builder::new(Vec::new());
    let contents = b"#!/bin/sh\nexec /usr/bin/true \"$@\"\n";

    for i in 0..entries {
        let dir = i / 32;
        if i % 32 == 0 {
            let mut header = tar::Header::new_gnu();
            header.set_entry_type(tar::EntryType::Directory);
            header.set_size(0);
            header.set_mode(0o755);
            header.set_cksum();
            builder
                .append_data(&mut header, format!("usr/lib/pkg{dir}/"), &[][..])
                .expect("append dir");
        }

        let mut header = tar::Header::new_gnu();
        if i % 16 == 15 {
            // Symlinks exercise `validate_link_target`, the other per-entry
            // validation branch.
            header.set_entry_type(tar::EntryType::Symlink);
            header.set_size(0);
            header.set_mode(0o777);
            header
                .set_link_name(format!("file{i}.sh"))
                .expect("set link name");
            header.set_cksum();
            builder
                .append_data(&mut header, format!("usr/lib/pkg{dir}/link{i}"), &[][..])
                .expect("append symlink");
        } else {
            header.set_entry_type(tar::EntryType::Regular);
            header.set_size(contents.len() as u64);
            header.set_mode(0o644);
            header.set_cksum();
            builder
                .append_data(
                    &mut header,
                    format!("usr/lib/pkg{dir}/file{i}.sh"),
                    &contents[..],
                )
                .expect("append file");
        }
    }

    builder.into_inner().expect("finish tar")
}

fn gzip(data: &[u8]) -> Vec<u8> {
    let mut encoder = GzEncoder::new(Vec::new(), Compression::default());
    encoder.write_all(data).expect("gzip write");
    encoder.finish().expect("gzip finish")
}

/// Extract into a fresh directory each iteration — reusing one would let the
/// tar crate short-circuit on already-present paths and measure the wrong
/// thing.
fn extract_into(blob: &[u8], media_type: &str, dest: &Path) {
    extract_layer(divan::black_box(blob), media_type, dest).expect("extraction succeeds");
}

/// Plain-tar layers: the devcontainer feature media type, where per-entry
/// validation is the dominant CPU cost (no decompression to hide behind).
#[divan::bench(args = ENTRY_COUNTS)]
fn extract_plain(bencher: divan::Bencher, entries: usize) {
    let blob = build_layer(entries);
    bencher
        .with_inputs(|| TempDir::new().expect("tempdir"))
        .bench_values(|dir| {
            extract_into(&blob, DEVCONTAINERS_LAYER_MEDIA_TYPE, dir.path());
            dir
        });
}

/// Gzipped layers: the common image-layer shape, where validation competes
/// with inflate for the profile.
#[divan::bench(args = ENTRY_COUNTS)]
fn extract_gzip(bencher: divan::Bencher, entries: usize) {
    let blob = gzip(&build_layer(entries));
    bencher
        .with_inputs(|| TempDir::new().expect("tempdir"))
        .bench_values(|dir| {
            extract_into(
                &blob,
                "application/vnd.oci.image.layer.v1.tar+gzip",
                dir.path(),
            );
            dir
        });
}
