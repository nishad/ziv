//! IIIF Image API 3.0 conformance assertions, runnable as plain `cargo test` (no live server, no
//! network) — the CI-runnable half of the conformance story. These check the SHAPE of a
//! `to_info_json`/`to_info_json_level0` response against the parts of the [Image API 3.0
//! specification](https://iiif.io/api/image/3.0/) that are mechanically checkable from the JSON
//! alone: `@context`/`type`/`protocol`/`profile`/`id` well-formedness, the `tiles`/`sizes`
//! structural shape, and (for `level0`) that OpenSeadragon can actually pin real level sizes from
//! what `sizes` declares, the same reconstruction its `IIIFTileSource` constructor runs (see
//! `crate::level0`).
//!
//! This deliberately does NOT replace the official validator
//! (<https://github.com/IIIF/image-validator>, `pip install iiif-validator`;
//! `iiif-validate.py --version=3.0 --level 2` — it supports 3.0 since v1.0.5, note the explicit
//! `--version=3.0` since it defaults to 2.0) which black-box-tests a LIVE server's actual pixel
//! responses against its own bundled test image. That's a heavyweight manual/live check (see
//! `docs/conformance.md`); this module is the fast, hermetic, always-in-CI layer.
use serde_json::Value;

/// One conformance failure, with a human-readable reason (assertion-message shaped, not an error
/// enum — this exists purely to accumulate multiple failures per `assert_info_json_conforms`
/// call so a single test run reports every violation at once instead of stopping at the first).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ConformanceViolation(pub String);

impl std::fmt::Display for ConformanceViolation {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.0)
    }
}

/// Checks `info` (a serialized `info.json`, level0 or level2) against the IIIF Image API 3.0
/// requirements that are mechanically checkable from the JSON shape alone. Returns every
/// violation found (empty = conforms) rather than stopping at the first, so a caller (typically
/// a test asserting `.is_empty()`) gets a complete report.
///
/// Checked:
/// - `@context` == `"http://iiif.io/api/image/3/context.json"`.
/// - `type` == `"ImageService3"`.
/// - `protocol` == `"http://iiif.io/api/image"`.
/// - `profile` is `"level0"` or `"level2"` (the two profiles this codebase emits).
/// - `id` is a non-empty string with no trailing `/` (IIIF `@id`/`id` must be a dereferenceable
///   URI with no trailing slash — a trailing slash changes the URI, and would produce a
///   double-slash when clients append `/info.json` or a tile path).
/// - `tiles` is a non-empty array of `{width, scaleFactors}` objects, `width` a positive integer,
///   `scaleFactors` a non-empty array of positive integers.
/// - `sizes`, if present, is an array of `{width, height}` objects with positive integers.
/// - For `profile == "level0"`: `sizes` MUST be present and non-empty, and OpenSeadragon MUST
///   rebuild real `levelSizes` from it (`crate::level0::osd_level_sizes`, the same reconstruction
///   `IIIFTileSource`'s constructor runs). The rebuilt array must: have as many entries as
///   `tiles[0].scaleFactors` (matching OSD's own length condition is not enough — see
///   `check_level0_pinning`'s doc comment for the untrimmed, factor-9 counterexample); end at the
///   full `(width, height)` and never exceed it; be non-decreasing on both axes; and have
///   OpenSeadragon's own tile arithmetic for every level agree with what the export writes
///   (`crate::level0::osd_tiling_agrees_with_the_export`). Note this is deliberately NOT a 1:1
///   length check against `sizes` itself: a tree may honestly advertise fewer `sizes` than levels
///   (see `crate::level0`), and the old 1:1 rule this replaced would have rejected exactly that.
///   When `maxWidth`/`maxHeight` are present (as a pair), `sizes` must not advertise anything
///   wider or taller than them.
///
///   KNOWN LIMIT: this document-only check cannot detect a width/height axis swap that keeps each
///   axis internally consistent (a mild-aspect-ratio example is a permanent test below,
///   `a_mild_aspect_ratio_width_height_swap_is_not_caught_by_the_document_only_check`) — a
///   stricter check able to see it would also refuse legitimately-shaped anisotropic pyramids
///   that tile safely. The real guard against that mistake is `crate::level0::level0_sizes`, on
///   the writing path, which compares against the tiling engine's ACTUAL per-level sizes.
pub fn assert_info_json_conforms(info: &Value) -> Vec<ConformanceViolation> {
    let mut v = Vec::new();
    let fail = |msg: String| ConformanceViolation(msg);

    match info.get("@context").and_then(Value::as_str) {
        Some("http://iiif.io/api/image/3/context.json") => {}
        other => v.push(fail(format!(
            "@context must be \"http://iiif.io/api/image/3/context.json\", got {other:?}"
        ))),
    }

    match info.get("type").and_then(Value::as_str) {
        Some("ImageService3") => {}
        other => v.push(fail(format!(
            "type must be \"ImageService3\", got {other:?}"
        ))),
    }

    match info.get("protocol").and_then(Value::as_str) {
        Some("http://iiif.io/api/image") => {}
        other => v.push(fail(format!(
            "protocol must be \"http://iiif.io/api/image\", got {other:?}"
        ))),
    }

    let profile = info.get("profile").and_then(Value::as_str);
    match profile {
        Some("level0") | Some("level2") => {}
        other => v.push(fail(format!(
            "profile must be \"level0\" or \"level2\", got {other:?}"
        ))),
    }

    match info.get("id").and_then(Value::as_str) {
        Some("") => v.push(fail("id must not be empty".to_string())),
        Some(id) if id.ends_with('/') => v.push(fail(format!(
            "id must not have a trailing slash, got {id:?}"
        ))),
        Some(_) => {}
        None => v.push(fail("id must be present and be a string".to_string())),
    }

    check_tiles(info, &mut v);
    let sizes = check_sizes(info, &mut v);

    if profile == Some("level0") {
        check_level0_pinning(info, sizes.as_deref(), &mut v);
    }

    v
}

fn check_tiles(info: &Value, v: &mut Vec<ConformanceViolation>) {
    let Some(tiles) = info.get("tiles").and_then(Value::as_array) else {
        v.push(ConformanceViolation(
            "tiles must be present and be an array".to_string(),
        ));
        return;
    };
    if tiles.is_empty() {
        v.push(ConformanceViolation("tiles must not be empty".to_string()));
        return;
    }
    for (i, t) in tiles.iter().enumerate() {
        match t.get("width").and_then(Value::as_u64) {
            Some(w) if w > 0 => {}
            other => v.push(ConformanceViolation(format!(
                "tiles[{i}].width must be a positive integer, got {other:?}"
            ))),
        }
        match t.get("scaleFactors").and_then(Value::as_array) {
            Some(sf) if !sf.is_empty() => {
                for (j, f) in sf.iter().enumerate() {
                    if !matches!(f.as_u64(), Some(n) if n > 0) {
                        v.push(ConformanceViolation(format!(
                            "tiles[{i}].scaleFactors[{j}] must be a positive integer, got {f:?}"
                        )));
                    }
                }
            }
            other => v.push(ConformanceViolation(format!(
                "tiles[{i}].scaleFactors must be a non-empty array, got {other:?}"
            ))),
        }
    }
}

/// Validates `sizes` (if present) is well-formed and returns it as `(width, height)` pairs for
/// `check_level0_pinning` to reuse, so the array is only walked/parsed once.
fn check_sizes(info: &Value, v: &mut Vec<ConformanceViolation>) -> Option<Vec<(u64, u64)>> {
    let sizes = info.get("sizes")?;
    let Some(arr) = sizes.as_array() else {
        v.push(ConformanceViolation("sizes must be an array".to_string()));
        return None;
    };
    let mut out = Vec::with_capacity(arr.len());
    for (i, s) in arr.iter().enumerate() {
        let w = s.get("width").and_then(Value::as_u64);
        let h = s.get("height").and_then(Value::as_u64);
        match (w, h) {
            (Some(w), Some(h)) if w > 0 && h > 0 => out.push((w, h)),
            _ => v.push(ConformanceViolation(format!(
                "sizes[{i}] must be {{width, height}} with positive integers, got {s:?}"
            ))),
        }
    }
    Some(out)
}

fn check_level0_pinning(
    info: &Value,
    sizes: Option<&[(u64, u64)]>,
    v: &mut Vec<ConformanceViolation>,
) {
    let Some(sizes) = sizes else {
        v.push(ConformanceViolation(
            "level0 profile requires a non-empty sizes array".to_string(),
        ));
        return;
    };
    if sizes.is_empty() {
        v.push(ConformanceViolation(
            "level0 profile requires a non-empty sizes array".to_string(),
        ));
        return;
    }
    let scale_factors: Vec<u64> = info
        .get("tiles")
        .and_then(Value::as_array)
        .and_then(|t| t.first())
        .and_then(|t| t.get("scaleFactors"))
        .and_then(Value::as_array)
        .map(|sf| sf.iter().filter_map(Value::as_u64).collect())
        .unwrap_or_default();
    let (Some(width), Some(height)) = (
        info.get("width").and_then(Value::as_u64),
        info.get("height").and_then(Value::as_u64),
    ) else {
        v.push(ConformanceViolation(
            "level0: width/height must be present integers".to_string(),
        ));
        return;
    };

    // The property that matters is not that `sizes` mirrors `scaleFactors`, but that
    // OpenSeadragon reconstructs REAL level sizes from what is declared. A tree may legitimately
    // advertise fewer sizes than it has levels (see `crate::level0`), and the 1:1 rule this
    // replaced would have rejected exactly that.
    let Some(level_sizes) = crate::level0::osd_level_sizes(sizes, &scale_factors, width, height)
    else {
        v.push(ConformanceViolation(format!(
            "level0: OpenSeadragon will not pin levelSizes from sizes.len() {} against \
             scaleFactors {scale_factors:?}, so it would invent its own level sizes",
            sizes.len()
        )));
        return;
    };

    // The length condition inside `osd_level_sizes` only proves OSD will TRUST `sizes` as
    // `levelSizes` at all (it reads `sizes.len()` against `maxLevel`/`maxLevel + 1`); it does not
    // prove the array OSD rebuilds describes THIS pyramid. An untrimmed 3-level, factor-9 pyramid
    // satisfies that length condition yet rebuilds 4 level sizes (`maxLevel == 3` pushes the full
    // size back on), silently mismatched against 3 declared `scaleFactors`.
    if level_sizes.len() != scale_factors.len() {
        v.push(ConformanceViolation(format!(
            "level0: OpenSeadragon rebuilds {} level sizes from sizes.len() {}, but \
             tiles[0].scaleFactors has {} entries — sizes does not describe this pyramid",
            level_sizes.len(),
            sizes.len(),
            scale_factors.len()
        )));
        return;
    }
    if level_sizes.last() != Some(&(width, height)) {
        v.push(ConformanceViolation(format!(
            "level0: reconstructed level sizes {level_sizes:?} must end at the full image \
             ({width}, {height})"
        )));
    }
    for (w, h) in &level_sizes {
        if *w > width || *h > height {
            v.push(ConformanceViolation(format!(
                "level0: reconstructed level size ({w}, {h}) exceeds the image ({width}, {height})"
            )));
        }
    }

    // Reconstruction can match on ARRAY SHAPE (length, sort order, endpoint) while still being
    // geometric nonsense: width and height are sorted and validated independently by OSD, so a
    // swapped-axis or anisotropic-but-tied-width pyramid can slip through the checks above. Two
    // further properties close that gap, both enforced by `osd_tiling_agrees_with_the_export`
    // below rather than here, so the emitter (`crate::level0::level0_sizes`) and this assertion
    // share exactly one rule and cannot drift apart again: the rebuilt sizes must be
    // non-decreasing on BOTH axes (an anisotropic pyramid whose width bottoms out, like spec D7's,
    // sorts by width alone and can produce a non-monotone height sequence), and every rebuilt
    // level's implied scale on BOTH axes must match OpenSeadragon's own `2^(maxLevel-level)` (an
    // x-only downsampled pyramid reconstructs a monotone, in-bounds array whose height axis never
    // actually scales). A per-axis "implied ratio must equal a nominal 2^level" test would be
    // STRICTER than OpenSeadragon itself: real pyramids drift from an exact power-of-two ratio at
    // levels small enough to always land in a single tile, where OpenSeadragon never evaluates any
    // per-tile region arithmetic at all (see that function's doc comment for the regressions this
    // avoids). `tile_size` absent from a level0 document is already flagged by `check_tiles`
    // (`tiles[0].width must be a positive integer`), so skipping this check in that case does not
    // let a malformed document through silently.
    let tile_size = info
        .get("tiles")
        .and_then(Value::as_array)
        .and_then(|t| t.first())
        .and_then(|t| t.get("width"))
        .and_then(Value::as_u64);
    if let Some(tile_size) = tile_size {
        if !crate::level0::osd_tiling_agrees_with_the_export(
            &level_sizes,
            &scale_factors,
            width,
            height,
            tile_size,
        ) {
            v.push(ConformanceViolation(format!(
                "level0: OpenSeadragon's own tile-region arithmetic for the reconstructed level \
                 sizes {level_sizes:?} does not match what this export would write (tile width \
                 {tile_size}, scaleFactors {scale_factors:?}) — this also covers geometric \
                 shape checks (non-decreasing on both axes) that array-shape matching alone \
                 cannot see"
            )));
        }
    }

    let max_w = info.get("maxWidth").and_then(Value::as_u64);
    let max_h = info.get("maxHeight").and_then(Value::as_u64);
    match (max_w, max_h) {
        (Some(max_w), Some(max_h)) => {
            for &(w, h) in sizes {
                if w > max_w {
                    v.push(ConformanceViolation(format!(
                        "level0: sizes advertises ({w}, {h}), wider than maxWidth {max_w}"
                    )));
                }
                if h > max_h {
                    v.push(ConformanceViolation(format!(
                        "level0: sizes advertises ({w}, {h}), taller than maxHeight {max_h}"
                    )));
                }
            }
        }
        (Some(_), None) | (None, Some(_)) => {
            v.push(ConformanceViolation(
                "level0: maxWidth and maxHeight must both be present or both absent".to_string(),
            ));
        }
        (None, None) => {}
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ImageInfo;

    fn level2() -> Value {
        ImageInfo {
            id: "https://host/iiif/img".into(),
            width: 1024,
            height: 768,
            tile_size: 512,
            scale_factors: vec![1, 2, 4],
            sizes: vec![(1024, 768), (512, 384), (256, 192)],
        }
        .to_info_json(2)
    }

    fn level0() -> Value {
        ImageInfo {
            id: "https://host/export/root".into(),
            width: 1024,
            height: 768,
            tile_size: 256,
            scale_factors: vec![1, 2, 4],
            sizes: vec![(1024, 768), (512, 384), (256, 192)],
        }
        .to_info_json_level0()
    }

    #[test]
    fn conformant_level2_info_json_has_no_violations() {
        assert_eq!(assert_info_json_conforms(&level2()), Vec::new());
    }

    #[test]
    fn conformant_level0_info_json_has_no_violations() {
        assert_eq!(assert_info_json_conforms(&level0()), Vec::new());
    }

    #[test]
    fn rejects_wrong_context() {
        let mut v = level2();
        v["@context"] = Value::String("http://iiif.io/api/image/2/context.json".into());
        let violations = assert_info_json_conforms(&v);
        assert!(violations.iter().any(|c| c.0.contains("@context")));
    }

    #[test]
    fn rejects_trailing_slash_id() {
        let mut v = level2();
        v["id"] = Value::String("https://host/iiif/img/".into());
        let violations = assert_info_json_conforms(&v);
        assert!(violations.iter().any(|c| c.0.contains("trailing slash")));
    }

    #[test]
    fn rejects_missing_tiles() {
        let mut v = level2();
        v.as_object_mut().unwrap().remove("tiles");
        let violations = assert_info_json_conforms(&v);
        assert!(violations.iter().any(|c| c.0.contains("tiles")));
    }

    /// The property that matters is whether OpenSeadragon can PIN level sizes from `sizes`, not
    /// whether `sizes` mirrors `scaleFactors` 1:1 — the old rule this replaced would have rejected
    /// an honestly trimmed tree. Here `sizes` is trimmed to a length (1) that matches neither
    /// `maxLevel` (2) nor `maxLevel + 1` (3), so OSD would invent its own level sizes instead.
    #[test]
    fn rejects_level0_sizes_openseadragon_cannot_pin() {
        let mut v = level0();
        v["sizes"].as_array_mut().unwrap().pop();
        let violations = assert_info_json_conforms(&v);
        assert!(violations
            .iter()
            .any(|c| c.0.contains("will not pin levelSizes")));
    }

    /// `maxWidth` is a promise about what an export wrote; `sizes` must not advertise anything
    /// wider than that promise.
    #[test]
    fn rejects_level0_sizes_wider_than_max_width() {
        let mut v = level0();
        v["maxWidth"] = Value::from(100u64);
        let violations = assert_info_json_conforms(&v);
        assert!(violations
            .iter()
            .any(|c| c.0.contains("wider than maxWidth")));
    }

    /// A minimal level0-shaped document built directly from field values, independent of whether
    /// `ImageInfo`'s own construction (which now refuses malformed pyramids, see `crate::level0`)
    /// would ever produce it. `check_level0_pinning` must reject a bad document on its own, not
    /// merely rely on the emitter never handing it one.
    fn level0_shaped(
        width: u64,
        height: u64,
        sizes: &[(u64, u64)],
        scale_factors: &[u64],
    ) -> Value {
        serde_json::json!({
            "@context": "http://iiif.io/api/image/3/context.json",
            "id": "https://host/export/root",
            "type": "ImageService3",
            "protocol": "http://iiif.io/api/image",
            "profile": "level0",
            "width": width,
            "height": height,
            "tiles": [{ "width": 256, "scaleFactors": scale_factors }],
            "sizes": sizes.iter().map(|&(w, h)| serde_json::json!({"width": w, "height": h})).collect::<Vec<_>>(),
            "extraFeatures": ["sizeByWh"],
        })
    }

    /// The brief's own unpinnable pyramid (`maxLevel = round(log2(9)) == 3`), but UNTRIMMED (all
    /// 3 sizes present, matching `scaleFactors.len()`). OSD's `n == maxLevel` arm still fires (3
    /// == 3) and pushes the full size back on, rebuilding 4 level sizes for a document that
    /// declares 3 `scaleFactors` — the length condition alone (proving OSD will READ `sizes`)
    /// does not prove it rebuilds THIS pyramid.
    #[test]
    fn rejects_level0_untrimmed_pyramid_openseadragon_cannot_pin() {
        let v = level0_shaped(900, 900, &[(900, 900), (300, 300), (100, 100)], &[1, 3, 9]);
        let violations = assert_info_json_conforms(&v);
        assert!(!violations.is_empty(), "expected a violation, got none");
    }

    /// Width and height swapped in an otherwise-plausible trimmed document, for a strongly
    /// anisotropic (very wide, short) image. Deliberately anisotropic: on a mild aspect ratio a
    /// width/height swap can reconstruct an array that still ends at the full image, never
    /// exceeds it, AND stays non-decreasing on both axes (that swap is geometrically wrong but
    /// not one of the properties this check enforces). Here the true pyramid's height only ever
    /// grows towards the full image; swapping the intermediate entries' axes drops the height
    /// back down just before the pushed full-resolution entry, breaking monotonicity for real.
    #[test]
    fn rejects_level0_sizes_with_width_and_height_swapped() {
        let v = level0_shaped(2000, 100, &[(50, 1000), (25, 500)], &[1, 2, 4]);
        let violations = assert_info_json_conforms(&v);
        assert!(!violations.is_empty(), "expected a violation, got none");
    }

    /// The same swap, on a MILD aspect ratio: `check_level0_pinning` honestly does NOT catch this
    /// today, and this test documents that limit rather than hiding it. The reconstructed array
    /// ends at the full image, never exceeds it, stays non-decreasing on both axes, and (per
    /// `crate::level0::osd_tiling_agrees_with_the_export`) OpenSeadragon's tile arithmetic never
    /// looks at width/height as a PAIR, only at each axis's own numbers against the declared
    /// factor — so a swap that keeps each axis internally consistent is, from a document alone,
    /// indistinguishable from a legitimately-shaped anisotropic pyramid that tiles perfectly
    /// safely. A stricter check able to see this swap would over-refuse exactly those legitimate
    /// pyramids (a design ruling, not a measured result). The real guard against this class of mistake
    /// is `level0_sizes`, on the writing path, which compares against the tiling engine's ACTUAL
    /// per-level sizes rather than only the declared document — a swap cannot reach it, because it
    /// would have to already be present in the engine's own truth to be reconstructed at all.
    #[test]
    fn a_mild_aspect_ratio_width_height_swap_is_not_caught_by_the_document_only_check() {
        let v = level0_shaped(1024, 768, &[(384, 512), (192, 256)], &[1, 2, 4]);
        let violations = assert_info_json_conforms(&v);
        assert_eq!(
            violations,
            Vec::new(),
            "documents check_level0_pinning's real, current limit — if this now finds a \
             violation, update this test to describe what changed, do not just re-assert failure"
        );
    }

    /// Spec D7's anisotropic pyramid, untrimmed: width bottoms out at 1 while height keeps
    /// halving, so OSD's width-only sort produces a non-monotone height sequence.
    #[test]
    fn rejects_level0_anisotropic_sizes_that_are_not_monotone() {
        let v = level0_shaped(
            8,
            64,
            &[(8, 64), (4, 32), (2, 16), (1, 8), (1, 4)],
            &[1, 2, 4, 8, 16],
        );
        let violations = assert_info_json_conforms(&v);
        assert!(!violations.is_empty(), "expected a violation, got none");
    }

    /// An x-only downsampled pyramid: width halves but height never does. The rebuilt array is
    /// monotone and ends at the full image, so only the per-level scale check (checking BOTH
    /// axes against `2^(maxLevel-e)`) catches that OSD would request a y-range this image doesn't
    /// have.
    #[test]
    fn rejects_level0_x_only_downsampled_pyramid() {
        let v = level0_shaped(1024, 1000, &[(1024, 1000), (512, 1000)], &[1, 2]);
        let violations = assert_info_json_conforms(&v);
        assert!(!violations.is_empty(), "expected a violation, got none");
    }

    /// All 3 levels declared explicitly (`n == maxLevel + 1`, so OSD does not push), but the
    /// largest one does not equal the declared full image.
    #[test]
    fn rejects_level0_sizes_not_ending_at_full_image() {
        let v = level0_shaped(1024, 768, &[(256, 192), (512, 384), (999, 768)], &[1, 2, 4]);
        let violations = assert_info_json_conforms(&v);
        assert!(violations
            .iter()
            .any(|c| c.0.contains("must end at the full image")));
    }

    /// A reconstructed level size wider than the declared full image.
    #[test]
    fn rejects_level0_sizes_exceeding_the_image() {
        let v = level0_shaped(1024, 768, &[(256, 192), (2000, 384)], &[1, 2, 4]);
        let violations = assert_info_json_conforms(&v);
        assert!(violations.iter().any(|c| c.0.contains("exceeds the image")));
    }

    /// `maxHeight` is a promise about what an export wrote, exactly like `maxWidth`; nothing
    /// advertised may be taller than it.
    #[test]
    fn rejects_level0_sizes_taller_than_max_height() {
        let mut v = level0();
        v["maxHeight"] = Value::from(10u64);
        let violations = assert_info_json_conforms(&v);
        assert!(violations
            .iter()
            .any(|c| c.0.contains("taller than maxHeight")));
    }

    /// `maxWidth` and `maxHeight` are emitted as a pair (see `ImageInfo::to_info_json_level0`);
    /// one present without the other is itself a malformed document.
    #[test]
    fn rejects_level0_max_width_without_max_height() {
        let mut v = level0();
        v.as_object_mut().unwrap().remove("maxHeight");
        let violations = assert_info_json_conforms(&v);
        assert!(violations
            .iter()
            .any(|c| c.0.contains("must both be present or both absent")));
    }

    /// The mirror of the above: a lone `maxHeight` without `maxWidth` is equally malformed.
    #[test]
    fn rejects_level0_max_height_without_max_width() {
        let mut v = level0();
        v.as_object_mut().unwrap().remove("maxWidth");
        let violations = assert_info_json_conforms(&v);
        assert!(violations
            .iter()
            .any(|c| c.0.contains("must both be present or both absent")));
    }

    #[test]
    fn rejects_level0_missing_sizes() {
        let mut v = level0();
        v.as_object_mut().unwrap().remove("sizes");
        let violations = assert_info_json_conforms(&v);
        assert!(violations
            .iter()
            .any(|c| c.0.contains("non-empty sizes array")));
    }

    #[test]
    fn rejects_bad_profile() {
        let mut v = level2();
        v["profile"] = Value::String("level1".into());
        let violations = assert_info_json_conforms(&v);
        assert!(violations.iter().any(|c| c.0.contains("profile")));
    }
}
