//! Standalone driver for profiling [`extract_layer`] under perf/valgrind/strace.
//!
//! The divan bench cannot be profiled directly: `perf` samples the whole
//! process, and the bench harness builds a fresh tar and a fresh `TempDir` per
//! iteration, so most of what it does is fixture work that divan excludes from
//! its timing but perf still attributes. This driver hoists every bit of that
//! out of the measured loop — the archive is built once and the destination
//! directories are created up front — so a profile of this process is a
//! profile of extraction and nothing else.
//!
//! ```text
//! cargo build --profile profiling --example profile_extract -p cella-oci
//! ./target/profiling/examples/profile_extract <entries> <iterations> <dest-root>
//! ```

use std::io::Write as _;
use std::path::PathBuf;

use cella_oci::extract::{DEVCONTAINERS_LAYER_MEDIA_TYPE, extract_layer};

/// Build a tar shaped like a real layer: nested directories, small files, and
/// a symlink every 16th entry. Mirrors the bench fixture so numbers compare.
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

fn main() {
    let mut args = std::env::args().skip(1);
    let entries: usize = args.next().and_then(|a| a.parse().ok()).unwrap_or(4096);
    let iterations: usize = args.next().and_then(|a| a.parse().ok()).unwrap_or(10);
    let dest_root = PathBuf::from(
        args.next()
            .unwrap_or_else(|| "/tmp/cella-profile-extract".to_owned()),
    );

    // Everything below this point is setup and must stay out of the loop.
    let blob = build_layer(entries);
    let _ = std::fs::remove_dir_all(&dest_root);
    let dests: Vec<PathBuf> = (0..iterations)
        .map(|i| {
            let d = dest_root.join(format!("run{i}"));
            std::fs::create_dir_all(&d).expect("create dest");
            d
        })
        .collect();

    let mut out = std::io::stderr();
    writeln!(
        out,
        "profiling {entries} entries x {iterations} iterations into {}",
        dest_root.display()
    )
    .ok();

    let start = std::time::Instant::now();
    for dest in &dests {
        extract_layer(&blob, DEVCONTAINERS_LAYER_MEDIA_TYPE, dest).expect("extract");
    }
    let elapsed = start.elapsed();

    writeln!(
        out,
        "total {:?}  per-iteration {:?}  per-entry {:?}",
        elapsed,
        elapsed / u32::try_from(iterations).unwrap_or(1),
        elapsed / u32::try_from(iterations * entries).unwrap_or(1),
    )
    .ok();
}
