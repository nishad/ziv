//! Which views a static export writes, and where each one lives.
//!
//! A view is one projection rendered as one complete IIIF Level 0 tree. Deciding the set is kept
//! apart from rendering it, and free of I/O, so every rule in spec §5 and §10 can be tested
//! without writing a single tile.

use iiif::{DynLabel, DynamicProj, ProjectionId};
use tiling::ImageDimensions;

/// The palette every exported overlay uses. Declared colours are left out on purpose (spec §3):
/// in IDR data they are one colour per class, so exporting them doubles the size for a flat render.
pub const OVERLAY_PALETTE: &str = "distinct";

/// The overlay opacity when `--overlay-opacity` is not given. The live viewer opens overlays at
/// the same value, so an export looks like what its author saw in `ziv serve`.
pub const DEFAULT_OVERLAY_OPACITY: f64 = 0.6;

/// What the caller asked to export beyond the default view.
#[derive(Debug, Clone, PartialEq)]
pub struct ViewSelection {
    /// Every z-plane, at the default timepoint and channels.
    pub planes: bool,
    /// An overlay of every addressable label image on each exported plane.
    pub labels: bool,
    /// Opacity for every overlay. Validated by the caller before planning.
    pub overlay_opacity: f64,
}

/// One tree to write.
#[derive(Debug, Clone, PartialEq)]
pub struct View {
    /// Relative to the export root, no trailing slash. `"."` is the root.
    pub folder: String,
    pub projection: ProjectionId,
    pub z: u64,
    /// Index into `ImageDimensions::labels` for an overlay; `None` for plain intensity.
    pub label: Option<usize>,
}

/// Every view an export writes, grouped by plane: each plane's intensity view, then its overlays.
#[derive(Debug, Clone, PartialEq)]
pub struct ViewPlan {
    pub views: Vec<View>,
    /// The exported z values, ascending.
    pub planes: Vec<u64>,
    /// The exported label indices, ascending. May have gaps where a label was skipped.
    pub labels: Vec<usize>,
    pub default_z: u64,
    pub overlay_opacity: f64,
    /// Messages for the person running the export (spec §10). Never fatal.
    pub warnings: Vec<String>,
}

/// Decides the views for `dims` under `selection`.
///
/// The default plane's intensity view is always the root tree (spec D4): the root must stay
/// exactly today's default export, and it is pixel-identical to that plane, so writing it twice
/// would only cost space.
pub fn plan_views(dims: &ImageDimensions, selection: &ViewSelection) -> ViewPlan {
    let mut warnings = Vec::new();

    let planes: Vec<u64> = if selection.planes {
        if dims.size_z <= 1 {
            warnings.push("--planes adds nothing: the image has one z-plane".to_string());
        }
        (0..dims.size_z.max(1)).collect()
    } else {
        vec![dims.default_z]
    };

    let mut labels = Vec::new();
    if selection.labels {
        if dims.labels.is_empty() {
            if dims.label_open_failures.is_empty() {
                warnings.push("--labels adds nothing: the image has no label images".to_string());
            } else {
                // Every declared label failed to open (see `ImageDimensions::label_open_failures`
                // and, upstream, `zarr_core::ZarrImage::label_open_failures`) — this is NOT the
                // same situation as "no label images", and reporting it as such would send an
                // operator looking for a `labels/` group that already exists, rather than at the
                // dtype/metadata problem actually named here.
                let details = dims
                    .label_open_failures
                    .iter()
                    .map(|f| format!("{}: {}", f.name, f.reason))
                    .collect::<Vec<_>>()
                    .join("; ");
                let count = dims.label_open_failures.len();
                let noun = if count == 1 {
                    "label image"
                } else {
                    "label images"
                };
                warnings.push(format!(
                    "--labels adds nothing: the image declares {count} {noun} and none could be opened: {details}"
                ));
            }
        } else {
            // At least one label opened, so `--labels` genuinely adds something — but a sibling
            // that failed must still be named rather than vanishing quietly. See the doc comment
            // on `zarr_core::LabelOpenFailure`.
            for failure in &dims.label_open_failures {
                warnings.push(format!(
                    "label {:?} could not be opened, skipping it: {}",
                    failure.name, failure.reason
                ));
            }
        }
        for (index, label) in dims.labels.iter().enumerate() {
            // `,` and `:` are the identifier grammar's own separators, so a label named with
            // either cannot be asked for, by a server or by this exporter.
            if label.name.contains(',') || label.name.contains(':') {
                warnings.push(format!(
                    "skipping label {:?}: a name containing ',' or ':' cannot be written as an identifier",
                    label.name
                ));
            } else {
                labels.push(index);
            }
        }
    }

    let mut views = Vec::new();
    for &z in &planes {
        views.push(if z == dims.default_z {
            View {
                folder: ".".to_string(),
                projection: ProjectionId::Default,
                z,
                label: None,
            }
        } else {
            View {
                folder: format!("planes/{z}"),
                projection: plane(z, None),
                z,
                label: None,
            }
        });
        for &index in &labels {
            views.push(View {
                folder: format!("planes/{z}/labels/{index}"),
                projection: plane(
                    z,
                    Some(DynLabel {
                        name: dims.labels[index].name.clone(),
                        palette: Some(OVERLAY_PALETTE.to_string()),
                        overlay: true,
                        opacity: Some(selection.overlay_opacity),
                    }),
                ),
                z,
                label: Some(index),
            });
        }
    }

    ViewPlan {
        views,
        planes,
        labels,
        default_z: dims.default_z,
        overlay_opacity: selection.overlay_opacity,
        warnings,
    }
}

/// `@z={z}`, optionally with an overlay: the same projection a server builds from that identifier.
fn plane(z: u64, label: Option<DynLabel>) -> ProjectionId {
    ProjectionId::Dynamic(DynamicProj {
        z: Some(z),
        label,
        ..DynamicProj::default()
    })
}

/// The IIIF `id` for the tree in `folder`, given the export's root id (spec §6.1).
///
/// A relative root (`.`) stays `.` for every tree: each tree's `id` then means "this folder", and
/// the static viewer resolves it against the folder it fetched, not against the page (spec §9.3).
/// Any other root is joined with the folder, so an absolute `--id` gives every tree an absolute,
/// dereferenceable id.
pub fn tree_id(root_id: &str, folder: &str) -> String {
    let root = root_id.trim_end_matches('/');
    let root = if root.is_empty() { "." } else { root };
    if folder == "." || root == "." {
        root.to_string()
    } else {
        format!("{root}/{folder}")
    }
}

/// `ziv/views.json` (spec §5.2): which planes and overlays exist, and the folder of each.
///
/// The static viewer maps its controls to folders through this file alone, so it lists exactly
/// the trees in `plan` and nothing it would have to guess.
pub fn views_json(plan: &ViewPlan, dims: &ImageDimensions) -> serde_json::Value {
    let folder = |z: u64, label: Option<usize>| -> serde_json::Value {
        let view = plan
            .views
            .iter()
            .find(|v| v.z == z && v.label == label)
            .expect("the plan holds a view for every exported plane and label");
        serde_json::Value::String(view.folder.clone())
    };
    let planes: serde_json::Map<String, serde_json::Value> = plan
        .planes
        .iter()
        .map(|&z| (z.to_string(), folder(z, None)))
        .collect();
    let labels: Vec<serde_json::Value> = plan
        .labels
        .iter()
        .map(|&index| {
            let label_planes: serde_json::Map<String, serde_json::Value> = plan
                .planes
                .iter()
                .map(|&z| (z.to_string(), folder(z, Some(index))))
                .collect();
            serde_json::json!({
                "index": index,
                "name": dims.labels[index].name,
                "palette": OVERLAY_PALETTE,
                "opacity": plan.overlay_opacity,
                "planes": label_planes,
            })
        })
        .collect();
    serde_json::json!({
        "version": 1,
        "defaultZ": plan.default_z,
        "planes": planes,
        "labels": labels,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use iiif::parse_identifier;
    use tiling::LabelDescriptor;

    fn dims(size_z: u64, default_z: u64, labels: &[&str]) -> ImageDimensions {
        dims_with_failures(size_z, default_z, labels, &[])
    }

    /// Same as [`dims`], but with declared labels that failed to open — see
    /// `zarr_core::LabelOpenFailure`. `failures` is `(name, reason)` pairs.
    fn dims_with_failures(
        size_z: u64,
        default_z: u64,
        labels: &[&str],
        failures: &[(&str, &str)],
    ) -> ImageDimensions {
        ImageDimensions {
            size_t: 1,
            size_z,
            size_c: 1,
            default_t: 0,
            default_z,
            channels: vec![],
            labels: labels
                .iter()
                .map(|n| LabelDescriptor {
                    name: n.to_string(),
                    declared_colors: 0,
                })
                .collect(),
            label_open_failures: failures
                .iter()
                .map(|(name, reason)| zarr_core::LabelOpenFailure {
                    name: name.to_string(),
                    reason: reason.to_string(),
                })
                .collect(),
        }
    }

    fn select(planes: bool, labels: bool) -> ViewSelection {
        ViewSelection {
            planes,
            labels,
            overlay_opacity: DEFAULT_OVERLAY_OPACITY,
        }
    }

    fn folders(plan: &ViewPlan) -> Vec<&str> {
        plan.views.iter().map(|v| v.folder.as_str()).collect()
    }

    #[test]
    fn with_no_flags_the_plan_is_the_root_alone() {
        let plan = plan_views(&dims(5, 2, &["nuclei"]), &select(false, false));
        assert_eq!(folders(&plan), ["."]);
        assert_eq!(plan.views[0].projection, ProjectionId::Default);
        assert_eq!(plan.planes, [2]);
        assert!(plan.labels.is_empty());
        assert!(plan.warnings.is_empty());
    }

    #[test]
    fn planes_cover_every_z_with_the_default_plane_at_the_root() {
        let plan = plan_views(&dims(5, 2, &[]), &select(true, false));
        assert_eq!(
            folders(&plan),
            ["planes/0", "planes/1", ".", "planes/3", "planes/4"]
        );
        assert_eq!(plan.views[0].projection, parse_identifier("@z=0"));
        assert_eq!(plan.views[2].projection, ProjectionId::Default);
        assert_eq!(plan.planes, [0, 1, 2, 3, 4]);
    }

    #[test]
    fn labels_without_planes_overlay_the_default_plane_only() {
        let plan = plan_views(&dims(5, 2, &["nuclei"]), &select(false, true));
        assert_eq!(folders(&plan), [".", "planes/2/labels/0"]);
        assert_eq!(
            plan.views[1].projection,
            parse_identifier("@z=2,overlay=nuclei:distinct:0.6")
        );
        assert_eq!(plan.views[1].label, Some(0));
    }

    #[test]
    fn planes_and_labels_group_each_plane_with_its_overlays() {
        let plan = plan_views(&dims(3, 1, &["cells", "chromosomes"]), &select(true, true));
        assert_eq!(
            folders(&plan),
            [
                "planes/0",
                "planes/0/labels/0",
                "planes/0/labels/1",
                ".",
                "planes/1/labels/0",
                "planes/1/labels/1",
                "planes/2",
                "planes/2/labels/0",
                "planes/2/labels/1",
            ]
        );
    }

    /// The planes x labels cross product: each label's own `planes` map must name that
    /// label's own per-plane folder, not e.g. every plane's overlay collapsed onto the default
    /// plane's. A `folder(plan.default_z, Some(index))` bug (ignoring `z` and always resolving
    /// to the default plane's overlay) would still pass every other `views_json` test, because
    /// none of them exports more than one plane's worth of the same label; this fixture does
    /// both (3 planes x 2 labels) so such a bug shows up as a wrong value under two of the three
    /// plane keys in each label's map.
    #[test]
    fn views_json_names_each_labels_own_plane_folder() {
        let dims = dims(3, 1, &["cells", "chromosomes"]);
        let plan = plan_views(&dims, &select(true, true));
        assert_eq!(
            views_json(&plan, &dims),
            serde_json::json!({
                "version": 1,
                "defaultZ": 1,
                "planes": {"0": "planes/0", "1": ".", "2": "planes/2"},
                "labels": [
                    {
                        "index": 0, "name": "cells", "palette": "distinct", "opacity": 0.6,
                        "planes": {
                            "0": "planes/0/labels/0",
                            "1": "planes/1/labels/0",
                            "2": "planes/2/labels/0"
                        }
                    },
                    {
                        "index": 1, "name": "chromosomes", "palette": "distinct", "opacity": 0.6,
                        "planes": {
                            "0": "planes/0/labels/1",
                            "1": "planes/1/labels/1",
                            "2": "planes/2/labels/1"
                        }
                    }
                ]
            })
        );
    }

    #[test]
    fn a_single_plane_image_warns_that_planes_add_nothing() {
        let plan = plan_views(&dims(1, 0, &[]), &select(true, false));
        assert_eq!(folders(&plan), ["."]);
        assert_eq!(
            plan.warnings,
            ["--planes adds nothing: the image has one z-plane"]
        );
    }

    #[test]
    fn an_image_without_labels_warns_that_labels_add_nothing() {
        let plan = plan_views(&dims(2, 0, &[]), &select(true, true));
        assert_eq!(folders(&plan), [".", "planes/1"]);
        assert_eq!(
            plan.warnings,
            ["--labels adds nothing: the image has no label images"]
        );
    }

    /// A `labels/.zattrs` that declares a label whose dtype (or other metadata) ziv cannot open
    /// must NOT be reported the same way as "no label images" — that would send an operator
    /// looking for a `labels/` group that already exists on disk. See `zarr_core::ZarrImage`'s
    /// `label_open_failures` and `image.rs`'s `open_labels_local`/`open_labels_remote`, which used
    /// to discard this `Err` silently.
    #[test]
    fn a_label_that_declares_but_cannot_open_is_named_not_reported_as_absent() {
        let dims = dims_with_failures(1, 0, &[], &[("0", "unsupported dtype: int64 / <i8")]);
        let plan = plan_views(&dims, &select(false, true));
        assert_eq!(folders(&plan), ["."]);
        assert_eq!(plan.warnings.len(), 1, "{:?}", plan.warnings);
        let warning = &plan.warnings[0];
        assert!(
            warning.contains("declares 1 label image and none could be opened"),
            "{warning:?}"
        );
        assert!(
            warning.contains("0: unsupported dtype: int64 / <i8"),
            "{warning:?}"
        );
    }

    /// The plural form, pinned separately from the singular case above: the real bug this fix
    /// covers happened to declare exactly one label, so a pluralisation bug in this message
    /// ("declares 1 label images") would have passed that test alone.
    #[test]
    fn several_declared_labels_all_failing_uses_the_plural_noun() {
        let dims = dims_with_failures(
            1,
            0,
            &[],
            &[
                ("a", "unsupported dtype: int64 / <i8"),
                ("b", "no multiscales metadata found in group attributes"),
            ],
        );
        let plan = plan_views(&dims, &select(false, true));
        assert_eq!(plan.warnings.len(), 1, "{:?}", plan.warnings);
        assert!(
            plan.warnings[0].contains("declares 2 label images and none could be opened"),
            "{:?}",
            plan.warnings
        );
    }

    /// A broken label must not take a working sibling down with it: the good one still exports,
    /// and the broken one is named in its own warning rather than silently vanishing from the
    /// count of labels that were considered.
    #[test]
    fn a_broken_label_is_named_but_its_working_sibling_still_exports() {
        let dims = dims_with_failures(
            1,
            0,
            &["nuclei"],
            &[("cells", "unsupported dtype: int64 / <i8")],
        );
        let plan = plan_views(&dims, &select(false, true));
        assert_eq!(folders(&plan), [".", "planes/0/labels/0"]);
        assert_eq!(plan.labels, [0], "the good label must still be exported");
        assert_eq!(plan.warnings.len(), 1, "{:?}", plan.warnings);
        assert!(
            plan.warnings[0].contains("\"cells\""),
            "{:?}",
            plan.warnings
        );
        assert!(
            plan.warnings[0].contains("unsupported dtype: int64 / <i8"),
            "{:?}",
            plan.warnings
        );
    }

    #[test]
    fn an_unaddressable_label_is_skipped_and_keeps_the_others_indices() {
        let plan = plan_views(&dims(1, 0, &["a,b", "cells"]), &select(false, true));
        assert_eq!(folders(&plan), [".", "planes/0/labels/1"]);
        assert_eq!(plan.labels, [1]);
        assert_eq!(plan.warnings.len(), 1);
        assert!(plan.warnings[0].contains("\"a,b\""), "{:?}", plan.warnings);
    }

    #[test]
    fn tree_ids_follow_the_root_id() {
        assert_eq!(tree_id(".", "."), ".");
        assert_eq!(tree_id(".", "planes/3"), ".");
        assert_eq!(tree_id("https://h/p", "."), "https://h/p");
        assert_eq!(tree_id("https://h/p/", "planes/3"), "https://h/p/planes/3");
        assert_eq!(tree_id("img", "planes/3/labels/0"), "img/planes/3/labels/0");
    }
}
