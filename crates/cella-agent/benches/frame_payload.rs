//! Cost of reading one credential-tunnel frame payload.
//!
//! `cella-agent` is a binary crate, so this bench cannot call
//! `credential_mux::read_payload` directly. It instead runs the two code
//! shapes that function had before and after, over the same in-memory reader,
//! so the numbers describe the transformation that was applied rather than
//! standing in for the function itself.
//!
//! Before: allocate `len` bytes and zero them, overwrite every one via
//! `read_exact`, then allocate again and copy the whole payload into `Bytes`
//! for hyper. After: fill spare capacity with `read_buf` and `freeze()`, which
//! hands the same allocation to hyper as a refcount bump.

use bytes::{Bytes, BytesMut};
use tokio::io::{AsyncReadExt as _, BufReader};

#[global_allocator]
static ALLOC: divan::AllocProfiler = divan::AllocProfiler::system();

fn main() {
    divan::main();
}

/// Frame payload sizes: a JSON envelope, a typical body chunk, and a large one.
/// `MAX_REQUEST_CHUNK` in the mux is 16 MiB.
const SIZES: &[usize] = &[4 * 1024, 256 * 1024, 4 * 1024 * 1024];

fn runtime() -> tokio::runtime::Runtime {
    tokio::runtime::Builder::new_current_thread()
        .build()
        .expect("runtime")
}

fn source(len: usize) -> Vec<u8> {
    (0..len).map(|i| (i % 251) as u8).collect()
}

/// Zero-then-overwrite, then copy again into `Bytes`.
#[divan::bench(args = SIZES)]
fn vec_zeroed_then_copy(bencher: divan::Bencher, len: usize) {
    let rt = runtime();
    let data = source(len);
    bencher.bench(|| {
        rt.block_on(async {
            let mut reader = BufReader::new(&data[..]);
            let mut buf = vec![0u8; len];
            reader.read_exact(&mut buf).await.expect("read");
            let out = Bytes::copy_from_slice(&buf);
            assert_eq!(out.len(), len, "short read");
            divan::black_box(out)
        })
    });
}

/// Fill spare capacity, then hand the same allocation onward.
#[divan::bench(args = SIZES)]
fn bytesmut_read_buf_freeze(bencher: divan::Bencher, len: usize) {
    let rt = runtime();
    let data = source(len);
    bencher.bench(|| {
        rt.block_on(async {
            let reader = BufReader::new(&data[..]);
            let mut buf = BytesMut::with_capacity(len);
            let mut limited = reader.take(len as u64);
            while buf.len() < len {
                if limited.read_buf(&mut buf).await.expect("read") == 0 {
                    break;
                }
            }
            let out = buf.freeze();
            assert_eq!(out.len(), len, "short read");
            divan::black_box(out)
        })
    });
}
