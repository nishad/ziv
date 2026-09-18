# Changelog

All notable changes to ziv are recorded here. The format follows
[Keep a Changelog](https://keepachangelog.com/en/1.1.0/), and the project uses
[Semantic Versioning](https://semver.org/).

## [0.1.1] - 2026-09-19

The first release with prebuilt binaries. Nothing changes in how images are served, exported or
rendered.

### Fixed

- **Release binaries.** The 0.1.0 release build failed on every platform, because the workspace
  had no `[profile.dist]` for cargo-dist to build with. 0.1.0 was therefore published to crates.io
  only; 0.1.1 is the first release with downloadable binaries and the installer script.
- **Closed output pipes.** `ziv completions <shell>` panicked, and `ziv man` printed
  `Broken pipe (os error 32)`, when the reader closed the output early, as in
  `ziv completions bash | head`. Both now exit quietly, as standard command-line tools do.

## [0.1.0] - 2026-09-19

Initial release.

- `ziv serve`: a live IIIF Image API 3.0 (Level 2) server for one or many OME-Zarr images, from a
  local path or `s3://`, `gs://`, `az://` or `https://` object storage, with a bundled viewer.
- `ziv export`: a static IIIF Level 0 tile tree for any static host, with every z-plane and a
  label overlay per plane on request, tied together by a IIIF Presentation 3 manifest.
- `ziv render`: one projection to a single PNG or JPEG, using the same identifier grammar as
  `serve`, so a label overlay can be baked into a flat image in one command.
- Selection across time, depth, channel and segmentation mask is expressed inside the IIIF
  identifier, so unmodified IIIF clients consume it.
- Requires Rust 1.91 or newer to build, the minimum set by its `zarrs` dependency.

[0.1.1]: https://github.com/nishad/ziv/compare/v0.1.0...v0.1.1
[0.1.0]: https://github.com/nishad/ziv/releases/tag/v0.1.0
