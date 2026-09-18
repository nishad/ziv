//! OME-NGFF label images — the segmentation masks stored alongside an image.
//!
//! An OME-Zarr image may carry a `labels/` group listing named label images. Each of those is a
//! FULL multiscale image in its own right — its own axes, its own resolution pyramid, its own
//! dtype — plus an `image-label` block mapping each integer label value to a colour.
//!
//! Two consequences shape everything that touches them, and both are easy to get wrong:
//!
//! 1. **A label image's pyramid is independent of its parent's.** The IDR sample this was built
//!    against has three levels for the image and four for its labels, so a level index chosen for
//!    one is meaningless for the other.
//! 2. **Label images must be resampled with nearest-neighbour.** The pixel values are object
//!    IDENTIFIERS, not intensities. Averaging label 3 and label 7 yields label 5 — a different
//!    object, invented by the filter. See `tiling::resample`.
use serde_json::{Map, Value};

/// One entry of an `image-label` colour table: the RGBA a given label value is drawn with.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct LabelColor {
    /// The value stored in the label array. Signed because label arrays are commonly `int8`/`int16`
    /// and the spec does not forbid negative values.
    pub value: i64,
    pub rgba: [u8; 4],
}

/// The colour table and metadata of one label image, without its pixels.
#[derive(Debug, Clone, PartialEq)]
pub struct LabelInfo {
    /// The name under `labels/`, which is also the identifier used to request it.
    pub name: String,
    /// Colour per label value, in the order the image declares them.
    pub colors: Vec<LabelColor>,
}

impl LabelInfo {
    /// Looks up the colour for a label value.
    ///
    /// A value with no entry renders fully transparent rather than picking a fallback colour: the
    /// image is asserting which objects it has named, and inventing a colour for an unnamed value
    /// would present noise as a segmented object. `0` is conventionally background and is normally
    /// absent from the table, which is exactly the case this handles.
    #[must_use]
    pub fn color_for(&self, value: i64) -> [u8; 4] {
        self.colors
            .iter()
            .find(|c| c.value == value)
            .map_or([0, 0, 0, 0], |c| c.rgba)
    }
}

/// Reads the `labels` list from a `labels/.zattrs` group: `{"labels": ["0", "nuclei"]}`.
///
/// Returns an empty list rather than an error when the key is missing or malformed — an image
/// without labels is the normal case, not a fault.
#[must_use]
pub fn parse_label_names(attrs: &Map<String, Value>) -> Vec<String> {
    attrs
        .get("labels")
        .and_then(Value::as_array)
        .map(|a| {
            a.iter()
                .filter_map(|v| v.as_str().map(str::to_string))
                .collect()
        })
        .unwrap_or_default()
}

/// Reads the `image-label` colour table from a label image's own attributes.
///
/// Checks the OME-Zarr 0.5 nested location (`ome.image-label`) before the 0.4 top-level one,
/// mirroring how `multiscales` and `omero` are located.
#[must_use]
pub fn parse_image_label(name: &str, attrs: &Map<String, Value>) -> LabelInfo {
    let block = attrs
        .get("ome")
        .and_then(|o| o.get("image-label"))
        .or_else(|| attrs.get("image-label"));

    let colors = block
        .and_then(|b| b.get("colors"))
        .and_then(Value::as_array)
        .map(|arr| {
            arr.iter()
                .filter_map(|c| {
                    let value = c.get("label-value").and_then(Value::as_i64)?;
                    let rgba = c.get("rgba").and_then(Value::as_array)?;
                    let mut out = [0u8; 4];
                    for (slot, v) in out.iter_mut().zip(rgba.iter()) {
                        *slot = u8::try_from(v.as_i64()?.clamp(0, 255)).ok()?;
                    }
                    // A three-element rgb is tolerated as fully opaque; the spec wants four.
                    if rgba.len() == 3 {
                        out[3] = 255;
                    }
                    Some(LabelColor { value, rgba: out })
                })
                .collect()
        })
        .unwrap_or_default();

    LabelInfo {
        name: name.to_string(),
        colors,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn map(v: Value) -> Map<String, Value> {
        v.as_object().unwrap().clone()
    }

    #[test]
    fn reads_the_label_name_list() {
        assert_eq!(
            parse_label_names(&map(json!({"labels": ["0", "nuclei"]}))),
            vec!["0".to_string(), "nuclei".to_string()]
        );
    }

    /// An image with no labels is ordinary, so every malformed or absent shape yields an empty
    /// list rather than failing the open.
    #[test]
    fn missing_or_malformed_label_lists_are_empty_not_errors() {
        assert!(parse_label_names(&map(json!({}))).is_empty());
        assert!(parse_label_names(&map(json!({"labels": "nuclei"}))).is_empty());
        assert!(parse_label_names(&map(json!({"labels": [1, 2]}))).is_empty());
    }

    #[test]
    fn reads_the_colour_table_in_both_metadata_layouts() {
        let v04 = json!({"image-label": {"version": "0.4", "colors": [
            {"label-value": 1, "rgba": [128, 128, 128, 128]},
            {"label-value": 2, "rgba": [255, 0, 0, 255]}
        ]}});
        let v05 = json!({"ome": {"image-label": {"colors": [
            {"label-value": 1, "rgba": [128, 128, 128, 128]},
            {"label-value": 2, "rgba": [255, 0, 0, 255]}
        ]}}});
        for (what, attrs) in [("0.4", v04), ("0.5", v05)] {
            let info = parse_image_label("0", &map(attrs));
            assert_eq!(info.colors.len(), 2, "{what}");
            assert_eq!(info.color_for(1), [128, 128, 128, 128], "{what}");
            assert_eq!(info.color_for(2), [255, 0, 0, 255], "{what}");
        }
    }

    /// An unnamed value must be transparent, not a guessed colour: the image declares which
    /// objects it has segmented, and painting an unlisted value would present noise as an object.
    /// Value 0 is conventionally background and is normally absent from the table.
    #[test]
    fn values_without_an_entry_are_transparent() {
        let info = parse_image_label(
            "0",
            &map(json!({"image-label": {"colors": [{"label-value": 7, "rgba": [1, 2, 3, 4]}]}})),
        );
        assert_eq!(info.color_for(7), [1, 2, 3, 4]);
        assert_eq!(info.color_for(0), [0, 0, 0, 0], "background");
        assert_eq!(info.color_for(99), [0, 0, 0, 0], "unnamed value");
    }

    #[test]
    fn a_three_element_rgb_is_treated_as_opaque() {
        let info = parse_image_label(
            "0",
            &map(json!({"image-label": {"colors": [{"label-value": 1, "rgba": [10, 20, 30]}]}})),
        );
        assert_eq!(info.color_for(1), [10, 20, 30, 255]);
    }

    #[test]
    fn a_label_image_with_no_colour_table_still_parses() {
        let info = parse_image_label("nuclei", &map(json!({"image-label": {"version": "0.4"}})));
        assert_eq!(info.name, "nuclei");
        assert!(info.colors.is_empty());
    }
}
