//! Static IIIF Image API 3.0 Level-0 export: `serve` materialized to disk.
//!
//! Given a `&dyn TileEngine`, [`export`] enumerates the OSD v3 request space
//! ([`enumerate_request_space`]) and writes every tile OpenSeadragon would ever request as a real
//! file, plus the IIIF Level-0 contract itself: a whole-image file for every size the tree's
//! `info.json` may advertise, and `full/max` (see [`writer::write_tree`]'s doc comment). A level0
//! profile is not honest until `full/max` actually resolves, and this is what makes it so for
//! every tree, not just the ones a manifest happens to reference, within a budget on the whole
//! image's pixels and edge length (`iiif::level0::MAX_WHOLE_IMAGE_PIXELS`/`MAX_WHOLE_IMAGE_EDGE`):
//! above it, a tree keeps its untrimmed `sizes`, skips every whole-image derivative including
//! `full/max`, and the export warns that it serves tiles only. A pyramid that OpenSeadragon
//! cannot tile at all (`iiif::level0_sizes` returns `None`) is refused before `out_dir` is created
//! ([`ExportError::UnsupportedPyramid`]); `ziv serve` is level 2 and reads `sizes` dynamically, so
//! such an image is never unviewable, only not exportable as a static tree. Plus the level0
//! `info.json` itself per tree and the shared viewer, split across `index.html` and `viewer/`
//! (not a single self-contained page: the viewer fetches its own `info.json` and
//! `ziv/views.json`, so it must be served, not opened as `file://`). Drop the output directory on
//! any static host and it is a live zoomable image with zero backend.
//!
//! # Export layout
//!
//! The image is exported at the OUT-DIR ROOT (not a per-image subdirectory) — the
//! simplest layout that still resolves correctly under OSD's relative-URL requests:
//!
//! ```text
//! {out_dir}/
//!   info.json                           <- id: "." (relative, resolves under any path/host)
//!   index.html                          <- the shared viewer page, mode marker set to static
//!   manifest.json                       <- IIIF Presentation 3 manifest, written with >1 view
//!   full/{w},{h}/0/default.jpg          <- whole-image files: every size info.json may advertise
//!   full/max/0/default.jpg              <- within budget: the IIIF Level-0 contract's own URL
//!   {x,y,w,h}/{w,h}/0/default.jpg       <- one directory tree per region tile
//!   planes/{z}/...                      <- one full Level-0 tree per extra exported z-plane
//!   planes/{z}/labels/{i}/...           <- one full Level-0 tree per overlay of label i on plane z
//!   viewer/viewer.js, viewer/viewer.css <- the viewer, the same files `serve` embeds
//!   viewer/openseadragon/...            <- vendored OpenSeadragon build and its licence
//!   viewer/nav/...                      <- the Lucide navigation buttons and their licence
//!   ziv/dimensions.json, ziv/views.json <- what the static viewer builds its controls from
//! ```
//!
//! `info.json`'s `id` is the literal string `.` (a relative reference to "this
//! directory"), so `{id}/{region}/{size}/0/default.jpg` — the IIIF Image API's own
//! templated tile URL — resolves via ordinary relative-URL rules to
//! `./{region}/{size}/0/default.jpg` under whatever path/host/CDN the folder is served
//! from, with no `--base-url` configuration required for the common case. A caller with a
//! reason to publish an absolute `@id` (e.g. so an external IIIF client can dereference it
//! out of context) can still pass one via [`ExportOptions::id`].
//!
//! [`export`] never cleans `out_dir` before writing: re-exporting plainly over a directory that
//! held a `--planes`/`--labels` export leaves that export's `manifest.json` and `planes/` trees
//! behind, even though `ziv/views.json` itself correctly narrows to the new, smaller plan. Point
//! it at an empty directory, or remove `out_dir` first, when the shape of the export changes
//! between runs.

pub mod dzi;
pub mod enumerate;
pub mod manifest;
pub mod viewer;
pub mod views;
pub mod writer;

/// The ambient tokio runtime's handle, if this call is happening anywhere underneath one --
/// `None` otherwise, with no panic either way.
///
/// Both `writer::write_tree` and `dzi::export_dzi` render tiles with `rayon`'s `.par_iter()`.
/// Rayon's global thread-pool workers are plain OS threads with no tokio context of their own,
/// but a remote-backed tile read eventually drives a `zarr_core` future with
/// `pollster::block_on` (see that crate's `image.rs` for why `pollster` specifically), and the
/// underlying HTTP client's sockets/timers need a live tokio reactor to do real network I/O --
/// `pollster` itself provides none. `ziv export` always runs its actual export inside
/// `tokio::task::spawn_blocking` (see `crates/cli/src/lib.rs`'s `Command::Export` arm), so a
/// runtime genuinely exists on the thread that calls into this crate; it just isn't visible to
/// the rayon workers spawned underneath. Capturing the handle HERE, on that calling thread, and
/// re-entering it inside each worker closure (`Handle::enter`) bridges exactly that gap, without
/// spawning any task or blocking on anything itself.
///
/// For a local-filesystem export -- including every test in this crate, all of which call
/// `export`/`export_dzi` from a plain synchronous `#[test]` fn with no runtime anywhere in the
/// process -- this returns `None` and every worker closure's `.enter()` call is skipped
/// entirely. That is correct, not merely tolerated: the local read path (`zarr_core::image`'s
/// `Levels::Sync` branch) never calls `pollster::block_on` and so never needs a reactor, meaning
/// a local export must keep working with no ambient runtime at all.
pub(crate) fn ambient_runtime_handle() -> Option<tokio::runtime::Handle> {
    tokio::runtime::Handle::try_current().ok()
}

pub use enumerate::{enumerate_request_space, EnumeratedRequest};
pub use manifest::{manifest_json, NO_MANIFEST_OVER_BUDGET_WARNING, RELATIVE_MANIFEST_WARNING};
pub use views::{
    plan_views, tree_id, views_json, View, ViewPlan, ViewSelection, DEFAULT_OVERLAY_OPACITY,
    OVERLAY_PALETTE,
};
pub use writer::{
    export, export_with_progress, write_tree, ExportError, ExportEvent, ExportOptions,
    ExportSummary,
};
