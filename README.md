# ziv — OME-Zarr → IIIF tile server + static exporter

`ziv` is a single, zero-install Rust binary with three jobs:

- **`ziv serve <src>...`** — a live IIIF Image API 3.0 (Level 2) tile server for one image or many
  (see [`docs/multi-image.md`](docs/multi-image.md)), with a bundled OpenSeadragon viewer (channel
  toggles, z/t sliders and a segmentation-overlay picker — see [`docs/viewer.md`](docs/viewer.md)),
  dynamic region/size/rotation/quality/format requests, metadata-driven multichannel projection,
  and OME-NGFF segmentation masks (see [`docs/labels.md`](docs/labels.md)).
- **`ziv export <src> <out-dir>`** — a static export: a self-contained IIIF Image API 3.0
  (Level 0) tile tree — every file OpenSeadragon v3 will ever request, plus `info.json` and an
  embedded viewer — that you can drop on any static host (S3, GitHub Pages, a CDN) for a live
  zoomable image with zero backend. Pass `--planes` and `--labels` to also export every z-plane and
  a label overlay per plane, with a IIIF Presentation 3 manifest tying them together (see
  [Exporting planes and label overlays](#exporting-planes-and-label-overlays)), or `--dzi` to
  additionally emit a DeepZoom (`.dzi`) tile pyramid from the same rasters.
- **`ziv render <src> <out>`** — one projection, one picture: renders a single PNG or JPEG file
  and exits, using the exact same projection identifier `serve` understands, so a label overlay's
  palette and opacity can be baked into a flat image in one command (see
  [Rendering a single projection](#rendering-a-single-projection)).

All three commands read a local filesystem path or a remote object-store URL. There is no
separate config file, database, or install step: one binary, one command, one image.

## Live demo

See it running at **<https://nishad.github.io/ziv-demos/>**: four published images from the
[Image Data Resource](https://idr.openmicroscopy.org/), each turned into a static IIIF site by one
`ziv export` against the public store and served by GitHub Pages, with no server anywhere.

| Demo | Shows |
| --- | --- |
| [Nuclear segmentation](https://nishad.github.io/ziv-demos/nuclear-segmentation/) | 236 z-planes, a segmentation mask on each, in distinct colours |
| [Whole brain](https://nishad.github.io/ziv-demos/whole-brain/) | 255 megapixels, nine pyramid levels, deep zoom |
| [In situ genome sequencing](https://nishad.github.io/ziv-demos/genome-seq/) | a 64-bit integer label image |
| [Condensin map](https://nishad.github.io/ziv-demos/condensin-map/) | two separate label images on one image |

Each demo's `info.json` is a real IIIF service, and the three multi-plane demos open unmodified in
[Mirador](https://projectmirador.org/) from their card on the landing page. Each also links to the
original OME-Zarr store in [vizarr](https://github.com/hms-dbmi/vizarr) for comparison. The exact
commands that build the site are in
[`scripts/build-demos.sh`](https://github.com/nishad/ziv-demos/blob/main/scripts/build-demos.sh).

## Quickstart

```sh
cargo run -p ziv -- serve ./path/to/image.ome.zarr
# open http://127.0.0.1:3000/viewer/

# or several at once, each mounted under /i/{name}/
cargo run -p ziv -- serve ./a.ome.zarr ./b.ome.zarr
```

```sh
cargo run -p ziv -- export ./path/to/image.ome.zarr ./out
# ./out is a self-contained static site: serve the directory, e.g. `cd ./out &&
# python3 -m http.server`, or drop the whole directory on any static host. Opening
# index.html directly as file:// does not work: the viewer fetches its own info.json,
# which browsers refuse at that origin.
```

## Supported inputs

- **OME-Zarr versions:** 0.4 and 0.5. Anything else fails loud at `open()` with
  `UnsupportedVersion` rather than silently misreading the array.
- **Pixel dtypes:** `uint8`/`uint16`/`uint32`, `int8`/`int16`/`int32`, `float32`/`float64`.
  Zarr V2 arrays are accepted in either byte order: big-endian data (`>u2`, `>i4`, ...) is
  byte-swapped on decode, and the one-byte spellings `>u1`/`<u1`/`>i1`/`<i1` are accepted as
  aliases of `|u1`/`|i1` (endianness is meaningless for a single byte, but real writers emit
  the prefixed forms and the Zarr library ziv builds on registers only `|u1`/`|i1`).
- **Pyramid consistency:** every multiscale level must declare the same dtype. A pyramid whose
  levels disagree fails loud at `open()` with `InconsistentLevelDtype` naming the offending
  level, rather than opening on level 0's dtype and failing later at tile-read time.
- **One image or many:** `serve` takes one or more store specs. One is served at the root exactly as
  before; two or more are mounted at `/i/{name}/`, with a catalogue at `/ziv/images.json` and a
  picker in the viewer. See [`docs/multi-image.md`](docs/multi-image.md).
- **Label images:** an OME-NGFF `labels/` group is discovered at open and served under the
  `@overlay=NAME[:PALETTE[:OPACITY]]` identifier (the mask over the image) or `@label=NAME[:PALETTE]`
  (the mask alone), resampled nearest-neighbour because label samples are object identifiers rather
  than intensities. See [`docs/labels.md`](docs/labels.md).
- **Axes:** read via the array's own OME-Zarr axes metadata (not a positional Y/X-last-two
  assumption) — unrecognized space-axis names fail loud at `open()` with `UnknownAxis`.
- **Stores:**
  - a local filesystem path (`./image.ome.zarr`, `/abs/path/image.ome.zarr`)
  - `s3://bucket/path` (AWS S3 and S3-compatible)
  - `gs://bucket/path` (Google Cloud Storage)
  - `az://container/path` (Azure Blob Storage)
  - `http://host/path` / `https://host/path`

## Exporting planes and label overlays

A plain `ziv export` writes one Level-0 tree for the image's default view. Two opt-in flags widen
that to every z-plane and every label image, and a viewer that browses between them with no
backend:

```sh
ziv export ./image.ome.zarr ./out --planes --labels --overlay-opacity 0.6
```

- **`--planes`:** export every z-plane (at the default timepoint and channels) as its own Level-0
  tree under `planes/{z}/`. The exported viewer gets a z slider.
- **`--labels`:** export an overlay of every label image on each exported plane, under
  `planes/{z}/labels/{i}/`, in distinct colours. The exported viewer gets a label picker. See
  [`docs/labels.md`](docs/labels.md) for why overlays only, in one fixed palette and opacity.
- **`--overlay-opacity <0..1>`:** the overlays' opacity, `0.6` by default. Checked whether or not
  `--labels` is given: an out-of-range value (e.g. `2`) is an error before anything renders. A
  valid value simply has no effect without `--labels`.

Every export flag, including `--planes`/`--labels`/`--overlay-opacity`/`--id`/`--dzi`, is
documented in `man ziv-export` (`ziv man export`).

Whenever `--planes` or `--labels` makes the export render more than one view, it also writes a
IIIF Presentation 3 `manifest.json` (one canvas per exported plane, with a `Choice` of intensity
plus overlays where they exist), so the same folder opens in a standard IIIF viewer as well as
ziv's own. Pass an absolute `--id` (the URL the folder will be hosted at) to get a manifest with
valid, dereferenceable ids; without it the manifest is still usable as a template but prints a
warning and is not valid Presentation 3. A trailing slash on `--id` is trimmed (a IIIF `id` must
not carry one), and an empty `--id` is rewritten to `.`. See
[`docs/conformance.md`](docs/conformance.md), which
also records it validating cleanly against the official IIIF Presentation validator and rendering
correctly (planes as canvases, the overlay as a layer choice) in Mirador.

The layout (abbreviated to top-level entries; see `crates/exporter/src/lib.rs` for the full tree):

```text
out/
  info.json           the default view's IIIF Image API 3.0 info.json, exactly as a plain export
  full/, {x,y,w,h}/    its tiles, plus one whole-image file per size, and full/max  (within budget)
  planes/              one Level-0 tree per exported plane, and its label overlays  (--planes/--labels)
  ziv/                 dimensions.json and views.json, written by every export
  manifest.json        IIIF Presentation 3                                          (more than one view)
  index.html, viewer/  the shared viewer, in static mode; see docs/viewer.md
```

`ziv export` never cleans `out_dir` before writing: re-exporting plainly over a directory that
held a `--planes`/`--labels` export leaves the old `manifest.json` and the old `planes/` trees
behind, even though `ziv/views.json` itself correctly narrows to the new, smaller plan. Point
`ziv export` at an empty directory, or remove `out_dir` first, when the shape of the export changes
between runs.

See [`docs/viewer.md`](docs/viewer.md#static-exports) for what the exported viewer shows and hides
in static mode, and a worked example measured against a real 25-plane image.

### Remote credentials

Credentials are read from each provider's standard SDK environment variables — never from a CLI
flag:

| Store | Env vars |
|---|---|
| `s3://` | `AWS_ACCESS_KEY_ID`, `AWS_SECRET_ACCESS_KEY`, `AWS_SESSION_TOKEN`, `AWS_REGION` / `AWS_DEFAULT_REGION`, `AWS_ENDPOINT` |
| `gs://` | `GOOGLE_SERVICE_ACCOUNT`, `GOOGLE_SERVICE_ACCOUNT_KEY`, or any other `GOOGLE_*` var |
| `az://` | `AZURE_STORAGE_ACCOUNT_NAME`, `AZURE_STORAGE_ACCOUNT_KEY`, `AZURE_STORAGE_SAS_KEY`, or any other `AZURE_*` var |
| `http(s)://` | none — a plain object store, no credentials |

`http(s)://` store specs are checked against an SSRF blocklist (loopback/link-local/private
ranges, including the cloud metadata endpoint `169.254.169.254`) by default. Pass
`--allow-internal-hosts` (or set `ZIV_ALLOW_INTERNAL_HOSTS=1`) to disable this for a legitimate
in-VPC deployment.

## Rendering a single projection

`ziv render <src> <out>` renders one projection to a single PNG or JPEG file and exits. It exists
for one reason: `--at` takes the exact same projection identifier `serve`'s IIIF endpoint
understands, so a label overlay — with its palette and opacity — can be baked into a flat image in
one command. No screenshot of `serve`, and no separate tool, does that in one step.

```sh
# The image exactly as its own metadata describes it (the `default` projection).
ziv render ./image.ome.zarr ./out.png

# Plane z=10, channel 0, with the "nuclei" label overlaid at 60% opacity in the default
# ("distinct") palette — one command, one file, overlay baked in.
ziv render ./image.ome.zarr ./nuclei-overlay.jpg \
  --at "@z=10,c=0,overlay=nuclei:distinct:0.6" --format jpg --quality 90
```

- **`--at <identifier>`** (default `default`): the projection to render, parsed with the exact
  same grammar `serve` uses — `default`, or a dynamic `@z=..,t=..,c=..` selection with an optional
  `label=NAME[:PALETTE]` (the mask alone) or `overlay=NAME[:PALETTE[:OPACITY]]` (the mask
  composited over the image). See [`docs/labels.md`](docs/labels.md) for the palette/opacity
  grammar.
- **`--region <full|square|x,y,w,h|pct:x,y,w,h>`** (default `full`) and
  **`--size <w,h|w,|,h|!w,h|pct:N|max>`** (default `max`): the same `region`/`size` path-segment
  grammars `serve` accepts, parsed with the identical parsers so `render` can never accept (or
  refuse) a request the server disagrees with.
- **`--format <png|jpg>`**: inferred from `<out>`'s extension when omitted; passing it overrides
  the extension. Given neither, `render` fails with a clear message rather than guessing.
- **`--quality <0-100>`**: JPEG encoder quality (default 85). Applies to JPEG output only — passed
  alongside PNG output, it is ignored with a note, not silently.

A render whose resolved output would exceed the same whole-image budget `full/max` enforces
elsewhere (about 64 megapixels, 65535 px per edge — see [IIIF conformance](#iiif-conformance)) is
refused before any pixel is read, naming a size that would actually fit:

```text
ziv: error: out of range: requested output is 19120x13350 px, which exceeds the whole-image
budget of 64 megapixels (65535 px per edge); ask for at most 9573x6684 px instead, e.g.
--size 9573,6684
```

## IIIF conformance

- `serve` claims **Image API 3.0, Level 2**, and meets every level-2 requirement: regions
  (`full`, `square`, `x,y,w,h`, `pct:`), sizes (`max`, `w,`, `,h`, `w,h`, `!w,h`, `pct:n`),
  rotation by `0`/`90`/`180`/`270`, qualities `default`/`color`/`gray` (plus optional `bitonal`),
  formats `jpg` and `png`, CORS, the json-ld media type, and the base-URI redirect. Verified with
  the official `iiif-validate.py` against a live server — see `docs/conformance.md`.
  Optional features deliberately not implemented, and refused rather than silently ignored:
  arbitrary rotation, mirroring (`!n`), upscaling (`^size`), and the `tif`/`gif`/`pdf`/`jp2`/
  `webp` formats.
- `export` claims **Image API 3.0, Level 0** (a fixed, pre-enumerated tile set pinned via
  `sizes`, `extraFeatures: ["sizeByWh"]`). A Level 0 tree is one projection's tiles, so exporting
  more than the default view (every z-plane with `--planes`, a label overlay on each with
  `--labels`) means exporting one whole extra tree per view, not adding a request parameter; see
  [Exporting planes and label overlays](#exporting-planes-and-label-overlays). `info.json` also
  declares `maxWidth`/`maxHeight`, and every advertised size (`full/max` included) is a real file
  on disk, within a whole-image budget above which a tree serves tiles only; a pyramid that
  OpenSeadragon cannot tile correctly is refused rather than exported (view it with `ziv serve`
  instead). See [`docs/conformance.md`](docs/conformance.md) for what this means for a client, the
  budget, and the validator run against a static export.

See [`docs/conformance.md`](docs/conformance.md) for what's asserted automatically in CI (fast
`serde_json` shape checks against both `serve`'s and `export`'s `info.json`) versus how to run
the official [IIIF Image API validator](https://github.com/IIIF/image-validator) manually
against a live server.

## Auth (optional)

`serve` supports two independent (OR'd) auth mechanisms, both read from environment variables —
**never** a CLI flag, since argv is visible to other processes on the host and lands in shell
history:

- `ZIV_AUTH_BEARER=<token>` — static bearer token, checked in constant time.
- `ZIV_AUTH_HMAC_SECRET=<secret>` — HMAC-signed URLs (`?exp=<unix_seconds>&sig=<...>`); the
  signature binds the identifier + region + size, not just the raw path, and an expired or
  tampered signature is rejected.

Either, both, or neither may be set; if both are set, a request is accepted if it satisfies
*either* mechanism. Pass `--require-auth` to `ziv serve` to fail startup loud if neither is
configured, instead of silently serving unauthenticated.

## Operability

`ziv serve` is built to run behind a load balancer / orchestrator, not just as a dev tool:

- **Health checks:** `GET /healthz` (liveness, always 200) and `GET /readyz` (200 once the image
  is open and usable, 503 otherwise) — both unauthed and uncached.
- **Graceful shutdown:** drains in-flight requests on `SIGINT`/`SIGTERM` before exiting, in two
  sequential stages — connections, then renders still running on the blocking pool (a remote
  read can legitimately still be in flight after its own HTTP request has timed out) — plus a
  third drain, for images still being OPENED, that runs CONCURRENTLY with the render stage
  rather than adding more sequential time. Each sequential stage has its own timeout, both
  overridable in milliseconds without a rebuild: `ZIV_SHUTDOWN_DRAIN_TIMEOUT_MS` (default 20000)
  and `ZIV_RENDER_DRAIN_TIMEOUT_MS` (default 150000, clamped up to a 120000ms floor if set lower —
  see `server::MIN_RENDER_DRAIN_TIMEOUT`; going below that floor would reopen a shutdown-time
  panic this default exists to prevent); the open drain shares the render drain's timeout rather
  than adding its own knob. **The two sequential stages together can take up to ~170s in the
  worst case** (a slow `overlay=` render against a flaky remote), which exceeds systemd's default
  `TimeoutStopSec` (90s) and Kubernetes' default `terminationGracePeriodSeconds` (30s) — on those
  defaults, ziv is SIGKILLed mid-drain and whatever it was waiting to finish is killed anyway,
  gaining nothing over an abrupt stop. If ziv sits behind such a supervisor, either raise its
  stop/termination grace period past ~170s, or lower `ZIV_RENDER_DRAIN_TIMEOUT_MS` (down to its
  120000ms floor) and accept the smaller residual risk of a shutdown-time panic log on a
  genuinely slow render or image open. **The open drain is best-effort, not a guarantee**: unlike
  a render, an image open has no internal deadline bounding its worst case, so a badly stalling
  remote store can still exceed the drain window and print the same panic log this whole
  mechanism exists to avoid — see [`docs/operations.md`](docs/operations.md#known-limitations).
- **Resource governance:** a global concurrency limit with load-shedding to `503`, a
  whole-request timeout, and a bounded semaphore around the actual tile-rendering work so a
  burst of expensive renders can't exhaust memory.
- **Metrics:** Prometheus-format `/metrics` — request rate/latency by route, cache hit/miss
  counters, and a render-concurrency gauge.
- **HTTP caching:** `Cache-Control` + `ETag` on tiles and `info.json`, with conditional-GET
  (`If-None-Match`) support.
- **Reverse-proxy correctness:** honors `X-Forwarded-Proto`/`X-Forwarded-Host`, or an explicit
  `--public-base-url` / `ZIV_PUBLIC_BASE_URL`, so `info.json`'s `id` (and any HMAC-signed URLs)
  reflect the address clients actually use behind a TLS-terminating proxy.

## Install

### With cargo

```sh
cargo install ziv
```

That installs the `ziv` binary and nothing else: the crate named `ziv` is the tool, and the library
crates it is built from are published under a `ziv-` prefix purely as its dependencies. It needs
Rust 1.91 or newer and a C compiler, and builds the dependency tree from source, so it takes a few
minutes. If you do not already have Rust, use a release binary instead.

### From a release binary

Prebuilt, statically-linked binaries are published via [cargo-dist](https://github.com/axodotdev/cargo-dist)
for:

- Linux x86_64 / aarch64 (`musl`, static — runs on any distro, no glibc version requirement)
- macOS x86_64 / aarch64 (Intel / Apple Silicon)

Download the archive for your platform from the
[Releases page](https://github.com/nishad/ziv/releases) and put the `ziv` binary on your `PATH`, or
run the installer script published with each release:

```sh
curl --proto '=https' --tlsv1.2 -LsSf https://github.com/nishad/ziv/releases/latest/download/ziv-installer.sh | sh
```

Windows has no prebuilt binary yet; build from source or use `cargo install ziv`.

### Build from source

Requires a C toolchain (the zarr codecs — blosc, zstd, gzip — link small C libraries via `cc`) and
Rust 1.91 or newer. No `cmake` is needed: everything builds through plain `cc`, which is preinstalled on
Linux/macOS/Windows (including MSVC) CI runners and typical dev machines.

```sh
git clone https://github.com/nishad/ziv
cd ziv
cargo build --release -p ziv
./target/release/ziv --help
```

## Shell completions & man page

```sh
ziv completions bash > /etc/bash_completion.d/ziv   # or: zsh, fish, powershell, elvish
ziv man > /usr/local/share/man/man1/ziv.1                    # top-level page
ziv man export > /usr/local/share/man/man1/ziv-export.1      # per-subcommand page
ziv man serve > /usr/local/share/man/man1/ziv-serve.1
ziv man render > /usr/local/share/man/man1/ziv-render.1
```

Pre-generated completions/man output are also committed under `dist-assets/` (including
`dist-assets/man/ziv-export.1`, `ziv-serve.1` and `ziv-render.1`) and wired into the release build
via `[[workspace.metadata.dist.extra-artifacts]]` in `Cargo.toml`. A `cargo test` fails, naming the
stale file and the regen command, if a committed page drifts from what the binary generates — and
fails just as loudly if a subcommand ships with no man page row at all, since that set is derived
from clap's own subcommand list rather than a hardcoded table.

## CI / release automation

- **CI** (`.github/workflows/ci.yml`): fmt + clippy (`-D warnings`) on every push/PR, then a
  `nextest` matrix across Linux/macOS/Windows with `--locked`, plus a separate doctest job.
- **Security** (`.github/workflows/security.yml`): weekly + on `Cargo.lock`/`deny.toml` changes,
  `cargo audit` (RustSec advisories) and `cargo deny check` (advisories/licenses/bans/sources) —
  see `deny.toml` for the license allowlist and any documented, currently-unfixable transitive
  advisories.
- **Release** (`.github/workflows/release.yml`): generated by cargo-dist from
  `[workspace.metadata.dist]` in `Cargo.toml`, and triggered by a `v*` tag. It builds the
  statically linked musl Linux and macOS binaries above, attaches the shell completions and man
  pages from `dist-assets/`, and publishes a GitHub Release. Regenerate it with `dist generate`
  after changing that table.

## Troubleshooting

- **`UnsupportedVersion` / `UnknownAxis` at startup** — the image's OME-Zarr metadata version
  isn't 0.4/0.5, or a space axis has a name ziv doesn't recognize. This is a deliberate loud
  failure (no silent positional-axis fallback); check the array's `.zattrs`/`zarr.json`
  `multiscales`/`axes` metadata.
- **`UnsupportedDtype`** — the array's pixel dtype isn't one of
  u8/u16/u32/i8/i16/i32/f32/f64. Note this is about the dtype itself, not its byte order:
  both endian prefixes are accepted for every supported type.
- **`InconsistentLevelDtype` at startup** — the multiscale levels don't all declare the same
  dtype. ziv carries one dtype per image, so a mismatched level would be unreadable; check each
  level's `.zarray`/`zarr.json` `dtype` and re-encode the odd one out.
- **SSRF-guard rejection on an `http(s)://` store** — the host resolves to a loopback/
  link-local/private/metadata address, blocked by default. Pass `--allow-internal-hosts` (or
  set `ZIV_ALLOW_INTERNAL_HOSTS=1`) only for a deployment where that's actually trusted (e.g. an
  in-VPC MinIO).
- **401 on every tile request** — `ZIV_AUTH_BEARER`/`ZIV_AUTH_HMAC_SECRET` is set but the
  client isn't presenting a matching credential; check the `WWW-Authenticate` response header
  for which mechanism(s) are active.
- **Build fails looking for a C compiler** — install your platform's standard C toolchain
  (`build-essential` on Debian/Ubuntu, Xcode Command Line Tools on macOS, the MSVC Build Tools
  on Windows). No `cmake` is required.
- **503 from `serve`** — the global concurrency limit is saturated; the server is shedding load
  rather than queuing unboundedly. Retry, or raise capacity/reduce concurrent load.
- **`/readyz` returns 503** — the backing image/store isn't reachable; check the store spec and
  credentials (see [Remote credentials](#remote-credentials) above).

## Development

Documentation lives in this README, in `docs/`, and in rustdoc comments on the public APIs
(`cargo doc --open`). There is deliberately no separate documentation site: for a single binary
with two subcommands, an mdBook would duplicate this README and drift from it, and the material
that genuinely needs prose already has a home below.

| Document | What it covers |
|---|---|
| [`docs/operations.md`](docs/operations.md) | Running `ziv serve` in production: every setting, the compiled limits, shutdown behind systemd or Kubernetes, security posture, known limitations |
| [`docs/viewer.md`](docs/viewer.md) | The built-in viewer: channel toggles, z/t sliders, and the `/ziv/dimensions.json` endpoint behind them |
| [`docs/labels.md`](docs/labels.md) | Segmentation masks: rendering, overlays, palettes, and why ziv does not trust declared label colours by default |
| [`docs/multi-image.md`](docs/multi-image.md) | Serving more than one image from one process, and how images are addressed |
| [`docs/conformance.md`](docs/conformance.md) | IIIF conformance: what CI asserts, plus the official-validator runbook and results |
| [`docs/zarr-v2-dtype-endianness.md`](docs/zarr-v2-dtype-endianness.md) | Why ziv registers Zarr V2 dtype aliases instead of rewriting dtype strings |

## Citing ziv

If you use ziv in your work, please cite it. The repository's
[`CITATION.cff`](CITATION.cff) carries the full metadata, and GitHub's **Cite this repository**
button turns it into BibTeX or APA:

> Thalhath, N., & Kasaragod, D. (2026). *ziv: an OME-Zarr to IIIF Image API 3.0 tile server and
> static exporter* (Version 0.1.1) [Computer software]. https://github.com/nishad/ziv

## Licensing

`ziv` itself is MIT-licensed (see `LICENSE`).

This repository bundles a **vendored copy of [OpenSeadragon](https://openseadragon.github.io/)
v5.0.1** (`crates/viewer-assets/assets/viewer/openseadragon/`) — the JS viewer embedded in both
the live `serve` viewer and every static `export` output. OpenSeadragon is licensed under
**BSD-3-Clause**; its copyright notice and license text are reproduced at
`crates/viewer-assets/assets/viewer/openseadragon/LICENSE.txt`, which ships alongside
`openseadragon.min.js` in both the compiled binary (via `rust-embed`, in `ziv-viewer-assets`) and
every exported static tree, satisfying BSD-3-Clause's redistribution requirement.

The viewer's navigation buttons use icons from **[Lucide](https://lucide.dev) v1.45.0**
(`crates/viewer-assets/assets/viewer/nav/nav.js`), licensed under **ISC**; most of them derive
from Feather, licensed under **MIT**. Both notices are reproduced at
`crates/viewer-assets/assets/viewer/nav/LICENSE-lucide.txt`, which ships beside `nav.js` in the
binary and in every exported tree.

See `deny.toml` for the full transitive-dependency license allowlist enforced in CI via
`cargo deny check`.
