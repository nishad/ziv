//! Criterion micro-benchmarks for the tile-rendering pipeline.
//!
//! `bench_engine_tile` measures the full `TileEngine::tile` path (zarr read + composite +
//! resample + JPEG encode) over a handful of fixed (region, size) requests against the
//! committed `sample_multi_tile.ome.zarr` fixture -- a real multi-tile pyramid, not the 64x64
//! toy used by most unit tests, so the numbers reflect genuine per-tile cost. `bench_composite`
//! and `bench_resize_rgb8` isolate the two pure-function hot spots inside that path so a
//! regression can be attributed to compositing vs. resampling vs. I/O without re-running the
//! full engine each time.
//!
//! The `ZarrTileEngine` (which opens the fixture and does the one-time startup percentile-window
//! read) is built ONCE in `main` via criterion's `Criterion::default()` harness setup below, not
//! per-iteration -- opening/auto-stretching is startup cost, not per-request cost, and would
//! otherwise dominate every sample.
//!
//! Run with `cargo bench -p ziv-tiling`. `cargo bench -p ziv-tiling --no-run` compiles without executing
//! (used as the CI-safe smoke check so a full bench run never blocks CI).

use std::hint::black_box;

use criterion::{criterion_group, criterion_main, Criterion};
use iiif::{ProjectionId, Region, Size};
use ndarray::Array2;
use projection::{composite, ChannelView, Lut};
use tiling::{resize_rgb8, TileEngine, ZarrTileEngine};
use zarr_core::ZarrImage;

const FIXTURE: &str = "../../tests/fixtures/sample_multi_tile.ome.zarr";

fn build_engine() -> ZarrTileEngine {
    let image = ZarrImage::open(FIXTURE).expect("open committed multi-tile fixture");
    ZarrTileEngine::new(image)
}

/// `engine.tile` over a few fixed, representative (region, size) requests: a full-image
/// thumbnail (heavy downscale, exercises coarse-level selection), a native-resolution region
/// tile (no resample), and a downscaled region tile (partial resample) -- the three shapes of
/// request an IIIF client actually issues.
fn bench_engine_tile(c: &mut Criterion) {
    let engine = build_engine();
    let id = ProjectionId::Default;

    let mut group = c.benchmark_group("engine_tile");

    group.bench_function("full_thumbnail_256", |b| {
        b.iter(|| {
            engine
                .tile(
                    black_box(&id),
                    black_box(Region::Full),
                    black_box(Size::Wh(256, 256)),
                    black_box(85),
                )
                .expect("tile render")
        })
    });

    group.bench_function("region_512_native", |b| {
        b.iter(|| {
            engine
                .tile(
                    black_box(&id),
                    black_box(Region::Px {
                        x: 0,
                        y: 0,
                        w: 512,
                        h: 512,
                    }),
                    black_box(Size::Max),
                    black_box(85),
                )
                .expect("tile render")
        })
    });

    group.bench_function("region_1024_downscaled_to_512", |b| {
        b.iter(|| {
            engine
                .tile(
                    black_box(&id),
                    black_box(Region::Px {
                        x: 0,
                        y: 0,
                        w: 1024,
                        h: 1024,
                    }),
                    black_box(Size::Wh(512, 512)),
                    black_box(85),
                )
                .expect("tile render")
        })
    });

    group.finish();
}

/// Isolated `composite` bench: a single grey channel over a 512x512 plane, no zarr I/O, no
/// resample -- attributes cost purely to the per-pixel window-map + LUT + accumulate loop.
fn bench_composite(c: &mut Criterion) {
    let dim = 512usize;
    let plane = Array2::from_shape_fn((dim, dim), |(r, col)| ((r + col) % 256) as f64);
    let view = ChannelView {
        index: 0,
        window: (0.0, 255.0),
        lut: Lut::Grey,
        enabled: true,
    };

    c.bench_function("composite_512x512_single_channel", |b| {
        b.iter(|| composite(black_box(&[(&view, plane.clone())])))
    });
}

/// Isolated `resize_rgb8` bench: sRGB-correct Lanczos3 downscale of a 1024x1024 RGB8 buffer to
/// 256x256, no zarr I/O, no compositing -- attributes cost purely to the srgb-mapper + resize
/// path.
fn bench_resize_rgb8(c: &mut Criterion) {
    let src_w = 1024u32;
    let src_h = 1024u32;
    let src: Vec<u8> = (0..(src_w as usize * src_h as usize * 3))
        .map(|i| (i % 256) as u8)
        .collect();

    c.bench_function("resize_rgb8_1024_to_256", |b| {
        b.iter(|| {
            resize_rgb8(
                black_box(&src),
                black_box(src_w),
                black_box(src_h),
                black_box(256),
                black_box(256),
            )
            .expect("resize")
        })
    });
}

criterion_group!(
    benches,
    bench_engine_tile,
    bench_composite,
    bench_resize_rgb8
);
criterion_main!(benches);
