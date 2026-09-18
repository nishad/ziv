# Serving more than one image

`ziv serve` takes one or more images. One is served at the root exactly as it always was; two or
more are each mounted under `/i/{name}/`.

```sh
ziv serve ./nuclei.ome.zarr                       # unchanged: served at the root
ziv serve ./a.ome.zarr ./b.ome.zarr s3://bkt/c.zarr
```

```text
ziv serving on http://127.0.0.1:3000  (viewer: http://127.0.0.1:3000/viewer/)
  a  ->  http://127.0.0.1:3000/i/a/viewer/
  b  ->  http://127.0.0.1:3000/i/b/viewer/
  c  ->  http://127.0.0.1:3000/i/c/viewer/
```

## The URL space

Every image gets a complete, independent copy of what a single-image server offers:

| URL | |
|---|---|
| `/i/{name}/iiif/{proj}/{region}/{size}/{rotation}/{quality}.{format}` | tiles |
| `/i/{name}/iiif/{proj}/info.json` | the IIIF description |
| `/i/{name}/iiif/{proj}` | redirects to `info.json` (IIIF `baseUriRedirect`) |
| `/i/{name}/ziv/dimensions.json` | t/z/c extents, channels, labels |
| `/i/{name}/viewer/` | the built-in viewer, for that image |

Process-level, at the root:

| URL | |
|---|---|
| `/ziv/images.json` | the catalogue |
| `/viewer/` | the image at the root if there is exactly one, otherwise a picker |
| `/healthz` `/readyz` `/metrics` | unchanged |

**Names may contain unencoded slashes**, which is the reason for the `/i/` mount rather than
putting the image in the IIIF identifier the way other IIIF servers do. The conventional scheme
requires `%2F` for a nested name, and `%2F` does not survive many reverse proxies; Cantaloupe ships
a `slash_substitute` config key to work around exactly this. Real OME-Zarr layouts are deeply
nested, so ziv sidesteps the problem instead of inheriting it.

Interop is unaffected: a IIIF client is handed `info.json`'s `id` as an opaque base URI and never
parses it.

## The single-image root alias

When the catalogue holds exactly one image, that image is **also** served at the root under the
pre-multi-image paths: `/iiif/…`, `/ziv/dimensions.json`, `/viewer/`.

This is a compatibility promise rather than a convenience. Every URL a single-image deployment
publishes today keeps working, unchanged, forever. As soon as there is more than one image the root
paths return 404, because there is no honest answer to "which image is at the root".

## Names, and which ones are safe to cite

A name is an identity in the URL space. It is deliberately **not** a path, even when it looks like
one.

Names given on the command line are derived: the final path component with a trailing `.ome.zarr`
or `.zarr` removed, so `/srv/data/idr0062A/6001240.ome.zarr` becomes `6001240`. Two paths that
derive the same name are a startup error rather than a silent collision.

**A derived name is unstable by construction.** Moving the file changes the URL. Do not put one in
a paper, a IIIF manifest, or anything else that has to keep resolving. Stable, operator-chosen names
arrive with the config file (see "Not here yet" below). Until then, a citable deployment should
serve one image per process, where the root alias gives it a name-free URL.

Every name is validated: no empty segments, no `.` or `..`, no control characters, at most 512
bytes.

## The catalogue

```console
$ curl -s localhost:3000/ziv/images.json
{"listable":true,"images":[{"name":"a","href":"/i/a/"},{"name":"b","href":"/i/b/"}]}
```

`listable` is `false`, rather than `images` being empty, for a source that genuinely cannot
enumerate. No such source ships yet; the field exists because a lazy directory root cannot walk a
filesystem it was chosen to avoid walking, and reporting `[]` would make the viewer say "no images"
when it means "type a name".

`/ziv/images.json` sits **inside the auth gate**, unlike the viewer's static assets. Those are
inert; this enumerates what exists. With `ZIV_AUTH_*` configured, an unauthenticated viewer gets a
401 for it and falls back gracefully, which is the same path it already takes for
`/ziv/dimensions.json`.

Readiness reports the catalogue rather than a bare status:

```console
$ curl -s localhost:3000/readyz
{"ready":true,"images":2,"open":1}
```

Ready means "the catalogue is built", not "every image is open". Images named on the command line
do open eagerly, so a broken path fails at startup where you can still fix it, but that is a
property of the command line rather than of readiness.

## Resource limits, and one number you may need to change

**`ZIV_CHUNK_CACHE_BYTES` is now a process-wide budget shared by every image**, not a per-image one.
Before this, each remote image got its own 256 MB, so forty images meant a 10 GB ceiling with
nothing bounding the count. The cache key carries the image, so entries cannot collide, and LRU
runs across images so an idle one holds no reserved memory.

The consequence is that **the 256 MB default may now be too small**. It was sized to hold one
image's multi-channel working set for one plane. If several images are hot at once, raise it. Local
images are unaffected: they keep a disabled chunk cache, because local reads pay decompression
rather than latency and sit on the page cache underneath.

Two further bounds exist and are not yet configurable from the command line (they arrive with the
config file):

- **64 open images**, LRU. This bounds handles, not bytes: an open image holds zarrs array handles,
  which hold file descriptors locally and connection pools remotely.
- **4 concurrent opens.** Opening an image without `omero` metadata does a percentile read of its
  coarsest level, which was harmless when it ran once before binding and is not once it can run
  inside a request. This is deliberately separate from the render semaphore, so a burst of cold
  opens cannot starve images that are already warm.

Four metrics cover the new machinery: `ziv_images_open`, `ziv_image_opens_total`,
`ziv_image_open_seconds`, `ziv_image_evictions_total`.

## Auth

Auth is server-wide, as before. There is no per-image access control; a deployment that needs it
runs more than one ziv, or puts a policy proxy in front.

**HMAC-signed URLs now bind the image.** The signed message is
`{image}\n{proj}\n{region}\n{size}\n{exp}`. Without the image, a signature minted for a tile of one
image would verify against the identical tile coordinates of another, which is an authorization
bypass rather than a cache oddity. Signed URLs work at both shapes: `/i/{name}/iiif/…` signs under
that name, and the root alias signs under the internal name its single image is filed as.

Under auth, an image that exists but will not open reports 404 rather than 502, so an
unauthenticated caller cannot probe catalogue membership by status code.

## Not here yet

Deliberately deferred, in the order they are planned:

- **A config file** giving operator-chosen, stable, citable names, plus limits and viewer
  capability switches.
- **Directory roots**, both lazy and scanned at boot.
- **Caller-named remote stores**, behind an explicit opt-in and an allowlist.
- **`viewer.enabled` / `allow_url_entry`** capability switches.

**`ziv export` remains single-image.** A Level 0 export is one projection's pre-enumerated tile
tree; exporting a catalogue is a separate feature with its own questions.

## Verification

- `crates/server/src/name.rs`, `mount.rs`, `registry.rs` — name validation, path splitting, source
  resolution, open coalescing and bounds.
- `crates/zarr-core/tests/shared_chunk_cache.rs` — two images sharing one cache never see each
  other's planes, and local images consume none of the budget.
- `crates/server/tests/multi_image.rs` — the mount, per-image endpoints, the 404/502 matrix, the
  auth gate on the catalogue.
- `crates/server/src/routes.rs` and `auth.rs` — the tile-cache key and the HMAC message each
  separate two images, both negative-controlled: removing the image from either makes its test fail.
- `e2e/tests/catalogue.spec.ts` — the picker, and that a mounted viewer requests tiles from its
  own mount rather than the root's.
