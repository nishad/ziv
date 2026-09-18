use crate::{
    apply_quality, colorize, composite_over, encode_rgb8, parse_opacity, resize_nearest,
    resize_rgb8, rotate_rgb8, LabelPalette, TileError,
};
use iiif::{DynLabel, Format, ImageInfo, ProjectionId, Quality, Region, Size};
use ndarray::Array2;
use projection::{
    composite, default_projection, percentile_window, ChannelView, Lut, Projection, Rgb, ZSelector,
};
use zarr_core::{DType, LabelOpenFailure, ZarrImage};

/// Hard ceiling on any single requested output dimension (width or height), in pixels.
///
/// This matches the JPEG encoder's own limit (`crate::encode_jpeg_rgb8` rejects images with
/// either dimension over 65535 via its dimension guards) — anything larger would be bounced by
/// `encode_jpeg_rgb8` regardless. Note this alone is NOT sufficient to bound the work done:
/// two dimensions each individually <= 65535 (e.g. via `Size::Wh`) still multiply out to a
/// ~4.3-billion-pixel resize target, which is why `plan()` also rejects on total pixel count
/// via `MAX_OUTPUT_PIXELS` rather than silently clamping each axis and proceeding.
const MAX_OUTPUT_DIM: u64 = 65_535;

/// Hard ceiling on total requested output pixels (`out_w * out_h`).
///
/// Chosen generously above any real IIIF tile/thumbnail request (the pyramid's own tile size
/// is 512x512) while still ruling out multi-gigabyte resize-buffer allocations: 16384x16384
/// (268 megapixels) is already far larger than any sane single JPEG tile. Fits comfortably in
/// u64/u128 arithmetic used to check it.
const MAX_OUTPUT_PIXELS: u64 = 16_384 * 16_384;

/// Hard ceiling on total pixels actually READ from a single pyramid level for one channel.
///
/// This bounds the level-0 (or any level's) read window independently of `MAX_OUTPUT_PIXELS`,
/// which only bounds the final resampled OUTPUT. Without this, a `Region::Full` request on a
/// very large image combined with a `Size` close to the region's own extent (so the read-vs-
/// output downscale is ~1x) would pick a fine pyramid level and still read a huge window into
/// memory before resampling down to a small-but-not-tiny output. Same value as
/// `MAX_OUTPUT_PIXELS` since both bound "how many pixels do we materialize at once" and there's
/// no reason for the read budget to differ from the output budget.
const MAX_READ_PIXELS: u64 = MAX_OUTPUT_PIXELS;

/// `plan()`'s result: the pyramid level to read from, the read window in THAT level's
/// coordinate space `(x, y, w, h)`, and the resample output `(w, h)`.
type PlanResult = (usize, (u64, u64, u64, u64), (u32, u32));

/// `resolve_request()`'s result: a read window `(x, y, w, h)` in full-resolution coordinates,
/// paired with the resample output `(w, h)`. Not yet mapped onto any pyramid level.
type WindowAndOutput = ((u64, u64, u64, u64), (u32, u32));

/// Safe fallback display window `(lo, hi)` for a dtype, used when `ZarrTileEngine::new()` skips
/// the coarsest-level percentile auto-stretch read because it would exceed `MAX_READ_PIXELS` (see
/// that call site). Integer dtypes use their natural full-range; float dtypes have no inherent
/// range, so `(0.0, 1.0)` is used as a documented, conservative default (matching
/// `percentile_window`'s own all-NaN/empty-plane fallback) — a possibly-suboptimal initial
/// contrast is an acceptable tradeoff for never triggering an unbounded startup read.
fn natural_range(dtype: DType) -> (f64, f64) {
    match dtype {
        DType::U8 => (0.0, u8::MAX as f64),
        DType::U16 => (0.0, u16::MAX as f64),
        DType::U32 => (0.0, u32::MAX as f64),
        DType::U64 => (0.0, u64::MAX as f64),
        DType::I8 => (i8::MIN as f64, i8::MAX as f64),
        DType::I16 => (i16::MIN as f64, i16::MAX as f64),
        DType::I32 => (i32::MIN as f64, i32::MAX as f64),
        DType::I64 => (i64::MIN as f64, i64::MAX as f64),
        DType::F32 | DType::F64 => (0.0, 1.0),
    }
}

/// Minimal seam every serving path (HTTP server, static exporter, cache wrapper) depends on
/// instead of the concrete `ZarrTileEngine`. Construction/internal methods (`new`, `plan`,
/// `pick_level`) stay inherent on `ZarrTileEngine` — this trait only covers what a caller needs
/// to actually serve a tile or describe the image.
/// One channel as the built-in viewer needs to present it: enough to draw a labelled swatch and
/// decide whether it starts on.
///
/// This describes the channels the server can actually RENDER, which is the resolved projection's
/// channel catalogue — not simply `0..size_c`. With no `omero` metadata the catalogue is capped
/// at the first three channels (see `projection::default_projection`), and asking for a channel
/// outside it yields an image with nothing composited. Offering only what can be rendered is the
/// difference between a control that works and one that silently produces black.
#[derive(Debug, Clone, PartialEq)]
pub struct ChannelDescriptor {
    pub index: usize,
    /// `omero`'s `label` for this channel, when the image supplies one.
    pub label: Option<String>,
    /// Six-digit sRGB hex, no leading `#` — the colour this channel composites with.
    pub color: String,
    pub window: (f64, f64),
    /// Whether this channel is on in the default projection.
    pub active: bool,
}

/// One label image as the built-in viewer needs to present it.
///
/// `declared_colors` is how many values the image supplies a colour for in `image-label`. The
/// viewer uses it to decide whether offering the `table` palette makes any sense: a label with an
/// empty table renders entirely transparent under `table`, and an option that produces a blank
/// screen is worse than no option.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LabelDescriptor {
    /// The name under `labels/`, which is also what goes in a `@label=` identifier.
    pub name: String,
    pub declared_colors: usize,
}

/// The extent of the non-spatial axes, plus the channel catalogue.
///
/// `info.json` deliberately does not carry this: it is a IIIF Image API document describing a 2D
/// image, and ziv advertises `level2` conformance, so bolting private fields onto it would put a
/// hard-won conformance claim at the mercy of a viewer feature. This travels on its own endpoint
/// instead.
#[derive(Debug, Clone, PartialEq)]
pub struct ImageDimensions {
    pub size_t: u64,
    pub size_z: u64,
    pub size_c: u64,
    /// The plane the `default` projection renders, so the viewer's controls can open exactly
    /// where the image the user is already looking at sits, rather than jumping on first input.
    pub default_t: u64,
    pub default_z: u64,
    pub channels: Vec<ChannelDescriptor>,
    /// The label images this image carries, in the order it declares them. Empty for most images.
    pub labels: Vec<LabelDescriptor>,
    /// Labels `labels/.zattrs` declared that could not be opened, each with the reason. Empty in
    /// the normal case.
    ///
    /// Without this, a declared-but-unopenable label was indistinguishable from no label having
    /// been declared at all: both left `labels` empty. That made the exporter's `--labels adds
    /// nothing: the image has no label images` warning a lie whenever a label genuinely existed
    /// but failed to open (see `zarr_core::ZarrImage::label_open_failures`), and gave an operator
    /// no way to learn WHY a label they know exists never shows up.
    pub label_open_failures: Vec<LabelOpenFailure>,
}

impl ImageDimensions {
    /// The JSON document the built-in viewer builds its controls from: `/ziv/dimensions.json` on a
    /// server, `ziv/dimensions.json` in a static export.
    ///
    /// One function for both, so the file an export writes and the response a server sends cannot
    /// drift apart. Moved here from the server's route handler for exactly that reason.
    ///
    /// `labelOpenFailures` is data for API consumers, not the viewer: it is a new field the
    /// current viewer never reads (unknown fields are ignored, same as any other JSON consumer
    /// that only looks up the keys it knows), added so a client hitting this endpoint directly
    /// sees the same "N declared, none opened" honesty the exporter's `--labels` warning already
    /// has, rather than an empty `labels` array indistinguishable from "none declared".
    pub fn to_json(&self) -> serde_json::Value {
        serde_json::json!({
            "sizeT": self.size_t,
            "sizeZ": self.size_z,
            "sizeC": self.size_c,
            "defaultT": self.default_t,
            "defaultZ": self.default_z,
            "channels": self.channels.iter().map(|c| serde_json::json!({
                "index": c.index,
                "label": c.label,
                "color": c.color,
                "window": { "start": c.window.0, "end": c.window.1 },
                "active": c.active,
            })).collect::<Vec<_>>(),
            "labels": self.labels.iter().map(|l| serde_json::json!({
                "name": l.name,
                "declaredColors": l.declared_colors,
            })).collect::<Vec<_>>(),
            "labelOpenFailures": self.label_open_failures.iter().map(|f| serde_json::json!({
                "name": f.name,
                "reason": f.reason,
            })).collect::<Vec<_>>(),
        })
    }
}

/// Everything a single IIIF image request asks of the renderer, in the spec's own application
/// order: region, size, rotation, quality, format.
///
/// Carried as one struct rather than five positional arguments so that adding a parameter is a
/// compile error at the construction sites that must supply it, not a silently-defaulted value.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct RenderSpec {
    pub region: Region,
    pub size: Size,
    pub rotation: u16,
    pub quality: Quality,
    pub format: Format,
    /// JPEG encoder quality (0-100). Ignored for lossless formats.
    pub jpeg_quality: u8,
}

pub trait TileEngine {
    /// Build the IIIF `info.json` model for this image, rooted at `id_base`.
    ///
    /// `id_base` sets only [`ImageInfo::id`]. Every geometry field (`width`, `height`,
    /// `tile_size`, `scale_factors`, `sizes`) must be the same whatever `id_base` is: a static
    /// export calls this once per tree with a different id, and separately once for the manifest,
    /// and relies on all of them describing the same pyramid. If an implementation let geometry
    /// vary by id, a manifest body could name a whole image that tree's writer never produced.
    fn image_info(&self, id_base: &str) -> ImageInfo;
    /// Extent of the t/z/c axes and the renderable channel catalogue, for the built-in viewer's
    /// controls. See [`ImageDimensions`].
    fn dimensions(&self) -> ImageDimensions;
    /// Render one image response for the given projection and request, as encoded bytes in
    /// `spec.format`.
    fn render(&self, id: &ProjectionId, spec: &RenderSpec) -> Result<Vec<u8>, TileError>;
    /// Convenience wrapper for the common case: unrotated, `default` quality, JPEG. Used by the
    /// static exporter and the DZI writer, both of which only ever emit plain JPEG tiles.
    fn tile(
        &self,
        id: &ProjectionId,
        region: Region,
        size: Size,
        jpeg_quality: u8,
    ) -> Result<Vec<u8>, TileError> {
        self.render(
            id,
            &RenderSpec {
                region,
                size,
                rotation: 0,
                quality: Quality::Default,
                format: Format::Jpg,
                jpeg_quality,
            },
        )
    }
}

pub struct ZarrTileEngine {
    image: ZarrImage,
    base_projection: Projection,
    tile_size: u64,
    scale_factors: Vec<u64>,
    /// One `scale_factors` per label image, positionally matching `image.labels()`.
    ///
    /// Kept separately rather than derived per request because a label's pyramid is its own: it
    /// need not have the same number of levels as the parent, nor the same factors between them.
    label_scale_factors: Vec<Vec<u64>>,
}

/// The pyramid's downsample factor per level, as `level0_width / levelN_width`, rounded.
fn scale_factors_of(image: &ZarrImage) -> Vec<u64> {
    let (_, full_x) = image.full_yx();
    (0..image.num_levels())
        .map(|lvl| {
            let (_, lx) = image.level_yx(lvl);
            ((full_x as f64 / lx as f64).round() as u64).max(1)
        })
        .collect()
}

impl ZarrTileEngine {
    pub fn new(image: ZarrImage) -> Self {
        let mut base = default_projection(image.size_c(), image.size_z(), image.omero());

        // If windows are the placeholder and there's no omero, auto-stretch from the coarsest
        // level by reading it in full and computing a percentile window.
        //
        // Guard: this is a synchronous read that happens at construction time, i.e. BEFORE the
        // server binds and starts serving — for a remote-backed image (P2) with no `omero`
        // metadata, an unbounded read here is a startup DoS/OOM vector (a large remote image
        // triggers a full-level network fetch just to open). `new()` must NEVER trigger an
        // unbounded read, so the coarsest level's pixel count is checked against
        // `MAX_READ_PIXELS` first; if it's over the cap, the read is skipped entirely and each
        // channel falls back to `natural_range(image.dtype())` — a possibly-less-ideal default
        // contrast, which is a fine tradeoff for never hanging/OOMing at startup.
        let needs_stretch = image.omero().is_none();
        if needs_stretch {
            let coarsest = image.num_levels() - 1;
            let (cy, cx) = image.level_yx(coarsest);
            let coarsest_pixels = cy as u128 * cx as u128;
            if coarsest_pixels > MAX_READ_PIXELS as u128 {
                let fallback = natural_range(image.dtype());
                for ch in base.channels.iter_mut() {
                    ch.window = fallback;
                }
            } else {
                let z = match base.z {
                    ZSelector::Plane(z) => z.min(image.size_z().saturating_sub(1)),
                };
                for ch in base.channels.iter_mut() {
                    if let Ok(plane) =
                        image.read_region_f64(coarsest, base.t, ch.index as u64, z, 0..cy, 0..cx)
                    {
                        ch.window = percentile_window(&plane, 2.0, 98.0);
                    }
                }
            }
        }

        let scale_factors = scale_factors_of(&image);
        let label_scale_factors = image
            .labels()
            .iter()
            .map(|l| scale_factors_of(&l.image))
            .collect();

        ZarrTileEngine {
            image,
            base_projection: base,
            tile_size: 512,
            scale_factors,
            label_scale_factors,
        }
    }

    fn resolve(&self, id: &ProjectionId) -> Projection {
        match id {
            ProjectionId::Default | ProjectionId::Named(_) => self.base_projection.clone(),
            ProjectionId::Dynamic(d) => {
                let mut p = self.base_projection.clone();
                if let Some(z) = d.z {
                    p.z = ZSelector::Plane(z);
                }
                if let Some(t) = d.t {
                    p.t = t;
                }
                if !d.channels.is_empty() {
                    // enable only the listed channels; apply window overrides
                    for ch in p.channels.iter_mut() {
                        ch.enabled = false;
                    }
                    for dc in &d.channels {
                        if let Some(cv) = p.channels.iter_mut().find(|c| c.index == dc.index) {
                            cv.enabled = true;
                            if let Some(w) = dc.window {
                                cv.window = w;
                            }
                        }
                    }
                }
                p
            }
        }
    }

    /// Resolve one request against THIS image's own extent and pyramid. See [`resolve_request`]
    /// and [`map_into_level`], which do the two halves of the job and which the label path
    /// recombines with a coordinate remap in between.
    fn plan(&self, region: Region, size: Size) -> Result<PlanResult, TileError> {
        let (full_y, full_x) = self.image.full_yx();
        let (window, out) = resolve_request(full_y, full_x, region, size)?;
        map_into_level(&self.image, &self.scale_factors, window, out)
    }

    /// Resolves `region`/`size` against this image's full extent into the output dimensions a
    /// render would actually produce, without reading any pixels, compositing or encoding.
    ///
    /// This is exactly the geometry half of [`Self::plan`] (and of [`Self::label_rgba`], which
    /// resolves against the same parent extent before rescaling into the label's own — see that
    /// method's doc comment on why an overlay's two halves must agree), exposed on its own so a
    /// caller can weigh the cost of a render BEFORE paying for the read+composite+encode a real
    /// one does. `ziv render` is the motivating caller: a `full`/`max` request against a very
    /// large image must be refused up front, the same way a static export's level0 whole-image
    /// budget refuses one, rather than after allocating a multi-hundred-megabyte buffer.
    pub fn output_size(&self, region: Region, size: Size) -> Result<(u32, u32), TileError> {
        let (full_y, full_x) = self.image.full_yx();
        let (_, out) = resolve_request(full_y, full_x, region, size)?;
        Ok(out)
    }

    /// Render the intensity channels: read, composite, resample. Returns packed RGB8 and its
    /// dimensions, before rotation/quality/format are applied.
    fn render_image_rgb(
        &self,
        proj: &Projection,
        region: Region,
        size: Size,
    ) -> Result<(Vec<u8>, u32, u32), TileError> {
        let (level, (rx, ry, rw, rh), (out_w, out_h)) = self.plan(region, size)?;
        let ZSelector::Plane(z) = proj.z;

        // Read each enabled channel's YX plane from the level chosen by `plan()` (the finest
        // level whose native resolution is still >= the requested output resolution for this
        // region), using the region already mapped into that level's coordinate space. This is
        // what prevents a full-res (level 0) read for a heavily downscaled request.
        // One read for every enabled channel, issued together. Sequentially, a tile cost the SUM
        // of each channel's latency — fine locally, but against a remote store a seven-channel
        // selection could not finish inside the server's request timeout. See
        // `ZarrImage::read_regions_f64` for why the concurrency is async rather than threaded.
        let enabled: Vec<&ChannelView> = proj.channels.iter().filter(|c| c.enabled).collect();
        let indices: Vec<u64> = enabled.iter().map(|c| c.index as u64).collect();
        let read =
            self.image
                .read_regions_f64(level, proj.t, &indices, z, ry..ry + rh, rx..rx + rw)?;
        let planes: Vec<(ChannelView, Array2<f64>)> =
            enabled.into_iter().cloned().zip(read).collect();
        let refs: Vec<(&ChannelView, Array2<f64>)> =
            planes.iter().map(|(c, p)| (c, p.clone())).collect();
        let rgb = composite(&refs);
        let resized = resize_rgb8(&rgb, rw as u32, rh as u32, out_w, out_h)?;
        Ok((resized, out_w, out_h))
    }

    /// Read, resample and colour one label image into packed RGBA8.
    ///
    /// Three things make this NOT just `render_image_rgb` with a different array:
    ///
    /// 1. **The pyramid is the label's own.** The request's coordinates are in the parent's space
    ///    (that is what `info.json` advertises), so the window is resolved against the parent and
    ///    then rescaled into the label's — which is a no-op in the usual case where the two share
    ///    a full-resolution extent, and is the whole ballgame when they do not.
    /// 2. **The resample is nearest-neighbour.** Label values are object identifiers; see
    ///    [`crate::resample::resize_nearest`].
    /// 3. **Colour is a table lookup, not a window stretch.** See [`crate::labels`].
    ///
    /// RGBA rather than RGB because the alpha is the whole point downstream: the same buffer is
    /// composited over black for `label=` and over the intensity render for `overlay=`, so the two
    /// views cannot disagree about colour.
    ///
    /// The output size is computed from the PARENT's extent, exactly as `render_image_rgb` does,
    /// which is what guarantees an overlay's two halves are the same shape.
    fn label_rgba(
        &self,
        sel: &DynLabel,
        proj: &Projection,
        region: Region,
        size: Size,
    ) -> Result<(Vec<u8>, u32, u32), TileError> {
        let idx = self
            .image
            .labels()
            .iter()
            .position(|l| l.info.name == sel.name)
            .ok_or_else(|| TileError::UnknownLabel(sel.name.clone()))?;
        let layer = &self.image.labels()[idx];
        let palette = LabelPalette::parse(sel.palette.as_deref())?;
        let opacity = parse_opacity(sel.opacity)?;

        let (full_y, full_x) = self.image.full_yx();
        let (window, out) = resolve_request(full_y, full_x, region, size)?;
        let window = rescale_window(window, (full_y, full_x), layer.image.full_yx());
        let (level, (rx, ry, rw, rh), (out_w, out_h)) =
            map_into_level(&layer.image, &self.label_scale_factors[idx], window, out)?;

        // A label image need not span the same non-spatial axes as the image it annotates — a
        // single-plane mask over a 236-plane stack is ordinary. Clamping rather than erroring
        // means the z/t controls keep working over such a mask instead of 404ing halfway up the
        // slider; the mask simply does not vary along that axis, which is the truth about it.
        let ZSelector::Plane(z) = proj.z;
        let t = proj.t.min(layer.image.size_t().saturating_sub(1));
        let z = z.min(layer.image.size_z().saturating_sub(1));

        let values = layer
            .image
            .read_region_f64(level, t, 0, z, ry..ry + rh, rx..rx + rw)?;
        let resampled = resize_nearest(&values, out_w, out_h);
        Ok((
            colorize(&resampled, &layer.info, palette, opacity),
            out_w,
            out_h,
        ))
    }

    /// `label=NAME` — the mask alone: coloured objects on black.
    fn render_label_rgb(
        &self,
        sel: &DynLabel,
        proj: &Projection,
        region: Region,
        size: Size,
    ) -> Result<(Vec<u8>, u32, u32), TileError> {
        let (rgba, out_w, out_h) = self.label_rgba(sel, proj, region, size)?;
        let mut rgb = vec![0u8; out_w as usize * out_h as usize * 3];
        composite_over(&mut rgb, &rgba);
        Ok((rgb, out_w, out_h))
    }

    /// `overlay=NAME` — the mask composited over the intensity render.
    ///
    /// This is the view the feature exists for: a segmentation is only interesting next to the
    /// pixels it segments. The channel selection still applies, because the base really is the
    /// ordinary image render.
    ///
    /// The label is read at ITS best level for this request and the image at THEIRS, so the two
    /// halves can come from pyramids of different depths; they meet at the output size, which both
    /// derive from the parent's extent.
    fn render_overlay_rgb(
        &self,
        sel: &DynLabel,
        proj: &Projection,
        region: Region,
        size: Size,
    ) -> Result<(Vec<u8>, u32, u32), TileError> {
        let (mut base, out_w, out_h) = self.render_image_rgb(proj, region, size)?;
        let (rgba, label_w, label_h) = self.label_rgba(sel, proj, region, size)?;
        // Both sides size their output from the parent's extent via `resolve_request`, so this can
        // only fail if that stops being true. Say so loudly rather than composite a shifted mask.
        if (label_w, label_h) != (out_w, out_h) {
            return Err(TileError::Encode(format!(
                "overlay size mismatch: image {out_w}x{out_h}, label {label_w}x{label_h}"
            )));
        }
        composite_over(&mut base, &rgba);
        Ok((base, out_w, out_h))
    }
}

/// Rescale a read window from one image's full-resolution coordinate space into another's.
///
/// Used to carry an IIIF request, whose coordinates are always in the parent image's space,
/// into a label image whose own level 0 may be a different size. Identity when the two extents
/// match, which is the common case and must stay exact rather than merely close.
fn rescale_window(
    window: (u64, u64, u64, u64),
    from: (u64, u64),
    to: (u64, u64),
) -> (u64, u64, u64, u64) {
    if from == to {
        return window;
    }
    let (rx, ry, rw, rh) = window;
    let (from_y, from_x) = from;
    let (to_y, to_x) = to;
    // u128 throughout: both extents and the window are u64 and their product is not.
    let origin = |v: u64, from: u64, to: u64| -> u64 {
        if from == 0 {
            return 0;
        }
        ((v as u128 * to as u128) / from as u128) as u64
    };
    // Round the extent UP so a window that lands between label pixels still covers what was
    // asked for; a mask clipped short would read as a missing object.
    let extent = |v: u64, from: u64, to: u64| -> u64 {
        if from == 0 {
            return 1;
        }
        ((v as u128 * to as u128).div_ceil(from as u128) as u64).max(1)
    };
    let nx = origin(rx, from_x, to_x).min(to_x.saturating_sub(1));
    let ny = origin(ry, from_y, to_y).min(to_y.saturating_sub(1));
    let nw = extent(rw, from_x, to_x).min(to_x.saturating_sub(nx)).max(1);
    let nh = extent(rh, from_y, to_y).min(to_y.saturating_sub(ny)).max(1);
    (nx, ny, nw, nh)
}

/// Pick the pyramid level to read from, given how much the request downscales the region
/// (`region_extent / output_extent`, e.g. reading a 4096px-wide region down to a 512px
/// output is `read_downscale = 8.0`).
///
/// We want the FINEST (largest-resolution, smallest index) level whose own downsample
/// factor is still `<= read_downscale`, i.e. the largest level index `L` such that
/// `scale_factors[L] <= read_downscale`. Reading from that level and then resampling the
/// (already-small) result the rest of the way down to the exact output size:
/// - avoids ever reading full-res (level 0) data for a heavily downscaled request, which is
///   the OOM this fix exists to prevent, and
/// - never picks a COARSER level than necessary, i.e. never upsamples from a level whose
///   native resolution is already below the requested output (which would lose quality).
///
/// `scale_factors` is populated from level 0 down to the coarsest level and is non-decreasing
/// (level 0 is always 1), so the "largest index with factor <= target" search is a simple linear
/// scan keeping the last level that still qualifies. Falls back to level 0 if no level's factor
/// qualifies (i.e. `read_downscale < scale_factors[1]`, meaning the output is close to full-res
/// of the region and level 0 is genuinely the right choice).
///
/// Free-standing rather than a method because a label image has its OWN pyramid, which need not
/// match its parent's — the IDR sample this was built against has three levels for the image and
/// four for its labels, so a level index chosen for one is meaningless for the other.
fn pick_level_in(scale_factors: &[u64], read_downscale: f64) -> usize {
    let mut best = 0usize;
    for (lvl, &factor) in scale_factors.iter().enumerate() {
        if factor as f64 <= read_downscale {
            best = lvl;
        }
    }
    best
}

/// Resolve an IIIF `region` + `size` pair against a full-resolution extent into a concrete
/// read window `(x, y, w, h)` and output size `(w, h)`, both in FULL-RESOLUTION coordinates.
///
/// This is the half of planning that is purely about the request, with no pyramid involved, so
/// the label path can run it against the PARENT's extent (which is what `info.json` advertises,
/// and therefore what the client's coordinates mean) before remapping into the label's own
/// coordinate space.
///
/// Bounds safety: the region is CLAMPED (never panics) to the image's full extent so an
/// over-large or offset `x,y,w,h` from an untrusted IIIF request can never produce a read
/// window that extends past the array. `x`/`y` are first clamped into `[0, full-1]` (an
/// out-of-bounds origin is pulled back to the last valid pixel, guaranteeing room for at
/// least a 1px window), then `w`/`h` are clamped so `x + w <= full_x` and `y + h <= full_y`
/// using saturating arithmetic, and finally widened to at least 1px. Because the origin is
/// already guaranteed to leave >=1px of room, this widening can never push the window past
/// the image bounds — unlike clamping the origin to `full_x` itself, which would leave zero
/// room and force an out-of-bounds widen.
///
/// Output-size safety: `size` is parsed directly from the untrusted IIIF size path segment
/// (`iiif::parse_size`) with no upper bound — `Size::Width`/`Size::Height` carry raw `u64`s
/// and `Size::Pct` a raw `f64`, any of which can be up to their type's max. Computing the
/// aspect-ratio-preserving dimension via `u64` multiplication (`rh * w`, `rw * h`) would
/// overflow (panics in debug, wraps in release) for large inputs. To avoid that, every arm
/// below does the multiply widened to `u128` (which cannot overflow for any `u64`/`u32`
/// inputs). The result is then validated (not silently clamped): if either output dimension
/// exceeds `MAX_OUTPUT_DIM`, or the total pixel count exceeds `MAX_OUTPUT_PIXELS`, this
/// returns `TileError::OutOfRange` rather than proceeding. Clamping alone is NOT enough
/// here — e.g. `Size::Wh(65535, 65535)` has each axis individually within a 65535 clamp but
/// the pair together would demand a ~4.3-billion-pixel resize buffer, so both the per-axis
/// and the total-pixel check are required to keep this side panic-free *and* bounded-cost.
fn resolve_request(
    full_y: u64,
    full_x: u64,
    region: Region,
    size: Size,
) -> Result<WindowAndOutput, TileError> {
    // Clamp an origin coordinate so at least 1px of room remains before the edge.
    let clamp_origin = |v: u64, full: u64| v.min(full.saturating_sub(1));
    let (rx, ry, rw, rh) = match region {
        Region::Full => (0, 0, full_x, full_y),
        Region::Square => {
            let s = full_x.min(full_y);
            ((full_x - s) / 2, (full_y - s) / 2, s, s)
        }
        Region::Px { x, y, w, h } => {
            let cx = clamp_origin(x, full_x);
            let cy = clamp_origin(y, full_y);
            let cw = w.min(full_x - cx);
            let ch = h.min(full_y - cy);
            (cx, cy, cw, ch)
        }
        Region::Pct { x, y, w, h } => {
            let cx = clamp_origin(((x.max(0.0) / 100.0) * full_x as f64) as u64, full_x);
            let cy = clamp_origin(((y.max(0.0) / 100.0) * full_y as f64) as u64, full_y);
            let raw_w = ((w.max(0.0) / 100.0) * full_x as f64) as u64;
            let raw_h = ((h.max(0.0) / 100.0) * full_y as f64) as u64;
            let cw = raw_w.min(full_x - cx);
            let chh = raw_h.min(full_y - cy);
            (cx, cy, cw, chh)
        }
    };
    // Widen a degenerate (zero-area) window to 1px; the origin clamp above guarantees
    // there is always room for this without crossing the image boundary.
    let rw = rw.max(1).min(full_x - rx);
    let rh = rh.max(1).min(full_y - ry);
    let (out_w, out_h): (u64, u64) = match size {
        Size::Max => (rw, rh),
        Size::Width(w) => {
            let h = (rh as u128 * w as u128 / rw.max(1) as u128) as u64;
            (w, h)
        }
        Size::Height(h) => {
            let w = (rw as u128 * h as u128 / rh.max(1) as u128) as u64;
            (w, h)
        }
        Size::Wh(w, h) => (w, h),
        // `!w,h` (sizeByConfinedWh): the largest size fitting INSIDE the w x h box with the
        // region's aspect ratio preserved — i.e. scale by whichever axis is more constraining.
        // u128 throughout because w/h arrive unbounded from the untrusted request; the
        // MAX_OUTPUT_DIM / MAX_OUTPUT_PIXELS checks below still bound the result.
        Size::Confined(w, h) => {
            if w == 0 || h == 0 {
                (0, 0)
            } else {
                let by_w = (rh as u128 * w as u128) / rw.max(1) as u128;
                if by_w <= h as u128 {
                    (w, by_w.min(u64::MAX as u128) as u64)
                } else {
                    let by_h = (rw as u128 * h as u128) / rh.max(1) as u128;
                    (by_h.min(u64::MAX as u128) as u64, h)
                }
            }
        }
        Size::Pct(p) => {
            // `p` is an untrusted, unbounded f64; clamp non-finite/negative to 0 before use
            // so the multiply below can never produce NaN/negative-cast garbage.
            let p = if p.is_finite() { p.max(0.0) } else { 0.0 };
            let w = (rw as f64 * p / 100.0) as u64;
            let h = (rh as f64 * p / 100.0) as u64;
            (w, h)
        }
    };
    let out_w = out_w.max(1);
    let out_h = out_h.max(1);
    // No upscaling. IIIF 3.0 allows an output larger than the extracted region ONLY via the
    // `^` (sizeUpscaling) syntax, which is an optional feature ziv does not implement — so a
    // plain size bigger than the region must be refused, not silently blown up. `rw`/`rh` are
    // still in full-resolution image coordinates here (the level mapping happens below), so
    // this compares like with like. Refusing is also what keeps the resample a downscale,
    // which is the only direction the pyramid can serve without inventing detail.
    if out_w > rw || out_h > rh {
        return Err(TileError::OutOfRange(format!(
            "requested output {out_w}x{out_h} is larger than the {rw}x{rh} region; \
             upscaling is not supported"
        )));
    }
    if out_w > MAX_OUTPUT_DIM || out_h > MAX_OUTPUT_DIM {
        return Err(TileError::OutOfRange(format!(
            "requested output {out_w}x{out_h} exceeds max dimension {MAX_OUTPUT_DIM}"
        )));
    }
    // Both factors are already <= MAX_OUTPUT_DIM (65535) here, so this multiply fits in u64
    // (max ~4.3e9) with room to spare, but widen to u128 anyway to make that non-overflow
    // property obvious without relying on the reader recalling the dimension cap above.
    let out_pixels = out_w as u128 * out_h as u128;
    if out_pixels > MAX_OUTPUT_PIXELS as u128 {
        return Err(TileError::OutOfRange(format!(
            "requested output {out_w}x{out_h} ({out_pixels} px) exceeds max {MAX_OUTPUT_PIXELS} px"
        )));
    }

    Ok(((rx, ry, rw, rh), (out_w as u32, out_h as u32)))
}

/// Map an already-resolved read window into the coordinate space of the best-matching level of
/// `image`'s pyramid, and bound the resulting read.
///
/// `window` must be in `image`'s OWN full-resolution coordinates. For the intensity image that is
/// simply what [`resolve_request`] produced; for a label image, whose pyramid is independent of
/// its parent's, the caller rescales the window first (see `plan_label`).
fn map_into_level(
    image: &ZarrImage,
    scale_factors: &[u64],
    window: (u64, u64, u64, u64),
    out: (u32, u32),
) -> Result<PlanResult, TileError> {
    let (rx, ry, rw, rh) = window;
    let (out_w, out_h) = out;
    // Pick the pyramid level whose native resolution best matches how much this request
    // downscales the region, then map the region (currently in full-res/level-0 coords)
    // into that level's coordinate space. This is the crux of the OOM fix: without it, the
    // read below always hits level 0 (full res) regardless of how small `out_w`/`out_h` are.
    let read_downscale_x = rw as f64 / out_w as f64;
    let read_downscale_y = rh as f64 / out_h as f64;
    // Use the smaller (less-downscaled) axis so neither axis ends up upsampled from a level
    // coarser than what it needs.
    let read_downscale = read_downscale_x.min(read_downscale_y);
    let level = pick_level_in(scale_factors, read_downscale);
    let factor = scale_factors[level].max(1);

    let (level_y, level_x) = image.level_yx(level);

    // Map the full-res region into level `level`'s coordinate space by dividing by the
    // level's scale factor (integer division). Clamp into `[0, level_extent - 1]` for the
    // origin (mirroring the full-res clamp above) so there's always room for a >=1px window,
    // then clamp the mapped width/height so the window never reads past the level's shape.
    let clamp_level_origin = |v: u64, full: u64| v.min(full.saturating_sub(1));
    let lx = clamp_level_origin(rx / factor, level_x);
    let ly = clamp_level_origin(ry / factor, level_y);
    // Divide width/height too; round up (`div_ceil`) so a small full-res window doesn't
    // collapse to a 0px window at a coarse level, then widen to >=1px and clamp to the
    // level's remaining extent, same reasoning as the full-res widen above.
    let lw = (rw.div_ceil(factor)).max(1).min(level_x - lx);
    let lh = (rh.div_ceil(factor)).max(1).min(level_y - ly);

    // Bound the READ window itself: even after picking a coarser level, guard against an
    // unbounded read (e.g. a pathological/huge level or a region that maps to a huge window
    // at the chosen level). This is independent of the OUTPUT pixel cap above.
    let read_pixels = lw as u128 * lh as u128;
    if read_pixels > MAX_READ_PIXELS as u128 {
        return Err(TileError::OutOfRange(format!(
            "read window {lw}x{lh} ({read_pixels} px) at level {level} exceeds max {MAX_READ_PIXELS} px"
        )));
    }

    Ok((level, (lx, ly, lw, lh), (out_w, out_h)))
}

impl TileEngine for ZarrTileEngine {
    fn image_info(&self, id_base: &str) -> ImageInfo {
        let (y, x) = self.image.full_yx();
        // sizes: full image + each pyramid level's whole-image size
        let mut sizes = Vec::new();
        for lvl in 0..self.image.num_levels() {
            let (ly, lx) = self.image.level_yx(lvl);
            sizes.push((lx, ly));
        }
        ImageInfo {
            id: id_base.to_string(),
            width: x,
            height: y,
            tile_size: self.tile_size,
            scale_factors: self.scale_factors.clone(),
            sizes,
        }
    }

    fn dimensions(&self) -> ImageDimensions {
        // `omero` supplies human labels; the resolved projection supplies the colour and window
        // actually used to composite. Reading the label from metadata but the colour from the
        // projection keeps the swatch honest: it shows what the pixel will be tinted with, even
        // when the image carried no colour and a fallback LUT was assigned.
        let labels: Vec<Option<String>> = self
            .image
            .omero()
            .and_then(|o| o.get("channels").cloned())
            .and_then(|c| c.as_array().cloned())
            .map(|arr| {
                arr.iter()
                    .map(|ch| ch.get("label").and_then(|v| v.as_str()).map(str::to_string))
                    .collect()
            })
            .unwrap_or_default();

        let channels = self
            .base_projection
            .channels
            .iter()
            .map(|ch| ChannelDescriptor {
                index: ch.index,
                label: labels.get(ch.index).cloned().flatten(),
                color: match ch.lut {
                    Lut::Fixed(Rgb(r, g, b)) => format!("{r:02X}{g:02X}{b:02X}"),
                    // A greyscale channel composites at full white; the swatch says so.
                    Lut::Grey => "FFFFFF".to_string(),
                },
                window: ch.window,
                active: ch.enabled,
            })
            .collect();

        let ZSelector::Plane(default_z) = self.base_projection.z;
        ImageDimensions {
            size_t: self.image.size_t(),
            size_z: self.image.size_z(),
            size_c: self.image.size_c(),
            default_t: self.base_projection.t,
            default_z,
            channels,
            labels: self
                .image
                .labels()
                .iter()
                .map(|l| LabelDescriptor {
                    name: l.info.name.clone(),
                    declared_colors: l.info.colors.len(),
                })
                .collect(),
            label_open_failures: self.image.label_open_failures().to_vec(),
        }
    }

    fn render(&self, id: &ProjectionId, spec: &RenderSpec) -> Result<Vec<u8>, TileError> {
        let RenderSpec {
            region,
            size,
            rotation,
            quality,
            format,
            jpeg_quality,
        } = *spec;

        // ziv defines no named projections, so a name is an identifier this service does not
        // serve. Rendering the default for it would make EVERY string a valid image identifier
        // and give a 200 where the spec requires a 404.
        if let ProjectionId::Named(name) = id {
            return Err(TileError::UnknownProjection(name.clone()));
        }

        // Validate the raw request's z/t/channel indices against the image's declared
        // extents BEFORE resolving. This matters because `resolve()` only mutates channels
        // that already exist in `base_projection.channels` (capped to size_c.min(3) with no
        // omero) — a request for a channel index outside that catalog (e.g. `@c=999`, or
        // `@c=2` on a 2-channel image) would otherwise be silently dropped rather than
        // rejected. z/t are validated again below post-resolve, but checking the raw request
        // catches bad input at the earliest point.
        if let ProjectionId::Dynamic(d) = id {
            if let Some(z) = d.z {
                if z >= self.image.size_z() {
                    return Err(TileError::OutOfRange(format!(
                        "z={z} out of range (size_z={})",
                        self.image.size_z()
                    )));
                }
            }
            if let Some(t) = d.t {
                if t >= self.image.size_t() {
                    return Err(TileError::OutOfRange(format!(
                        "t={t} out of range (size_t={})",
                        self.image.size_t()
                    )));
                }
            }
            for dc in &d.channels {
                if dc.index as u64 >= self.image.size_c() {
                    return Err(TileError::OutOfRange(format!(
                        "channel index {} out of range (size_c={})",
                        dc.index,
                        self.image.size_c()
                    )));
                }
            }
        }

        let proj = self.resolve(id);
        let ZSelector::Plane(z) = proj.z;

        // Validate z/t/channel against the image's declared extents before touching zarrs.
        // `read_region_f64` does bounds-check the y/x range (returns a real `Err`), but not the
        // t/c/z indices used to select the singleton range on those axes, so anything derived
        // from an untrusted IIIF request must be rejected here rather than passed through.
        if z >= self.image.size_z() {
            return Err(TileError::OutOfRange(format!(
                "z={z} out of range (size_z={})",
                self.image.size_z()
            )));
        }
        if proj.t >= self.image.size_t() {
            return Err(TileError::OutOfRange(format!(
                "t={} out of range (size_t={})",
                proj.t,
                self.image.size_t()
            )));
        }
        for ch in &proj.channels {
            if ch.enabled && ch.index as u64 >= self.image.size_c() {
                return Err(TileError::OutOfRange(format!(
                    "channel index {} out of range (size_c={})",
                    ch.index,
                    self.image.size_c()
                )));
            }
        }

        // Which pixels this request is for: the intensity channels, or a label image the
        // identifier named instead. Everything after this point is identical for both, which is
        // the point of splitting here — rotation, quality and format are properties of the IIIF
        // request, not of what was drawn.
        let (rgb, out_w, out_h) = match label_selection(id) {
            Some(sel) if sel.overlay => self.render_overlay_rgb(sel, &proj, region, size)?,
            Some(sel) => self.render_label_rgb(sel, &proj, region, size)?,
            None => self.render_image_rgb(&proj, region, size)?,
        };

        // Spec order: region and size are already applied, then rotation, then quality, then
        // format. Rotation must follow the resample so `size` constrains the pre-rotation axes.
        let (mut out, out_w, out_h) = rotate_rgb8(&rgb, out_w, out_h, rotation)?;
        apply_quality(&mut out, quality);
        encode_rgb8(&out, out_w, out_h, format, jpeg_quality)
    }
}

/// The label component of an identifier, if it has one. `default` and named identifiers never do.
fn label_selection(id: &ProjectionId) -> Option<&DynLabel> {
    match id {
        ProjectionId::Dynamic(d) => d.label.as_ref(),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn engine() -> ZarrTileEngine {
        let img = ZarrImage::open("../../tests/fixtures/sample_v04.ome.zarr").unwrap();
        ZarrTileEngine::new(img)
    }

    /// The fixture that carries a `labels/` group. Image: 64x64, two levels. Label `nuclei`:
    /// 64x64, THREE levels, four flat quadrants valued 0/1/2/3. See `build_labels_fixture.rs`.
    fn engine_with_labels() -> ZarrTileEngine {
        let img = ZarrImage::open("../../tests/fixtures/sample_labels.ome.zarr").unwrap();
        ZarrTileEngine::new(img)
    }

    fn label_id(spec: &str) -> ProjectionId {
        iiif::parse_identifier(spec)
    }

    /// Decode a PNG render back to `(pixels_rgb, width, height)` so a test can talk about colours
    /// rather than bytes. PNG rather than JPEG throughout the label tests: label colours are flat
    /// and exact, and JPEG would smear the quadrant boundaries this is checking.
    fn decode_png(bytes: &[u8]) -> (Vec<u8>, u32, u32) {
        let decoder = png::Decoder::new(std::io::Cursor::new(bytes));
        let mut reader = decoder.read_info().unwrap();
        let mut buf = vec![0u8; reader.output_buffer_size().unwrap()];
        let info = reader.next_frame(&mut buf).unwrap();
        buf.truncate(info.buffer_size());
        (buf, info.width, info.height)
    }

    fn render_png(e: &ZarrTileEngine, id: &ProjectionId, size: Size) -> (Vec<u8>, u32, u32) {
        let bytes = e
            .render(
                id,
                &RenderSpec {
                    region: Region::Full,
                    size,
                    rotation: 0,
                    quality: Quality::Default,
                    format: Format::Png,
                    jpeg_quality: 85,
                },
            )
            .unwrap();
        decode_png(&bytes)
    }

    fn pixel(px: &[u8], w: u32, x: u32, y: u32) -> [u8; 3] {
        let i = ((y * w + x) * 3) as usize;
        [px[i], px[i + 1], px[i + 2]]
    }

    /// The four quadrant colours of the label fixture, sampled at the quadrant centres.
    fn quadrants(px: &[u8], w: u32, h: u32) -> [[u8; 3]; 4] {
        let (qx, qy) = (w / 4, h / 4);
        [
            pixel(px, w, qx, qy),
            pixel(px, w, qx * 3, qy),
            pixel(px, w, qx, qy * 3),
            pixel(px, w, qx * 3, qy * 3),
        ]
    }

    // --- label rendering ---

    /// `table` renders exactly what the image declares: value 1 red, 2 green, 3 blue at half
    /// alpha over black, and background (0) left black because it has no entry.
    #[test]
    fn the_table_palette_renders_the_declared_colours() {
        let e = engine_with_labels();
        let (px, w, h) = render_png(&e, &label_id("@label=nuclei:table"), Size::Max);
        assert_eq!((w, h), (64, 64));
        assert_eq!(
            quadrants(&px, w, h),
            [[0, 0, 0], [255, 0, 0], [0, 255, 0], [0, 0, 128]]
        );
    }

    /// The default palette must colour the same four quadrants distinctly, background included as
    /// black — without consulting the declared table at all.
    #[test]
    fn the_default_palette_renders_distinct_colours_per_value() {
        let e = engine_with_labels();
        let (px, w, h) = render_png(&e, &label_id("@label=nuclei"), Size::Max);
        let q = quadrants(&px, w, h);
        assert_eq!(q[0], [0, 0, 0], "background stays black");
        let distinct: std::collections::HashSet<_> = q[1..].iter().collect();
        assert_eq!(distinct.len(), 3, "labels 1, 2 and 3 must differ: {q:?}");
        assert_ne!(q, [[0, 0, 0], [255, 0, 0], [0, 255, 0], [0, 0, 128]]);
    }

    /// End-to-end proof that the resample does not interpolate: downscaled 8x, every pixel must
    /// still be one of the four colours the four label values map to. A convolution filter would
    /// produce blended edges between quadrants and fail this immediately.
    #[test]
    fn a_downscaled_label_render_invents_no_colours() {
        let e = engine_with_labels();
        let (full, _, _) = render_png(&e, &label_id("@label=nuclei:table"), Size::Max);
        let allowed: std::collections::HashSet<[u8; 3]> = (0..full.len() / 3)
            .map(|i| [full[i * 3], full[i * 3 + 1], full[i * 3 + 2]])
            .collect();
        assert_eq!(allowed.len(), 4, "the fixture has four label values");

        let (small, w, h) = render_png(&e, &label_id("@label=nuclei:table"), Size::Wh(8, 8));
        assert_eq!((w, h), (8, 8));
        for i in 0..(w * h) as usize {
            let c = [small[i * 3], small[i * 3 + 1], small[i * 3 + 2]];
            assert!(
                allowed.contains(&c),
                "pixel {i} is {c:?}, not a label colour"
            );
        }
    }

    /// The label pyramid is deeper than the image's. A request downscaled far enough to want a
    /// level the IMAGE does not have must read the level the LABEL does have — using the image's
    /// `scale_factors` here would cap at level 1 and read four times the data it needs to.
    #[test]
    fn a_label_render_reads_the_labels_own_pyramid() {
        let e = engine_with_labels();
        assert_eq!(e.scale_factors, vec![1, 2], "image: two levels");
        assert_eq!(e.label_scale_factors, vec![vec![1, 2, 4]], "label: three");
        // 64 -> 8 is an 8x downscale: level 2 (factor 4) is the finest level that still qualifies.
        assert_eq!(pick_level_in(&e.label_scale_factors[0], 8.0), 2);
        // And it renders, which it could not if the level index came from the image's pyramid.
        let (_, w, h) = render_png(&e, &label_id("@label=nuclei:table"), Size::Wh(8, 8));
        assert_eq!((w, h), (8, 8));
    }

    /// A label the image does not carry is a 404 (`UnknownLabel`), not a blank render and not a
    /// 500 — the request grammar was fine, the resource does not exist.
    #[test]
    fn an_unknown_label_name_is_rejected() {
        let e = engine_with_labels();
        let err = e
            .render(
                &label_id("@label=mitochondria"),
                &RenderSpec {
                    region: Region::Full,
                    size: Size::Max,
                    rotation: 0,
                    quality: Quality::Default,
                    format: Format::Png,
                    jpeg_quality: 85,
                },
            )
            .unwrap_err();
        assert!(
            matches!(err, TileError::UnknownLabel(ref n) if n == "mitochondria"),
            "got {err:?}"
        );
    }

    /// An image with no labels at all must reject any `label=`, rather than the component being
    /// silently ignored and the intensity image served in its place.
    #[test]
    fn a_label_request_against_an_image_without_labels_is_rejected() {
        let e = engine();
        let err = e
            .render(
                &label_id("@label=nuclei"),
                &RenderSpec {
                    region: Region::Full,
                    size: Size::Max,
                    rotation: 0,
                    quality: Quality::Default,
                    format: Format::Png,
                    jpeg_quality: 85,
                },
            )
            .unwrap_err();
        assert!(matches!(err, TileError::UnknownLabel(_)), "got {err:?}");
    }

    #[test]
    fn an_unknown_palette_is_rejected() {
        let e = engine_with_labels();
        let err = e
            .render(
                &label_id("@label=nuclei:rainbow"),
                &RenderSpec {
                    region: Region::Full,
                    size: Size::Max,
                    rotation: 0,
                    quality: Quality::Default,
                    format: Format::Png,
                    jpeg_quality: 85,
                },
            )
            .unwrap_err();
        assert!(matches!(err, TileError::OutOfRange(_)), "got {err:?}");
    }

    /// Without a `label=` component nothing changes: the same identifier grammar still renders
    /// the intensity image, and an image that happens to carry labels is unaffected.
    #[test]
    fn an_identifier_without_a_label_component_renders_the_image() {
        let e = engine_with_labels();
        let (px, w, _) = render_png(&e, &ProjectionId::Default, Size::Max);
        // The fixture image is a horizontal ramp, so the right edge is bright and the left dark —
        // nothing like the label quadrants.
        assert!(pixel(&px, w, 63, 32)[0] > pixel(&px, w, 0, 32)[0]);
    }

    /// A region request must select the same part of the label that it selects of the image.
    #[test]
    fn a_region_request_selects_the_matching_part_of_the_label() {
        let e = engine_with_labels();
        // Bottom-right 32x32 quadrant: label value 3 throughout, which `table` paints blue.
        let bytes = e
            .render(
                &label_id("@label=nuclei:table"),
                &RenderSpec {
                    region: Region::Px {
                        x: 32,
                        y: 32,
                        w: 32,
                        h: 32,
                    },
                    size: Size::Max,
                    rotation: 0,
                    quality: Quality::Default,
                    format: Format::Png,
                    jpeg_quality: 85,
                },
            )
            .unwrap();
        let (px, w, h) = decode_png(&bytes);
        assert_eq!((w, h), (32, 32));
        assert!(
            px.chunks_exact(3).all(|c| c == [0, 0, 128]),
            "every pixel of the bottom-right quadrant should be label 3"
        );
    }

    // --- overlay ---

    /// The view the feature exists for. The fixture image is a horizontal ramp, so the base under
    /// each quadrant is known; the mask's declared colours are opaque, so an opaque overlay must
    /// replace the base exactly where an object is and leave it exactly where there is none.
    #[test]
    fn an_opaque_overlay_replaces_the_base_only_where_the_mask_is() {
        let e = engine_with_labels();
        let (image, w, h) = render_png(&e, &ProjectionId::Default, Size::Max);
        let (over, ow, oh) = render_png(&e, &label_id("@overlay=nuclei:table"), Size::Max);
        assert_eq!((ow, oh), (w, h));

        let (qx, qy) = (w / 4, h / 4);
        // Top-left is label 0, which has no entry: the image shows through untouched.
        assert_eq!(pixel(&over, w, qx, qy), pixel(&image, w, qx, qy));
        // The other three are opaque declared colours, so they win outright.
        assert_eq!(pixel(&over, w, qx * 3, qy), [255, 0, 0]);
        assert_eq!(pixel(&over, w, qx, qy * 3), [0, 255, 0]);
    }

    /// Opacity blends rather than replacing, and the blend is against the actual image pixel.
    #[test]
    fn a_translucent_overlay_blends_with_the_image_underneath() {
        let e = engine_with_labels();
        let (image, w, h) = render_png(&e, &ProjectionId::Default, Size::Max);
        let (over, _, _) = render_png(&e, &label_id("@overlay=nuclei:table:0.5"), Size::Max);

        let (qx, qy) = (w / 4, h / 4);
        let base = pixel(&image, w, qx * 3, qy);
        let got = pixel(&over, w, qx * 3, qy);
        // Declared (255,0,0,255) at opacity 0.5 -> alpha 128 over the ramp.
        let want = [0, 1, 2].map(|i| {
            let over_c = [255u32, 0, 0][i];
            ((over_c * 128 + base[i] as u32 * 127 + 127) / 255) as u8
        });
        for i in 0..3 {
            assert!(
                got[i].abs_diff(want[i]) <= 1,
                "channel {i}: base {base:?}, got {got:?}, want ~{want:?}"
            );
        }
        assert_ne!(got, base, "a 0.5 overlay must actually change the image");
    }

    /// Opacity 0 is a legal request that must be a no-op, not an error and not a black frame.
    #[test]
    fn an_overlay_at_zero_opacity_is_the_image_itself() {
        let e = engine_with_labels();
        let (image, _, _) = render_png(&e, &ProjectionId::Default, Size::Max);
        let (over, _, _) = render_png(&e, &label_id("@overlay=nuclei:table:0"), Size::Max);
        assert_eq!(over, image);
    }

    /// An overlay draws over the CHANNEL SELECTION, not over a fixed base. The fixture's two
    /// channels are a red horizontal ramp and a blue vertical one, so dropping the blue must change
    /// the picture everywhere the mask is transparent and change nothing where it is opaque.
    #[test]
    fn an_overlay_composites_over_the_selected_channels() {
        let e = engine_with_labels();
        let (both, w, h) = render_png(&e, &label_id("@overlay=nuclei:table"), Size::Max);
        let (across, _, _) = render_png(&e, &label_id("@c=0,overlay=nuclei:table"), Size::Max);
        assert_ne!(both, across, "dropping a channel must change the base");

        let (qx, qy) = (w / 4, h / 4);
        // Top-left is label 0: no mask, so the base shows through and the two must differ.
        assert_eq!(pixel(&both, w, qx, qy), [63, 0, 63], "red ramp + blue ramp");
        assert_eq!(pixel(&across, w, qx, qy), [63, 0, 0], "red ramp only");
        // Top-right is label 1, declared opaque red: the mask wins, so the two must agree.
        assert_eq!(pixel(&both, w, qx * 3, qy), [255, 0, 0]);
        assert_eq!(pixel(&across, w, qx * 3, qy), [255, 0, 0]);
    }

    /// The three renders are three different pictures, and each identifier must get its own.
    #[test]
    fn image_label_and_overlay_are_three_distinct_renders() {
        let e = engine_with_labels();
        let (image, _, _) = render_png(&e, &ProjectionId::Default, Size::Max);
        let (label, _, _) = render_png(&e, &label_id("@label=nuclei:table"), Size::Max);
        let (over, _, _) = render_png(&e, &label_id("@overlay=nuclei:table"), Size::Max);
        assert_ne!(image, label);
        assert_ne!(image, over);
        assert_ne!(label, over);
    }

    #[test]
    fn an_out_of_range_opacity_is_rejected() {
        let e = engine_with_labels();
        for spec in ["@overlay=nuclei:table:1.5", "@overlay=nuclei:table:-0.2"] {
            let err = e
                .render(
                    &label_id(spec),
                    &RenderSpec {
                        region: Region::Full,
                        size: Size::Max,
                        rotation: 0,
                        quality: Quality::Default,
                        format: Format::Png,
                        jpeg_quality: 85,
                    },
                )
                .unwrap_err();
            assert!(matches!(err, TileError::OutOfRange(_)), "{spec}: {err:?}");
        }
    }

    /// An unknown label is a 404 whichever spelling asked for it.
    #[test]
    fn an_unknown_label_is_rejected_for_an_overlay_too() {
        let e = engine_with_labels();
        let err = e
            .render(
                &label_id("@overlay=mitochondria"),
                &RenderSpec {
                    region: Region::Full,
                    size: Size::Max,
                    rotation: 0,
                    quality: Quality::Default,
                    format: Format::Png,
                    jpeg_quality: 85,
                },
            )
            .unwrap_err();
        assert!(matches!(err, TileError::UnknownLabel(_)), "got {err:?}");
    }

    // --- window rescaling between an image and a label of a different size ---

    /// The common case: a label whose level 0 matches its parent's. Must be bit-exact identity,
    /// not merely close, or every tile would drift by a pixel.
    #[test]
    fn rescaling_a_window_between_equal_extents_is_identity() {
        let w = (13, 7, 100, 50);
        assert_eq!(rescale_window(w, (275, 271), (275, 271)), w);
    }

    /// A label stored at half the image's resolution. The window halves, and the extent rounds UP
    /// so the mapped window still covers everything that was asked for.
    #[test]
    fn rescaling_a_window_into_a_coarser_label_rounds_the_extent_up() {
        assert_eq!(
            rescale_window((10, 20, 31, 41), (64, 64), (32, 32)),
            (5, 10, 16, 21)
        );
    }

    /// Whatever the ratio, the mapped window must stay inside the label. This is the bound that
    /// stops a region from reading off the end of a differently-sized label array.
    ///
    /// The inputs are windows `resolve_request` can actually produce against a 64x64 image (it
    /// clamps into bounds before this is ever called): the whole image, a single corner pixel,
    /// and an offset slab.
    #[test]
    fn a_rescaled_window_never_leaves_the_label() {
        for to in [(1, 1), (7, 5), (64, 64), (999, 3), (64, 128)] {
            for window in [(0, 0, 64, 64), (63, 63, 1, 1), (10, 20, 31, 41)] {
                let (x, y, w, h) = rescale_window(window, (64, 64), to);
                assert!(
                    x + w <= to.1 && y + h <= to.0,
                    "{window:?} into {to:?} gave {:?}",
                    (x, y, w, h)
                );
                assert!(w >= 1 && h >= 1, "{window:?} into {to:?} collapsed");
            }
        }
    }

    /// Fixture with axes `[c, x, y]` (X before Y; neither positionally the "last two as
    /// Y-then-X"). Shape is c=2, x=40 (WIDTH), y=20 (HEIGHT), width != height so a positional
    /// `shape[len-2]`/`shape[len-1]` read swaps AND mis-sizes width/height rather than merely
    /// transposing equal dims. Built by `tests/build_axes_fixture.rs` into the committed fixture
    /// tree; see that file for the exact layout.
    fn engine_non_yx_last() -> ZarrTileEngine {
        let img = ZarrImage::open("../../tests/fixtures/sample_axes_cxy.ome.zarr").unwrap();
        ZarrTileEngine::new(img)
    }

    /// Regression lock (P1 deliverable 3): `image_info` and `plan` must resolve Y/X via the
    /// `AxesModel`, never via `level_shape[len-2..]` positional indexing. Against the OLD
    /// positional code, this fixture's `[c, x, y]` axes order means `shape[len-2]` (=x=40) would
    /// be read as Y and `shape[len-1]` (=y=20) as X, so `image_info` would report
    /// width=20,height=40 instead of the correct width=40,height=20 — this test fails under that
    /// code and passes only once every Y/X lookup is model-driven.
    #[test]
    fn image_info_uses_axes_model_not_position_for_non_yx_last_order() {
        let e = engine_non_yx_last();
        let info = e.image_info("https://host/iiif/img/default");
        assert_eq!((info.width, info.height), (40, 20));
        // `sizes` (per-level whole-image (w,h)) must also be model-driven, not
        // `(shape[len-1], shape[len-2])` positional — the single level here must report
        // (40, 20), not the positionally-swapped (20, 40).
        assert_eq!(info.sizes, vec![(40, 20)]);
    }

    /// Same regression lock via the `plan()` path: request the full region at the model-correct
    /// full size and confirm plan resolves the level dims (and thus the read window) using the
    /// `AxesModel`-derived width/height, not positional indexing.
    #[test]
    fn plan_uses_axes_model_not_position_for_non_yx_last_order() {
        let e = engine_non_yx_last();
        let (level, (rx, ry, rw, rh), (out_w, out_h)) = e.plan(Region::Full, Size::Max).unwrap();
        assert_eq!(level, 0);
        assert_eq!((rx, ry, rw, rh), (0, 0, 40, 20));
        assert_eq!((out_w, out_h), (40, 20));
    }

    #[test]
    fn image_info_matches_fixture() {
        let e = engine();
        let info = e.image_info("https://host/iiif/img/default");
        assert_eq!((info.width, info.height), (64, 64));
        assert_eq!(info.scale_factors, vec![1, 2]);
        assert_eq!(info.tile_size, 512);
    }

    /// `output_size` must agree with `plan()`'s own third element (the same geometry, without the
    /// level lookup) — this is the whole point of exposing it as a separate call rather than a
    /// reimplementation: a caller (`ziv render`'s size budget) that resolves `full`/`max` this way
    /// must see exactly the number a real render would produce.
    #[test]
    fn output_size_agrees_with_plan_for_full_max() {
        let e = engine();
        let (_, _, plan_out) = e.plan(Region::Full, Size::Max).unwrap();
        let out = e.output_size(Region::Full, Size::Max).unwrap();
        assert_eq!(out, plan_out);
        assert_eq!(out, (64, 64));
    }

    #[test]
    fn output_size_scales_by_requested_width() {
        let e = engine();
        let (w, h) = e.output_size(Region::Full, Size::Width(32)).unwrap();
        assert_eq!((w, h), (32, 32));
    }

    /// `output_size` must reject exactly when `plan()` would, using the same read-side geometry
    /// bound — a declared-huge single level over `MAX_READ_PIXELS`/`MAX_OUTPUT_PIXELS`.
    #[test]
    fn output_size_rejects_what_plan_rejects() {
        let img = ZarrImage::open("../../tests/fixtures/sample_huge_level.ome.zarr").unwrap();
        let e = ZarrTileEngine::new(img);
        assert!(e.plan(Region::Full, Size::Max).is_err());
        assert!(e.output_size(Region::Full, Size::Max).is_err());
    }

    #[test]
    fn full_default_tile_is_valid_jpeg() {
        let e = engine();
        let jpeg = e
            .tile(&ProjectionId::Default, Region::Full, Size::Max, 85)
            .unwrap();
        assert_eq!(&jpeg[0..2], &[0xFF, 0xD8]); // JPEG SOI
    }

    #[test]
    fn dynamic_projection_single_channel() {
        let e = engine();
        let id = iiif::parse_identifier("@c=0");
        let jpeg = e
            .tile(
                &id,
                Region::Px {
                    x: 0,
                    y: 0,
                    w: 32,
                    h: 32,
                },
                Size::Wh(32, 32),
                85,
            )
            .unwrap();
        assert_eq!(&jpeg[0..2], &[0xFF, 0xD8]);
    }

    // --- Bounds-checking regression tests (untrusted-input safety) ---

    #[test]
    fn oversized_region_is_clamped_not_panicking() {
        let e = engine();
        // Region far larger than the 64x64 fixture; must clamp rather than panic/underflow.
        let jpeg = e
            .tile(
                &ProjectionId::Default,
                Region::Px {
                    x: 0,
                    y: 0,
                    w: 10_000,
                    h: 10_000,
                },
                Size::Max,
                85,
            )
            .unwrap();
        assert_eq!(&jpeg[0..2], &[0xFF, 0xD8]);
    }

    #[test]
    fn offset_region_beyond_bounds_is_clamped_not_panicking() {
        let e = engine();
        // Origin itself is past the image edge; must clamp to an empty-but-valid window
        // instead of underflowing `full_x - x`.
        let jpeg = e
            .tile(
                &ProjectionId::Default,
                Region::Px {
                    x: 1_000,
                    y: 1_000,
                    w: 100,
                    h: 100,
                },
                Size::Max,
                85,
            )
            .unwrap();
        assert_eq!(&jpeg[0..2], &[0xFF, 0xD8]);
    }

    #[test]
    fn out_of_range_z_is_rejected() {
        let e = engine();
        let id = iiif::parse_identifier("@z=999");
        let err = e.tile(&id, Region::Full, Size::Max, 85).unwrap_err();
        assert!(matches!(err, TileError::OutOfRange(_)));
    }

    #[test]
    fn out_of_range_t_is_rejected() {
        let e = engine();
        let id = iiif::parse_identifier("@t=999");
        let err = e.tile(&id, Region::Full, Size::Max, 85).unwrap_err();
        assert!(matches!(err, TileError::OutOfRange(_)));
    }

    #[test]
    fn out_of_range_channel_is_rejected() {
        let e = engine();
        let id = iiif::parse_identifier("@c=999");
        let err = e.tile(&id, Region::Full, Size::Max, 85).unwrap_err();
        assert!(matches!(err, TileError::OutOfRange(_)));
    }

    #[test]
    fn oversized_size_does_not_panic() {
        let e = engine();
        // `Size::Width`/`Height` carry raw, unbounded u64s straight from the untrusted IIIF
        // size path segment (e.g. "99999999999999,"). Pre-fix, `rh * w` in `plan()` overflows
        // this u64 multiply and panics in debug builds (`attempt to multiply with overflow`).
        // The fix does the multiply in u128 (so it can never overflow) and then REJECTS an
        // absurd result with `TileError::OutOfRange` rather than proceeding, so every arm here
        // must return that error rather than panicking (or hanging on a multi-gigabyte resize).
        let huge = u64::MAX / 10;
        let err_w = e
            .tile(&ProjectionId::Default, Region::Full, Size::Width(huge), 85)
            .unwrap_err();
        assert!(matches!(err_w, TileError::OutOfRange(_)));

        let err_h = e
            .tile(&ProjectionId::Default, Region::Full, Size::Height(huge), 85)
            .unwrap_err();
        assert!(matches!(err_h, TileError::OutOfRange(_)));

        // Size::Wh and Size::Pct take an unbounded u64/f64 too; make sure neither panics.
        let err_wh = e
            .tile(
                &ProjectionId::Default,
                Region::Full,
                Size::Wh(huge, huge),
                85,
            )
            .unwrap_err();
        assert!(matches!(err_wh, TileError::OutOfRange(_)));

        let err_pct = e
            .tile(
                &ProjectionId::Default,
                Region::Full,
                Size::Pct(f64::MAX),
                85,
            )
            .unwrap_err();
        assert!(matches!(err_pct, TileError::OutOfRange(_)));

        // Two dimensions each individually within MAX_OUTPUT_DIM (65535) but whose product is
        // still an absurd ~4.3 billion pixels must also be rejected by the total-pixel check,
        // not just the per-dimension check.
        let err_wide_pair = e
            .tile(
                &ProjectionId::Default,
                Region::Full,
                Size::Wh(65_535, 65_535),
                85,
            )
            .unwrap_err();
        assert!(matches!(err_wide_pair, TileError::OutOfRange(_)));

        // Sanity: a reasonable, in-bounds size still succeeds (the fix must not over-reject).
        // The fixture is 64x64, so this must be a DOWNSCALE — an upscale is refused by the
        // no-upscaling rule below, not by these DoS caps.
        let jpeg = e
            .tile(&ProjectionId::Default, Region::Full, Size::Wh(32, 32), 85)
            .unwrap();
        assert_eq!(&jpeg[0..2], &[0xFF, 0xD8]);
    }

    /// IIIF 3.0 permits an output larger than the extracted region only via the `^` upscaling
    /// syntax, which ziv does not implement — so a plain oversized size must be refused rather
    /// than silently upscaled. The fixture is 64x64.
    #[test]
    fn upscaling_is_refused() {
        let e = engine();
        for size in [
            Size::Wh(128, 128),
            Size::Width(128),
            Size::Height(128),
            Size::Pct(200.0),
            Size::Confined(2000, 3000),
        ] {
            let err = e
                .tile(&ProjectionId::Default, Region::Full, size, 85)
                .unwrap_err();
            assert!(
                matches!(err, TileError::OutOfRange(_)),
                "{size:?} upscales a 64x64 image and must be refused, got {err:?}"
            );
        }

        // Exactly the region's own size is NOT upscaling and must still be served.
        let jpeg = e
            .tile(&ProjectionId::Default, Region::Full, Size::Wh(64, 64), 85)
            .unwrap();
        assert_eq!(&jpeg[0..2], &[0xFF, 0xD8]);
    }

    // --- Pyramid level selection (OOM-prevention fix) ---

    /// Direct unit test of the level-pick math against the fixture's `scale_factors = [1, 2]`
    /// (see `image_info_matches_fixture`), independent of the full read/resample/encode path.
    #[test]
    fn pick_level_selects_finest_level_within_downscale() {
        let e = engine();
        // Below level 1's factor (2): level 0 is the finest level whose factor (1) still
        // qualifies (`<= read_downscale`), so level 0 must be picked even though it's not the
        // last level in `scale_factors` — this guards against a coarser level being picked
        // when it's not warranted (would upsample and lose quality).
        assert_eq!(pick_level_in(&e.scale_factors, 1.0), 0);
        assert_eq!(pick_level_in(&e.scale_factors, 1.9), 0);
        // At or above level 1's factor (2): level 1 now qualifies and, being the largest
        // qualifying index, should be picked over level 0.
        assert_eq!(pick_level_in(&e.scale_factors, 2.0), 1);
        assert_eq!(pick_level_in(&e.scale_factors, 3.9), 1);
        // Far beyond the coarsest level's factor: still clamps to the coarsest level (1) rather
        // than an out-of-range index, since `scale_factors` has no entry beyond it.
        assert_eq!(pick_level_in(&e.scale_factors, 1000.0), 1);
        // Below every level's factor (i.e. less than 1, degenerate/upscale case): falls back to
        // level 0.
        assert_eq!(pick_level_in(&e.scale_factors, 0.5), 0);
    }

    // --- TileEngine trait seam ---

    /// `ZarrTileEngine` must implement `TileEngine` so callers (server, future exporter/cache)
    /// can depend on `dyn TileEngine` rather than the concrete struct. Exercise both trait
    /// methods through a trait object to prove the impl is wired up, not just present.
    #[test]
    fn zarr_tile_engine_is_usable_as_dyn_tile_engine() {
        let e: std::sync::Arc<dyn TileEngine + Send + Sync> = std::sync::Arc::new(engine());
        let info = e.image_info("https://host/iiif/img/default");
        assert_eq!((info.width, info.height), (64, 64));

        let jpeg = e
            .tile(&ProjectionId::Default, Region::Full, Size::Max, 85)
            .unwrap();
        assert_eq!(&jpeg[0..2], &[0xFF, 0xD8]);
    }

    #[test]
    fn downscaled_request_uses_coarser_level() {
        let e = engine();
        // Fixture is 64x64 with scale_factors [1, 2] (see `image_info_matches_fixture`).
        // Requesting the full region resampled down to 16x16 is a 4x downscale, which is
        // `>= scale_factors[1]` (2), so `pick_level` must choose level 1 (the coarsest
        // available) rather than reading the full 64x64 level-0 plane and shrinking it after
        // the fact. Confirm the level-pick math agrees before checking the end-to-end path.
        assert_eq!(pick_level_in(&e.scale_factors, 4.0), 1);

        let jpeg = e
            .tile(&ProjectionId::Default, Region::Full, Size::Wh(16, 16), 85)
            .unwrap();
        assert_eq!(&jpeg[0..2], &[0xFF, 0xD8]);

        // Same assertion via `plan()` directly: for this request the chosen level must be 1,
        // proving `tile()` actually reads from the coarser level rather than merely resampling
        // a level-0 read down to the right output size afterward.
        let (level, _, (out_w, out_h)) = e.plan(Region::Full, Size::Wh(16, 16)).unwrap();
        assert_eq!(level, 1);
        assert_eq!((out_w, out_h), (16, 16));
    }

    /// `MAX_READ_PIXELS` bounds the READ window independently of the output size — confirms the
    /// cap is on PIXEL COUNT, not bytes, and still holds after widening reads from u16 (2
    /// bytes/px) to f64 (8 bytes/px, Deliverable A). A single-level 100000x100000 declared
    /// fixture (`sample_huge_level.ome.zarr`, no real chunk data — see
    /// `build_huge_level_fixture.rs`) has only one pyramid level, so `Size::Max` forces
    /// `pick_level` to level 0 and the read window equals the full 100000x100000 region
    /// (10,000,000,000 px), far past `MAX_OUTPUT_PIXELS`/`MAX_READ_PIXELS`
    /// (16384*16384 ~= 268,435,456 px). `plan()` must reject with `OutOfRange` before any chunk
    /// read is attempted (this fixture has no chunk files — an attempted real read would fail
    /// differently, proving the rejection happens at the bounds-check stage).
    #[test]
    fn full_region_on_huge_single_level_is_rejected_not_oom() {
        let img = ZarrImage::open("../../tests/fixtures/sample_huge_level.ome.zarr").unwrap();
        let e = ZarrTileEngine::new(img);
        let err = e.plan(Region::Full, Size::Max).unwrap_err();
        assert!(matches!(err, TileError::OutOfRange(_)));
    }

    // --- Startup auto-stretch read guard (P2 follow-up fix) ---

    /// `ZarrTileEngine::new()`'s auto-stretch path (no `omero` metadata) used to read the ENTIRE
    /// coarsest pyramid level unconditionally to compute a percentile window, with no size guard
    /// — for a huge remote image with no `omero` metadata (reachable via P2's `serve
    /// <remote-url>`), that's a full synchronous network read triggered before the server even
    /// binds. `sample_huge_level_no_omero.ome.zarr` (100000x100000 declared shape, NO chunk data,
    /// NO `omero` metadata — see `build_huge_level_no_omero_fixture.rs`) is exactly this shape:
    /// if `new()` attempted the unguarded read, it would try to read 10 billion pixels from a
    /// fixture with zero chunk files, which would hang or error deep inside zarrs rather than
    /// completing quickly. This test proves `new()` completes quickly (bounded by
    /// `MAX_READ_PIXELS`) and falls back to `natural_range(image.dtype())` — for this fixture's
    /// `|u1` (u8) dtype, `(0.0, 255.0)` — rather than attempting the full-level read.
    #[test]
    fn new_skips_unbounded_autostretch_read_on_huge_no_omero_level() {
        let img =
            ZarrImage::open("../../tests/fixtures/sample_huge_level_no_omero.ome.zarr").unwrap();
        assert!(img.omero().is_none());
        assert_eq!(img.dtype(), zarr_core::DType::U8);

        // Must complete (not hang/OOM attempting a 10-billion-pixel read against a fixture with
        // no chunk data) and produce a sane fallback window.
        let e = ZarrTileEngine::new(img);
        assert_eq!(e.base_projection.channels.len(), 1);
        assert_eq!(e.base_projection.channels[0].window, (0.0, 255.0));
    }

    /// Regression guard: normal small images WITHOUT `omero` metadata must still get a real
    /// percentile-computed auto-stretch window (not the dtype-natural-range fallback) — the new
    /// `MAX_READ_PIXELS` guard in `new()` must only kick in for genuinely huge levels, never for
    /// the common small-image case. `sample_f32.ome.zarr` (16x16, f32, no `omero` metadata, values
    /// spanning 0.0..=1000.0 — see P2's `build_f32_fixture.rs`) is well outside both the
    /// `natural_range(F32)` fallback `(0.0, 1.0)` AND the `default_projection` placeholder
    /// `(0.0, 65535.0)`, so a real percentile computation is unambiguously distinguishable from
    /// either fallback.
    #[test]
    fn small_image_without_omero_still_gets_percentile_autostretch() {
        let img = ZarrImage::open("../../tests/fixtures/sample_f32.ome.zarr").unwrap();
        assert!(img.omero().is_none());
        let e = ZarrTileEngine::new(img);
        for ch in &e.base_projection.channels {
            assert!(ch.window.0 <= ch.window.1);
            // Must not be the dtype-natural-range fallback (proves the guard did NOT skip this
            // small read) and not the pre-auto-stretch placeholder (proves auto-stretch ran at
            // all).
            assert_ne!(ch.window, (0.0, 1.0));
            assert_ne!(ch.window, (0.0, 65535.0));
        }
    }

    /// The document the viewer builds its controls from. Pinned field by field because a server
    /// and a static export both emit it through `to_json`, and the viewer reads either one without
    /// knowing which it got.
    #[test]
    fn dimensions_json_has_the_documented_shape() {
        let d = ImageDimensions {
            size_t: 3,
            size_z: 5,
            size_c: 2,
            default_t: 0,
            default_z: 2,
            channels: vec![
                ChannelDescriptor {
                    index: 0,
                    label: Some("Red".into()),
                    color: "FF0000".into(),
                    window: (0.0, 255.0),
                    active: true,
                },
                ChannelDescriptor {
                    index: 1,
                    label: None,
                    color: "00FF00".into(),
                    window: (10.0, 20.0),
                    active: false,
                },
            ],
            labels: vec![LabelDescriptor {
                name: "nuclei".into(),
                declared_colors: 4,
            }],
            label_open_failures: vec![LabelOpenFailure {
                name: "cells".into(),
                reason: "unsupported dtype: int64 / <i8".into(),
            }],
        };
        assert_eq!(
            d.to_json(),
            serde_json::json!({
                "sizeT": 3, "sizeZ": 5, "sizeC": 2, "defaultT": 0, "defaultZ": 2,
                "channels": [
                    {"index": 0, "label": "Red", "color": "FF0000",
                     "window": {"start": 0.0, "end": 255.0}, "active": true},
                    {"index": 1, "label": null, "color": "00FF00",
                     "window": {"start": 10.0, "end": 20.0}, "active": false}
                ],
                "labels": [{"name": "nuclei", "declaredColors": 4}],
                "labelOpenFailures": [{"name": "cells", "reason": "unsupported dtype: int64 / <i8"}]
            })
        );
    }
}
