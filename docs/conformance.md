# IIIF Image API 3.0 conformance

ziv serves and exports **IIIF Image API 3.0** — `serve` at conformance profile **Level 2**
(dynamic region/size/rotation/quality/format requests, JPEG output), `export` at conformance
profile **Level 0** (a fixed, pre-enumerated set of tiles, pinned via `sizes`).

## CI-runnable conformance (this repo, automatic)

Every `cargo test --workspace` run asserts the `info.json` shape against the parts of the
[Image API 3.0 spec](https://iiif.io/api/image/3.0/) that are mechanically checkable from the
JSON alone:

- `@context` == `http://iiif.io/api/image/3/context.json`
- `type` == `ImageService3`
- `protocol` == `http://iiif.io/api/image`
- `profile` is `level0` or `level2`
- `id` is non-empty with no trailing slash
- `tiles` is well-formed (`width` + non-empty positive `scaleFactors`)
- `sizes`, when present, is well-formed, and for `level0` is required and pinned 1:1 against
  `scaleFactors` (the exact `levelSizes` validation OpenSeadragon's `IIIFTileSource` constructor
  runs before trusting a level0 service's finite request space)

This lives in `iiif::conformance::assert_info_json_conforms` and is exercised against:
- a **live** `serve` router (`crates/server/tests/iiif_conformance.rs`) — both the `default`
  projection and a dynamic `@z=..,c=..` projection identifier
- a real **exported** tree on disk (`crates/exporter/tests/iiif_conformance.rs`) — both the
  default relative `id` (`"."`) and an explicit absolute `id`
- **every tree of a `--planes --labels` export** (`crates/exporter/tests/views_export.rs`,
  `every_plane_is_a_complete_conforming_tree_with_no_orphans`), checking the root, every plane and
  every overlay, not just the default view

This is fast, hermetic, and requires nothing beyond `cargo test` — it is the conformance bar CI
enforces on every push/PR.

## Official validator (manual, live-endpoint) — RUN, and what it found

The [IIIF Image API validator](https://github.com/IIIF/image-validator) (`iiif-validate.py`)
black-box tests a **running** server's actual pixel responses against its own reference image. It
has supported Image API 3.0 since v1.0.5, but **defaults to 2.0**, so `--version=3.0` must be
passed explicitly or it silently validates against the wrong spec version.

It is not run in CI (it needs a live listener, Python, and a generated fixture), but it has been
run against `ziv serve`, and the first run mattered: **it found that ziv's advertised
`"profile": "level2"` was an overclaim.** Six required features were missing and two classes of
invalid request were being answered `200`. The in-repo `iiif::conformance` assertions could not
have caught any of it — they check the *shape* of `info.json`, not whether the server honours
what that shape promises. Everything it found is now implemented and pinned by
`crates/server/tests/iiif_level2.rs`:

| Feature | Required at | Was | Now |
|---|---|---|---|
| `jsonldMediaType` | level 1 | always `application/json` | honours `Accept: application/ld+json`, with the Image API context as the `profile` parameter |
| `baseUriRedirect` | level 1 | `/iiif/{id}` → 404 | 303 → `{id}/info.json` |
| `sizeByConfinedWh` (`!w,h`) | level 2 | 400 | fits inside the box, aspect preserved |
| `rotationBy90s` | level 2 | 400 | `0`/`90`/`180`/`270` served; arbitrary + mirroring still refused |
| quality `gray` | level 2 | accepted, silently ignored, returned colour | real Rec. 601 luma (plus optional `bitonal`) |
| format `png` | level 2 | 400 | served, lossless |
| unknown identifier | — | `200` (rendered the default image) | 404 |
| unknown quality | — | `200` (silently ignored) | 400 |
| size larger than the region | — | silently upscaled | 400 — upscaling is only legal via `^`, which ziv does not implement and now also refuses |

### Reproducing the run

The validator needs a server that is serving *its own* reference image: a 1000x1000 grid of ten
by ten 100x100 colour squares whose exact RGB values live in `ValidationInfo.colorInfo` in
`iiif_validator/validator.py`. That is the step this document previously left unresolved. The
validator does not ship the image as a file, so generate it as an OME-Zarr — three uint8 channels
carrying the R, G and B planes, with `omero` channel colours `FF0000`/`00FF00`/`0000FF` and
windows `0-255`, which ziv composites back into the original RGB:

```python
from iiif_validator.validator import ValidationInfo
colorInfo = ValidationInfo().colorInfo          # colorInfo[x][y] is the block at column x, row y
# write a (3, 1000, 1000) |u1 Zarr v2 array where plane 0/1/2 = R/G/B of colorInfo[x//100][y//100]
```

Then:

```sh
pip install iiif-validator
cargo run --release -p ziv -- serve /path/to/validator.ome.zarr --addr 127.0.0.1:3000
iiif-validate.py -s 127.0.0.1:3000 -p iiif -i default --version=3.0 --level 2 -v
```

`-s` is `host:port`, `-p` the prefix before the identifier (`iiif`, matching ziv's
`/iiif/{proj}/...` routes), `-i` the identifier (`default`), `--version=3.0` the spec version
(**do not omit**), `--level 2` the profile `serve` declares.

### Reading the result

The current result is **33 tests, 0 failures**, with two caveats worth knowing before you treat a
red run as a regression:

- **`size_region` is intermittently red on JPEG quantization, not on conformance.** It downscales
  a single uniform colour block and requires all five sampled squares to land within 6 of the
  reference RGB (the other colour tests need only four of five). At small sizes, JPEG at quality
  85 can shift a near-zero channel by exactly 6 — observed on `(6, 85, 234)` rendered at 43x43,
  which came back `(0, 86, 235)`. Re-run before investigating; it passes consistently.
- **`size_noup` can fail with a Python `TypeError` rather than a verdict.** That is a bug in the
  validator itself (`size_noup.py` raises `ValidatorError()` with no arguments), which masks its
  actual finding. Check the underlying property by hand: a size larger than the region must
  return 400.

### Level 0 (manual, static export): RUN

A **level0** conformance check against a static `export` tree works the same way: serve the
exported directory with any static file server and point the validator at it with `--level 0`.
The validator probes a *dynamic* service's request grammar, so in general a level0 static tree
(one that answers only the fixed, pre-enumerated URL set) can fail any check that constructs a
request outside that set; in practice `--level 0` issues only 5 tests, all against URLs a level0
tree actually promises, and all 5 passed.

The validator needs its own reference image the same way the level2 run above does. Since `export`
writes a directory tree rather than opening a socket, this used a fresh single-level (no-pyramid)
OME-Zarr built the same way (3 uint8 channels carrying the reference's R/G/B planes, `omero`
colours `FF0000`/`00FF00`/`0000FF`, windows `0-255`), exported with this repository's own binary
and served statically:

```sh
pip install iiif-validator   # in a throwaway virtualenv
# build a (3, 1000, 1000) |u1 Zarr v2 array from iiif_validator.validator.ValidationInfo().colorInfo,
# the same way the level2 fixture above is built
cargo run --release -p ziv -- export /path/to/validator-src.ome.zarr ./serve-root/iiif/default \
  --id http://127.0.0.1:PORT/iiif/default
python3 -m http.server PORT --bind 127.0.0.1   # from ./serve-root
iiif-validate.py -s 127.0.0.1:PORT -p iiif -i default --version=3.0 --level 0 -v
```

This particular reference image has no pyramid, so it hits the D9/D6 cost warning documented
below (`each tree writes a 1000x1000 whole image at full resolution to satisfy level0's full/max`).
That is expected, and unrelated to the validator's result.

**Verbatim result: `Done (5 tests, 0 failures)`.**

```
[1] test format_jpg PASS
  url: ['http://127.0.0.1:PORT/iiif/default/full/max/0/default.jpg']
  tests: ['quality']

[2] test id_basic PASS
  url: ['http://127.0.0.1:PORT/iiif/default/full/max/0/default.jpg']
  tests: ['status']

[3] test id_squares PASS
  url: ['http://127.0.0.1:PORT/iiif/default/full/max/0/default.jpg']
  tests: ['status', '0,9:True', '7,6:True', '8,9:True', '0,7:True', '1,9:True']

[4] test info_json PASS
  url: ['http://127.0.0.1:PORT/iiif/default/info.json']
  tests: ['required-field: width', 'required-field: height', 'type-is-int: height', 'type-is-int: width', 'required-field: id', 'type-is-uri: id', 'id is correct URI', 'required-field: @context', 'correct-context', 'required-field: protocol', 'correct-protocol', 'required-field: profile', 'profile-compliance', 'is-list', 'is-object', 'required-field: height', 'required-field: width', 'type-is-int: height', 'type-is-int: width', 'is-list', 'is-object', 'required-field: scaleFactors', 'required-field: width', 'type-is-int: width', 'correct-type', 'license-renamed', 'is-list', 'attribution-missing', 'logo-missing']

[5] test size_nofull PASS
  url: ['http://127.0.0.1:PORT/iiif/default/full/full/0/default.jpg']
  tests: ['size']

Done (5 tests, 0 failures)
```

All five are genuinely level0-relevant: `format_jpg`/`id_basic` resolve `full/max/0/default.jpg`,
the level 0 minimum, as a real JPEG; `id_squares` decodes it and checks five sampled colour
squares against the reference, proving the served pixels are correct rather than merely present;
`info_json` checks the level0 `info.json` shape end to end; `size_nofull` confirms the deprecated
2.0 `.../full/0/default.jpg` (no `max`) correctly does not resolve, since a level0 tree never
writes that file. The CI-runnable `level0` assertions above, plus the exporter's own completeness
test, remain the fast, hermetic bar this repository actually gates on; this manual run is the
external check that ziv's own belief about what it serves matches an independent validator's.

## `sizes`, `maxWidth`/`maxHeight`, and what a level0 export actually serves

Every exported tree declares `"profile": "level0"` and a `sizes` array (spec: IIIF Image API 3.0
requires `sizes` for level0). A level0 service's contract is that it answers every request `sizes`
advertises, and the one every level0 client needs first is the full image at its own listed size
(`full/{w},{h}/0/default.jpg`, `full/max/...` for the largest listed size). Until this was fixed,
`export` did not write that file for every size it listed, and the gap was measurable, not
theoretical. `tests/fixtures/sample_multi_tile.ome.zarr`, exported plain
(`cargo run -p ziv -- export tests/fixtures/sample_multi_tile.ome.zarr /tmp/mt`), used to produce:

```console
$ cat /tmp/mt/info.json   # abridged
"sizes": [{"width": 1024, "height": 1024}, {"width": 512, "height": 512}],
"extraFeatures": ["sizeByWh"]
$ find /tmp/mt/full -maxdepth 1
/tmp/mt/full
/tmp/mt/full/512,512
```

`sizes` named both 1024x1024 (the full-resolution image) and 512x512, but `full/` on disk held
only `512,512`. `full/max/0/default.jpg`, the request every level0 client tries first and the one
`sizes` promised existed, 404'd. The same gap was present in the yeast worked example in
[`viewer.md`](viewer.md#static-exports): its `info.json` listed 2048x2048 and 1024x1024 among its
`sizes`, but `full/` held only 512x512 down to 64x64. That measurement is what drove the fix below;
it is kept here as the evidence, not as today's behaviour.

**That gap is now closed.** The same `sample_multi_tile.ome.zarr` export today:

```console
$ cat /tmp/mt/info.json   # abridged
"sizes": [{"width": 512, "height": 512}],
"maxWidth": 512, "maxHeight": 512,
"extraFeatures": ["sizeByWh"]
$ find /tmp/mt/full -maxdepth 1
/tmp/mt/full
/tmp/mt/full/512,512
/tmp/mt/full/max
```

### What `maxWidth`/`maxHeight` mean for a client

`info.json` now declares `maxWidth`/`maxHeight`: the IIIF Image API 3.0 bound a client "must not
expect requests with a width greater than this value to be supported", and the value `full/max`
resolves to. Every entry left in `sizes` is `<= maxWidth`/`maxHeight`, and every one of them,
`sizes[0]` included, is a whole-image file the export actually wrote: `crates/exporter`'s own
completeness test decodes every one, `full/max` included, and asserts its pixel dimensions against
what `info.json` declares, across every kind of committed fixture and every tree of a
`--planes --labels` export. That is the assertion whose absence let this gap exist in the first
place.

`sizes` is trimmed by at most the single full-resolution entry, and only when OpenSeadragon
provably reconstructs it without that entry (`iiif::level0_sizes`, `crates/iiif/src/level0.rs`):
OpenSeadragon's own `IIIFTileSource` constructor rebuilds the pyramid's level sizes from `sizes`
and pushes `{width, height}` back on when `sizes.length == maxLevel`, so dropping exactly that one
entry is invisible to it. **Full resolution is never lost: it stays reachable, tiled**, through the
ordinary `{x,y,w,h}/{w,h}/0/default.jpg` grid `tiles` describes: `maxWidth` bounds whole-image
requests, not the image. When the finest level already fits in a single tile (a small image, or a
pyramid's own top level), there is nothing to trim: OpenSeadragon already requests `full/max` at
the true full size, so `sizes` is left untrimmed and the bound is the full size.

### The whole-image budget, and tiles-only degradation

Writing a whole-image file for every advertised size costs a JPEG encode per size, per tree, and
that is not free for a very large image: `sample_huge_level`'s 100000x100000 single-level fixture
would need a ten-gigapixel JPEG, which JPEG cannot even encode (65535px is its own maximum edge).
So the whole-image derivatives are bounded: `MAX_WHOLE_IMAGE_PIXELS = 64,000,000` (about
8000x8000) and no edge above `65,535` (`iiif::MAX_WHOLE_IMAGE_PIXELS`/`iiif::MAX_WHOLE_IMAGE_EDGE`,
defined in `crates/iiif/src/level0.rs`). When the largest size a tree would advertise exceeds
either bound, the tree degrades as a whole: `sizes` is left untrimmed, no `maxWidth`/`maxHeight`
is declared (a bound ziv does not honour is worse than none), no whole-image derivatives or
`full/max` are written, and the export warns, naming the image's own size and the budget, that
these trees serve tiles only. Tiles and the viewer are unaffected: only whole-image requests
(`full/max`, `full/{w},{h}`) degrade. A multi-view export that is over budget still writes
`manifest.json` pointing at the coarsest pyramid level as each canvas's body, provided that
level's own whole image is itself within budget; when even that is too large, no `manifest.json`
is written and the export warns instead of shipping one whose only body would 404.

### Refusing pyramids OpenSeadragon cannot tile

Trimming `sizes` is only safe when OpenSeadragon's own tile-region arithmetic, not just its level-
size reconstruction, agrees with what the export writes (spec D7, amended after the design's
review found cases the original rule missed: a factor-3 or factor-4 pyramid, and an anisotropic
pyramid whose width bottoms out, both fail this check even though a naive length test would have
accepted them). When neither the trimmed nor the untrimmed `sizes` array survives that check,
`export` refuses the image outright, before writing anything, rather than producing a tree whose
own viewer would request tiles it does not contain:

```
$ ziv export tests/fixtures/sample_unpinnable.ome.zarr /tmp/out
ziv: error: cannot export this image as a static IIIF level 0 tree: its pyramid's scale factors
[1, 3, 9] (3 levels) don't match the tile layout OpenSeadragon expects, so the exported viewer
would request tiles the export does not contain. View it with `ziv serve` instead, which is
unaffected.
```

`ziv serve` is level 2 and reads `sizes` dynamically on every request rather than trusting a
static pyramid, so a refused image is never unviewable, only not exportable as a static tree.
Factor-4-per-level OME-Zarr pyramids are refused by this rule and are not exotic. Such an image,
exported before this fix, produced a silently broken viewer, so refusing it now is an improvement,
not a new limitation.

### Verified against the official validator — and what that run did not cover

The level0 validator run in "Official validator" above (`Done (5 tests, 0 failures)`) is a real,
external check, but a narrow one, because the reference image was deliberately single-level:
`sizes` was untrimmed, and `maxWidth`/`maxHeight` equalled the full image size. Its five requests
were `full/max` (three times, from `format_jpg`/`id_basic`/`id_squares`), `info.json`, and
`full/full` (from `size_nofull`) — nothing else. That proves `full/max` resolves and decodes to
the correct pixels for an untrimmed, single-level tree, and that `info.json`'s shape passes the
validator's own `info_json` sub-checks, none of which names `maxWidth` or `maxHeight`.

What it did NOT cover: it never saw a TRIMMED `sizes` array (a single-level image has nothing to
trim), never saw a `maxWidth`/`maxHeight` below the full image's own size, never requested a
multi-tile grid URL (`{x,y,w,h}/{w,h}/0/default.jpg`), and never fetched a single
`full/{w},{h}` URL for a non-`max` size — the validator's own level0 request grammar simply never
constructs one. Do not read a level0 `"profile": "level0"` as a claim that arbitrary Image API
requests succeed: only `full/max`, the sizes actually listed, and the pre-enumerated tile grid
`tiles` describes. This run checked only the slice of that contract a single-level reference image
can exercise.

A stronger run would repeat this against a pyramided, multi-tile reference export, so `sizes` is
actually trimmed and `maxWidth`/`maxHeight` are genuinely below the full resolution, and would add
an explicit HTTP fetch of every size `sizes` advertises, decoding each response's real pixel
dimensions. `crates/exporter`'s own completeness test already does exactly that check on disk
(see "CI-runnable conformance" above); nothing outside ziv's own test suite currently confirms
those files are also correct when served over HTTP, because the validator's own request grammar
never asks for one.

## IIIF Presentation 3 manifest

`--planes` or `--labels` (either, on a plan with more than one view) also writes `manifest.json`;
see [`README.md`](../README.md#exporting-planes-and-label-overlays) and spec §6.2. Its ids are
absolute, and the manifest is valid Presentation 3, only when `--id` is an `http(s)` URL; without
one its ids are paths relative to the export root, which is not valid Presentation 3 (most IIIF
viewers will not load it), and the export prints a warning saying so at write time.

### Manual checks, run 2026-09-16

Both checks used the worked example from [`viewer.md`](viewer.md#static-exports):
`idr0047A-4496763`, exported twice with `ziv export ... --planes --labels`, once with no `--id`
(relative manifest) and once with `--id http://127.0.0.1:8911` (absolute manifest, matching where
it was then served).

**Presentation validator — RUN.** The public validator at presentation-validator.iiif.io fetches
manifests by URL and cannot reach one served on `127.0.0.1`, so this ran a local clone of
[`IIIF/presentation-validator`](https://github.com/IIIF/presentation-validator)
(`uv sync`, then `uv run iiif-validator serve --host 127.0.0.1 --port 8080`, per its README), with
the export served over HTTP from its own folder (`python3 -m http.server`-style, with
`Access-Control-Allow-Origin: *`, as `docs/viewer.md`'s worked example does):

```console
$ curl "http://127.0.0.1:8080/validate?url=http://127.0.0.1:8911/manifest.json&version=3.0"
```

| Manifest | Verdict | Errors |
|---|---|---|
| absolute `--id http://127.0.0.1:8911` | **`okay: 1`** | 0 |
| no `--id` (relative) | `okay: 0` | 126 |

The relative-manifest result is exactly the shape the export's own warning predicts (relative ids
are "not valid IIIF Presentation 3"): 76 of the 126 errors are a relative `id`
(`'<path>' does not match '^http.*$'`): the manifest's own id, plus each of the 25 planes' canvas,
page and annotation id (1 + 3×25 = 76). The remaining errors are the schema validator
cascading that failure up to the `Choice` and `Canvas` objects that contain a bad id (`'canvas/z0'
is not valid under any of the given schemas`, one pair per plane), plus one final `Resolve Error`.
The absolute-manifest result has zero errors and zero warnings; spot-checked by hand, the
manifest's `Choice` bodies and their `service` ids all resolved against files really present in
the export (the same property `views_export.rs` pins in CI, see
[`labels.md`](labels.md#verification)).

**Mirador — RUN, after working around a dependency-resolution problem in Mirador's own
repository.** A hosted Mirador on HTTPS cannot fetch an `http://127.0.0.1` manifest, so this needs
Mirador running locally, per spec: clone
[`ProjectMirador/mirador`](https://github.com/ProjectMirador/mirador) at a v3 release (`v3.4.3`,
its latest v3 tag), install its dependencies, then serve its dev bundle and open the exported
manifest in it.

`v3.4.3` has no committed lockfile, so `npm ci` cannot run; `npm install` completed (1,703
packages), but every transitive version resolved to whatever satisfies today's `^`-range rather
than what the 2021 release shipped with, and the dev build (`webpack serve --mode=development`;
`npm start` itself only adds `--open`, which does not matter here) then failed:
`react-rnd@10.5.3`'s bundle pulls in `react-draggable@4.7.1`, which ships its CommonJS entry as an
ES module (`build/cjs/cjs.mjs`) exporting only a default export. Webpack 4, what this Mirador
release is pinned to, cannot resolve the named exports (`Component`, `Children`, `cloneElement`,
...) that `react-rnd`'s compiled output imports from it, and the build failed with `Can't import
the named export '...' from non EcmaScript module`. Pinning `react-draggable` to `4.4.5` (the last
version before that `.mjs` build) and removing `react-rnd`'s own nested copy of it (a `node_modules`
edit local to this throwaway clone, not a change checked in anywhere) resolved it: webpack's
plain Node module resolution then found the one hoisted copy, which does ship the plain-CommonJS
`cjs.js` `react-rnd`'s code actually calls, and the dev build compiled cleanly.

With that in place, a page under Mirador's own `__tests__/integration/mirador/` (served by its
`webpack-dev-server`, `contentBase` unchanged) opened `Mirador.viewer({ windows: [{ manifestId:
'http://127.0.0.1:8911/manifest.json' }] })` against the absolute-`--id` export, served exactly as
in [`viewer.md`](viewer.md#static-exports)'s worked example (`--id http://127.0.0.1:8911`, then
served from that folder). Driven in a real, automated browser (Playwright), with both spec §11
questions checked directly rather than inferred:

- **Planes page as canvases.** Mirador's item counter read "1 of 25 • z 0" on open, and clicking
  "Next item" advanced to "2 of 25 • z 1": one canvas per exported plane, in order, each one
  correctly labelled `z {n}`.
- **The overlay appears as a layer choice.** Mirador's **Layers** sidebar panel listed two entries
  per canvas, `intensity` and `0 overlay`, each with its own visibility toggle and opacity slider:
  exactly the `Choice` body the manifest encodes for that plane. Toggling each one off in turn and
  screenshotting confirmed they are independently real, not decorative: intensity off, overlay on
  showed the full-frame distinct-colour-per-cell segmentation mask alone; overlay off, intensity on
  showed the four-channel composite alone with no mask visible. Both toggles held across the
  canvas change in the point above.

The browser console showed nothing beyond the two informational entries pre-existing in this
Mirador build regardless of manifest (a Material-UI `overlap="rectangle"` prop-type deprecation
warning, and a `favicon.ico` 404 from the test harness page, which does not declare one): nothing
came from ziv's manifest or the tiles it serves.

**Conclusion: Mirador 3.4.3 renders this export's manifest as intended, satisfying spec §11's
Mirador check on both points.** The fallback in spec §11 (one canvas per view instead of a
`Choice`, for a viewer that cannot show `Choice` layers) is not needed and was not implemented.
The dependency workaround above is specific to bringing up a `v3.4.3` dev build from a fresh clone
today; it is not a ziv change and does not affect what was actually verified. The same manifest,
unmodified, is what a person would point any working Mirador 3 installation (e.g. its published
`mirador` npm package, or a container image, which do not hit this build-from-source path) at.
