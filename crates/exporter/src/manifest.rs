//! The IIIF Presentation 3 manifest a multi-view export writes beside its image services
//! (spec §6.2).
//!
//! Each tree is already a standard IIIF image service. A standard viewer, though, sees N separate
//! images; this manifest is the standard way to present them as one object, one canvas per plane,
//! with overlays as alternatives on their plane.

use iiif::ImageInfo;
use serde_json::{json, Value};
use tiling::ImageDimensions;

use crate::enumerate::{enumerate_request_space, smallest_level_whole_image};
use crate::views::{View, ViewPlan};

/// Printed whenever the manifest cannot be given absolute ids.
pub const RELATIVE_MANIFEST_WARNING: &str = "manifest.json has relative ids, which is not valid \
    IIIF Presentation 3; most IIIF viewers will not load it. Re-export with \
    --id https://<where this will be hosted>.";

/// Printed instead of writing `manifest.json` when the image is over the whole-image budget
/// (`iiif::level0::MAX_WHOLE_IMAGE_PIXELS`/`MAX_WHOLE_IMAGE_EDGE`) AND even its smallest pyramid
/// level is too large to serve as a canvas body: at that point no whole-image file exists, or ever
/// will at this budget, for any canvas body to name, so a manifest would just be another 404
/// waiting to happen. See `writer::export_with_progress`'s budget gate, which decides this.
pub const NO_MANIFEST_OVER_BUDGET_WARNING: &str = "manifest.json not written: this image is over \
    the whole-image budget and even its smallest level is too large to serve as a canvas body.";

/// How ids are spelled: joined to an absolute base, or as paths relative to the export root.
struct Ids {
    absolute: bool,
    /// `""` for a relative export, else the root id plus `/`.
    prefix: String,
}

impl Ids {
    fn new(root_id: &str) -> Self {
        let root = root_id.trim_end_matches('/');
        Ids {
            absolute: root.starts_with("http://") || root.starts_with("https://"),
            prefix: if root.is_empty() || root == "." {
                String::new()
            } else {
                format!("{root}/")
            },
        }
    }

    fn path(&self, relative: &str) -> String {
        format!("{}{relative}", self.prefix)
    }

    /// The service id for the tree in `folder`. In relative mode this is the FOLDER, never the
    /// tree's own `info.json` id: without `--id` every tree's id is `.`, which would point every
    /// body at the root.
    fn tree(&self, folder: &str) -> String {
        if folder == "." {
            match self.prefix.trim_end_matches('/') {
                "" => ".".to_string(),
                root => root.to_string(),
            }
        } else {
            self.path(folder)
        }
    }

    fn in_tree(&self, folder: &str, relative: &str) -> String {
        if folder == "." {
            self.path(relative)
        } else {
            self.path(&format!("{folder}/{relative}"))
        }
    }
}

/// The whole-image derivative a canvas body points at, as `(path within a tree, width, height)`.
///
/// WITHIN budget (`iiif::level0_sizes(info)` is `Some` and its `within_budget()` holds): the
/// largest whole image `info.json` itself advertises, `full/{maxWidth},{maxHeight}/0/default.jpg`.
/// This is the SAME `level0_sizes`/`within_budget` decision `writer::write_tree`'s level 0
/// contract makes to decide what to write, not a second, independent derivation of it, so the file
/// this names is guaranteed to exist: within budget, the writer materialises a whole image for
/// every size the plan advertises, and `max_width`/`max_height` is always the largest of them —
/// guaranteed by `sizes` being finest-first and non-increasing on both axes (equivalently: the
/// level sizes OpenSeadragon rebuilds, which are ascending, never decrease on either axis), the
/// rule `iiif::level0::level0_sizes` enforces via `osd_tiling_agrees_with_the_export` before
/// returning a plan at all, rather than an assumption this function makes independently.
///
/// Deliberately NOT the largest whole image in OpenSeadragon's OWN request space
/// (`enumerate_request_space`): a pyramid level whose tile grid has more than one cell is
/// genuinely multi-tile, and OSD never asks for that level's whole image at all (see the
/// `enumerate` module doc), so that request space can be much smaller than what the tree actually
/// advertises and writes. Pointing a canvas body there, as this function used to, could serve a
/// visibly lower-resolution image than the tree offers even though the higher-resolution file
/// exists on disk.
///
/// OVER budget, `write_tree` writes no advertised whole images at all (see its own doc comment on
/// the budget gate), so there is nothing from `level0_sizes` to point at: fall back to whatever
/// whole image OpenSeadragon's own request space happens to contain (largest first), and beneath
/// that to the coarsest level's own whole image (`enumerate::smallest_level_whole_image`), for a
/// "no pyramid" image whose sole level does not fit a single tile either.
/// `writer::export_with_progress` only ever writes a manifest in this case once it has confirmed
/// that fallback file is itself within budget (its `manifest_needs_body` flag), so it always names
/// a real file too.
fn largest_whole_image(info: &ImageInfo) -> (String, u64, u64) {
    if let Some(plan) = iiif::level0_sizes(info) {
        if plan.within_budget() {
            let (w, h) = (plan.max_width, plan.max_height);
            return (format!("full/{w},{h}/0/default.jpg"), w, h);
        }
    }
    enumerate_request_space(info)
        .into_iter()
        .filter(|r| r.region_str == "full")
        .map(|r| {
            let (w, h) = match r.size {
                iiif::Size::Wh(w, h) => (w, h),
                _ => (info.width, info.height),
            };
            (r.relative_path(), w, h)
        })
        .max_by_key(|(_, w, h)| w * h)
        .unwrap_or_else(|| {
            let req = smallest_level_whole_image(info);
            let (w, h) = match req.size {
                iiif::Size::Wh(w, h) => (w, h),
                _ => (info.width, info.height),
            };
            (req.relative_path(), w, h)
        })
}

/// Builds `manifest.json` for `plan`. Returns the manifest and any warnings for the person
/// running the export.
pub fn manifest_json(
    plan: &ViewPlan,
    dims: &ImageDimensions,
    info: &ImageInfo,
    root_id: &str,
    name: &str,
) -> (Value, Vec<String>) {
    let ids = Ids::new(root_id);
    let mut warnings = Vec::new();
    if !ids.absolute {
        warnings.push(RELATIVE_MANIFEST_WARNING.to_string());
    }
    // `largest_whole_image` is the whole of this decision: WITHIN budget it names the largest
    // whole image `info.json` itself advertises (`full/{maxWidth},{maxHeight}`), guaranteed to
    // exist by `writer::write_tree`'s level 0 contract; OVER budget it falls back to whatever
    // whole image OpenSeadragon's own request space contains, or the coarsest level's own whole
    // image (`enumerate::smallest_level_whole_image`) when even that is empty — a case
    // `writer::export_with_progress` only reaches once it has confirmed that file is itself within
    // budget. It is computed here, not read back from what `write_tree` did, so a caller building
    // a manifest directly from an `ImageInfo` (as this function's own tests do) gets the same
    // answer without going through a real export at all.
    let (file, file_w, file_h) = largest_whole_image(info);

    let body = |view: &View| -> Value {
        // Every body gets a label, not just overlays: a `Choice` with a labelled overlay beside a
        // blank option is confusing in a real viewer (e.g. Mirador) — "intensity" names the plain
        // image the same way an overlay is named.
        let label = match view.label {
            Some(index) => format!("{} overlay", dims.labels[index].name),
            None => "intensity".to_string(),
        };
        json!({
            "id": ids.in_tree(&view.folder, &file),
            "type": "Image",
            "format": "image/jpeg",
            "width": file_w,
            "height": file_h,
            "label": { "none": [label] },
            "service": [{ "id": ids.tree(&view.folder), "type": "ImageService3", "profile": "level0" }],
        })
    };

    let canvases: Vec<Value> = plan
        .planes
        .iter()
        .map(|&z| {
            let id = ids.path(&format!("canvas/z{z}"));
            let mut on_plane: Vec<Value> =
                plan.views.iter().filter(|v| v.z == z).map(body).collect();
            let painted = if on_plane.len() == 1 {
                on_plane.remove(0)
            } else {
                json!({ "type": "Choice", "items": on_plane })
            };
            json!({
                "id": id,
                "type": "Canvas",
                "label": { "none": [format!("z {z}")] },
                "width": info.width,
                "height": info.height,
                "items": [{
                    "id": format!("{id}/page"),
                    "type": "AnnotationPage",
                    "items": [{
                        "id": format!("{id}/painting"),
                        "type": "Annotation",
                        "motivation": "painting",
                        "body": painted,
                        "target": id,
                    }],
                }],
            })
        })
        .collect();

    // `start` (spec §3.3.2: permitted on Manifest, a JSON object with `id`/`type`) tells a
    // spec-following viewer which canvas to open on. Without it, a viewer such as Mirador always
    // opens canvas 1 (z=0), regardless of what the image's own default plane is. `dims.default_z`
    // is exactly the value `ziv`'s own viewer opens on -- it reads it straight off
    // `ziv/dimensions.json`'s `defaultZ` (`tiling::engine::ImageDimensions::to_json`), which is
    // the very same field, not a second copy of it. Looking the matching canvas up by z in the
    // canvases just built, rather than re-deriving its id string a second time, means `start` can
    // only ever name a canvas that is genuinely in `items` right here, with the SAME id, never an
    // independently-computed guess that could drift from either the viewer or `items`. Emitted
    // even when the default plane is z=0, where it is redundant with a viewer's own "open canvas
    // 1" default: still harmless, and every manifest this crate writes then makes its opening
    // canvas explicit rather than leaving half of them to an unstated convention.
    let start = plan
        .planes
        .iter()
        .zip(&canvases)
        .find(|&(&z, _)| z == dims.default_z)
        .map(|(_, canvas)| json!({ "id": canvas["id"].clone(), "type": "Canvas" }));

    let mut manifest = json!({
        "@context": "http://iiif.io/api/presentation/3/context.json",
        "id": ids.path("manifest.json"),
        "type": "Manifest",
        "label": { "none": [name] },
        "items": canvases,
    });
    // No canvas for the default plane exists (should not happen: `plan_views` always includes
    // `dims.default_z` in `plan.planes`, see its own doc comment) -- omit `start` rather than
    // point it at a canvas this manifest does not have.
    if let Some(start) = start {
        manifest["start"] = start;
    }
    (manifest, warnings)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::views::DEFAULT_OVERLAY_OPACITY;
    use tiling::ImageDimensions;

    /// A single-level, single-tile `ImageInfo` (mirrors `iiif::level0`'s own `info_for` test
    /// helper): just enough for `manifest_json` to compute a body without a real fixture.
    fn info() -> ImageInfo {
        ImageInfo {
            id: ".".to_string(),
            width: 64,
            height: 64,
            tile_size: 512,
            scale_factors: vec![1],
            sizes: vec![(64, 64)],
        }
    }

    fn dims(default_z: u64) -> ImageDimensions {
        ImageDimensions {
            size_t: 1,
            size_z: 3,
            size_c: 1,
            default_t: 0,
            default_z,
            channels: vec![],
            labels: vec![],
            label_open_failures: vec![],
        }
    }

    /// One plain intensity view per plane in `planes`, at the root for `default_z` and under
    /// `planes/{z}` otherwise -- exactly `views::plan_views`' own shape, built by hand so these
    /// tests can give `plan.planes` and `plan.default_z` values a real `plan_views` call could
    /// never produce (see `start_is_omitted_...` below).
    fn plan_for(planes: Vec<u64>, default_z: u64) -> ViewPlan {
        let views = planes
            .iter()
            .map(|&z| View {
                folder: if z == default_z {
                    ".".to_string()
                } else {
                    format!("planes/{z}")
                },
                projection: iiif::ProjectionId::Default,
                z,
                label: None,
            })
            .collect();
        ViewPlan {
            views,
            planes,
            labels: vec![],
            default_z,
            overlay_opacity: DEFAULT_OVERLAY_OPACITY,
            warnings: vec![],
        }
    }

    /// The end-to-end version of this, against the committed `sample_planes_labels.ome.zarr`
    /// fixture (default z = 2 of 4, nowhere near either end of the stack), lives in
    /// `tests/views_export.rs`'s `the_manifest_start_points_at_the_default_planes_canvas`.
    #[test]
    fn start_names_the_default_planes_canvas_which_exists_in_items() {
        let plan = plan_for(vec![0, 1, 2], 1);
        let (manifest, _) = manifest_json(&plan, &dims(1), &info(), ".", "image");
        assert_eq!(manifest["start"]["type"], "Canvas");
        let start_id = manifest["start"]["id"].as_str().unwrap();
        assert_eq!(start_id, "canvas/z1");
        let items = manifest["items"].as_array().unwrap();
        assert!(
            items.iter().any(|c| c["id"] == start_id),
            "start must name a canvas in items: {manifest:#}"
        );
    }

    /// z=0 makes `start` redundant with a viewer's own "open canvas 1" default -- still emitted
    /// rather than special-cased away, so every manifest states its opening canvas explicitly
    /// instead of leaving some of them to an unstated convention.
    #[test]
    fn start_is_emitted_even_when_the_default_plane_is_z_zero() {
        let plan = plan_for(vec![0, 1, 2], 0);
        let (manifest, _) = manifest_json(&plan, &dims(0), &info(), ".", "image");
        assert_eq!(manifest["start"]["type"], "Canvas");
        assert_eq!(manifest["start"]["id"], "canvas/z0");
    }

    /// `manifest_json` takes `plan` and `dims` as separate arguments, so nothing in the type
    /// system stops a caller from passing a `plan` whose `planes` doesn't include `dims`'s
    /// `default_z` (in the one real call site, `writer.rs`, `plan` is always built from that same
    /// `dims` by `plan_views`, which guarantees the opposite -- see its doc comment -- so this
    /// mismatch has to be constructed by hand). `start` must then be omitted entirely, not emitted
    /// pointing at a canvas `items` does not have: naming a missing canvas is worse than naming
    /// none.
    #[test]
    fn start_is_omitted_when_the_default_planes_canvas_is_missing() {
        let plan = plan_for(vec![1, 2], 5);
        let (manifest, _) = manifest_json(&plan, &dims(5), &info(), ".", "image");
        assert!(
            manifest.get("start").is_none(),
            "no canvas for z5 exists, so start must be entirely absent: {manifest:#}"
        );
    }

    /// Pins the whole id-spelling contract `Ids` implements (spec §6.2): relative vs. absolute,
    /// `.` and `""` as equivalent spellings of "no prefix", a bare (non-`http`) relative root, and
    /// trailing-slash trimming. Exercised only incidentally by the end-to-end tests in
    /// `tests/views_export.rs`, which each only ever construct one `Ids` at a time.
    #[test]
    fn ids_new_pins_the_id_spelling_contract() {
        // `.` (the default root): no prefix at all, so `tree(".")` names the root itself.
        let dot = Ids::new(".");
        assert!(!dot.absolute);
        assert_eq!(dot.path("manifest.json"), "manifest.json");
        assert_eq!(dot.tree("."), ".");
        assert_eq!(dot.tree("planes/1"), "planes/1");
        assert_eq!(
            dot.in_tree("planes/1", "full/max/0/default.jpg"),
            "planes/1/full/max/0/default.jpg"
        );
        assert_eq!(
            dot.in_tree(".", "full/max/0/default.jpg"),
            "full/max/0/default.jpg"
        );

        // "" behaves exactly like ".": both mean "no prefix".
        let empty = Ids::new("");
        assert!(!empty.absolute);
        assert_eq!(empty.path("manifest.json"), "manifest.json");
        assert_eq!(empty.tree("."), ".");

        // A bare, non-`http` root is still relative (the warning still fires) but DOES prefix
        // every id, unlike "." or "".
        let bare = Ids::new("img");
        assert!(!bare.absolute);
        assert_eq!(bare.path("manifest.json"), "img/manifest.json");
        assert_eq!(bare.tree("."), "img");
        assert_eq!(bare.tree("planes/1"), "img/planes/1");

        // An absolute `http(s)` root: trailing slash trimmed exactly once, then every id is
        // joined under it.
        let absolute = Ids::new("https://example.org/iiif/foo/");
        assert!(absolute.absolute);
        assert_eq!(
            absolute.path("manifest.json"),
            "https://example.org/iiif/foo/manifest.json"
        );
        assert_eq!(absolute.tree("."), "https://example.org/iiif/foo");
        assert_eq!(
            absolute.tree("planes/1"),
            "https://example.org/iiif/foo/planes/1"
        );
        assert_eq!(
            absolute.in_tree("planes/1", "full/max/0/default.jpg"),
            "https://example.org/iiif/foo/planes/1/full/max/0/default.jpg"
        );
    }
}
