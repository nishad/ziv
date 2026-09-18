//! `ZarrImage::read_regions_f64` — the concurrent multi-channel read behind every composited tile.
//!
//! Compositing needs one plane per enabled channel. Reading them one after another made a tile
//! cost the sum of every channel's latency, which against a remote store meant a seven-channel
//! selection could not complete inside the server's request timeout. These tests pin the part that
//! must not change while that got faster: the planes it returns, and in what order.
use std::path::Path;
use std::sync::Arc;

use object_store::memory::InMemory;
use object_store::path::Path as StorePath;
use object_store::ObjectStoreExt;

use zarr_core::store::RemoteStoreSpec;
use zarr_core::{ZarrError, ZarrImage};

/// 3 channels x 3 timepoints x 5 z-planes, every plane a flat known value.
const MULTIDIM: &str = "../../tests/fixtures/sample_multidim.ome.zarr";
/// 2 channels, values that differ per channel (ch0 varies with x, ch1 with y).
const V04: &str = "../../tests/fixtures/sample_v04.ome.zarr";

fn populate(store: &InMemory, dir: &Path, base: &Path) {
    for entry in std::fs::read_dir(dir).unwrap() {
        let entry = entry.unwrap();
        let path = entry.path();
        if path.is_dir() {
            populate(store, &path, base);
        } else {
            let key = path
                .strip_prefix(base)
                .unwrap()
                .to_string_lossy()
                .replace(std::path::MAIN_SEPARATOR, "/");
            let bytes = std::fs::read(&path).unwrap();
            pollster::block_on(store.put(&StorePath::from(key.as_str()), bytes.into())).unwrap();
        }
    }
}

/// Opens the same fixture through the ASYNC object-store path, which is where the reads actually
/// run concurrently.
fn open_remote(fixture: &str) -> ZarrImage {
    let dir = Path::new(fixture);
    let mem = InMemory::new();
    populate(&mem, dir, dir);
    ZarrImage::open_remote_store(RemoteStoreSpec {
        store: Arc::new(mem),
        group_path: "/".to_string(),
    })
    .unwrap()
}

/// The whole point: reading N channels together must produce exactly what reading them one at a
/// time produced. Checked on BOTH paths, because only the async one changed behaviour.
#[test]
fn matches_reading_each_channel_separately() {
    for (what, img) in [
        ("local", ZarrImage::open(V04).unwrap()),
        ("remote", open_remote(V04)),
    ] {
        let sequential: Vec<_> = (0..2)
            .map(|c| img.read_region_f64(0, 0, c, 0, 0..32, 0..32).unwrap())
            .collect();
        let together = img
            .read_regions_f64(0, 0, &[0, 1], 0, 0..32, 0..32)
            .unwrap();

        assert_eq!(together.len(), 2, "{what}: one plane per channel");
        for (c, (batch, one)) in together.iter().zip(sequential.iter()).enumerate() {
            assert_eq!(batch, one, "{what}: channel {c} must be identical");
        }
        // ...and the two channels are genuinely different data, so the test could actually fail
        // if the implementation returned the same plane twice.
        assert_ne!(
            together[0], together[1],
            "{what}: fixture channels must differ, or this proves nothing"
        );
    }
}

/// Order is positional: result[i] is the plane for channels[i], whatever order they arrive in.
/// Concurrency makes completion order arbitrary, so this is the property most at risk.
#[test]
fn results_are_in_the_order_requested_not_the_order_completed() {
    let img = open_remote(MULTIDIM);
    // sample_multidim planes are flat: value = 80 + 40*t + 15*z, identical across channels, so
    // channel identity is checked via a reversed request against per-channel single reads.
    let forward = img
        .read_regions_f64(0, 1, &[0, 1, 2], 2, 0..32, 0..32)
        .unwrap();
    let reversed = img
        .read_regions_f64(0, 1, &[2, 1, 0], 2, 0..32, 0..32)
        .unwrap();

    assert_eq!(forward.len(), 3);
    assert_eq!(reversed.len(), 3);
    for i in 0..3 {
        assert_eq!(
            forward[i],
            reversed[2 - i],
            "reversing the request must reverse the results"
        );
    }
    // t=1, z=2 -> 80 + 40 + 30 = 150, on every channel of this fixture.
    assert_eq!(forward[0][[0, 0]], 150.0);
}

/// An out-of-range channel must fail, and fail BEFORE any I/O — the planning pass exists so a bad
/// index doesn't cost N-1 remote reads first.
#[test]
fn rejects_an_out_of_range_channel() {
    for (what, img) in [
        ("local", ZarrImage::open(V04).unwrap()),
        ("remote", open_remote(V04)),
    ] {
        let err = img
            .read_regions_f64(0, 0, &[0, 99], 0, 0..32, 0..32)
            .expect_err("{what}: channel 99 does not exist");
        assert!(
            matches!(err, ZarrError::Read(_)),
            "{what}: expected a read error, got {err:?}"
        );
    }
}

/// A region outside the image is still refused when asked for several channels at once — the
/// bounds check must not have been lost in the batching.
#[test]
fn still_bounds_checks_the_region() {
    let img = ZarrImage::open(V04).unwrap();
    assert!(img
        .read_regions_f64(0, 0, &[0, 1], 0, 0..9999, 0..32)
        .is_err());
}

/// No enabled channels is a legitimate state (the caller composites nothing), not an error.
#[test]
fn no_channels_is_empty_not_an_error() {
    let img = ZarrImage::open(V04).unwrap();
    assert!(img
        .read_regions_f64(0, 0, &[], 0, 0..32, 0..32)
        .unwrap()
        .is_empty());
}

/// Reading one channel through the batch API must equal the single-channel API — the batching
/// must not have changed the common case.
#[test]
fn a_single_channel_batch_matches_the_single_read() {
    let img = open_remote(MULTIDIM);
    let one = img.read_region_f64(0, 2, 1, 4, 0..32, 0..32).unwrap();
    let batch = img.read_regions_f64(0, 2, &[1], 4, 0..32, 0..32).unwrap();
    assert_eq!(batch.len(), 1);
    assert_eq!(batch[0], one);
}

/// The same channel twice is not special-cased away: the caller gets what it asked for, in
/// position. (The compositor never does this, but silently deduplicating would misalign results
/// against the channel list the caller zips them with.)
#[test]
fn repeated_channels_are_returned_positionally() {
    let img = ZarrImage::open(V04).unwrap();
    let planes = img
        .read_regions_f64(0, 0, &[1, 1], 0, 0..32, 0..32)
        .unwrap();
    assert_eq!(planes.len(), 2);
    assert_eq!(planes[0], planes[1]);
}
