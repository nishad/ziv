//! Opening an OME-NGFF label stored as `uint64` (`<u8`) — `DType::U64`'s only committed fixture
//! (see its doc comment for why `U64` is otherwise spot-checked rather than fixture-covered).
//!
//! Fixture: `tests/fixtures/sample_u64_label.ome.zarr`, built by `build_i64_label_fixture.rs`'s
//! `builds_u64_fixture` test. Its on-disk bytes are identical to `sample_i64_label.ome.zarr`'s
//! (every value it writes is non-negative); only the declared dtype differs, which is the point —
//! this proves `classify_dtype`'s `uint64` branch and `widen!(u64)` are genuinely exercised, not
//! merely reachable in principle because `int64` already works.
use zarr_core::ZarrImage;

const FIXTURE: &str = "../../tests/fixtures/sample_u64_label.ome.zarr";

#[test]
fn a_uint64_label_is_discovered_with_the_right_dtype_and_values() {
    let img = ZarrImage::open(FIXTURE).unwrap();
    let label = &img.label("cells").unwrap().image;
    assert_eq!(label.dtype(), zarr_core::DType::U64);

    let plane = label.read_region_f64(0, 0, 0, 0, 0..8, 0..8).unwrap();
    // Same quadrants as the int64 fixture: TL 0, TR 1, BL 2, BR 5_000_000_000 (already beyond
    // u32::MAX, so this also proves the read path does not narrow to 32 bits).
    assert_eq!(plane[[0, 0]], 0.0);
    assert_eq!(plane[[7, 7]], 5_000_000_000.0);
}
