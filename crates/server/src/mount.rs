//! Splitting a request path under the `/i/` mount into an image name and a sub-route.
//!
//! `/i/{name…}/{marker}/{tail}` has a variable-length name in the MIDDLE of the path, which axum
//! cannot express as a declarative route. So the router registers one wildcard, `/i/{*rest}`, and
//! this module does the split.
//!
//! # Why the LAST marker, not the first
//!
//! A caller-named remote store puts a whole store path inside the image name, and a store path may
//! legitimately contain a segment called `iiif`. Splitting on the first occurrence would tear such
//! a name in half. Splitting on the last occurrence is correct for every name, including one that
//! ends in a marker, and moves the residual risk somewhere far easier to control: the only way it
//! can go wrong is if a path THIS SERVER generates after a marker itself contains a marker
//! segment. The only generated paths are viewer asset paths, and
//! `viewer::no_embedded_asset_path_contains_a_marker_segment` pins that they do not.
use std::fmt;

/// Segments that terminate an image name and select a sub-route.
pub const MARKER_SEGMENTS: [&str; 3] = ["iiif", "ziv", "viewer"];

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Marker {
    Iiif,
    Ziv,
    Viewer,
}

impl Marker {
    fn from_segment(s: &str) -> Option<Self> {
        match s {
            "iiif" => Some(Marker::Iiif),
            "ziv" => Some(Marker::Ziv),
            "viewer" => Some(Marker::Viewer),
            _ => None,
        }
    }
}

impl fmt::Display for Marker {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Marker::Iiif => "iiif",
            Marker::Ziv => "ziv",
            Marker::Viewer => "viewer",
        })
    }
}

/// A request path under `/i/`, split into its parts. Borrows from the original path, so the tile
/// hot path allocates nothing to work out which image it is for.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Mount<'a> {
    /// Everything before the marker. Not yet validated as an `ImageName`.
    pub name: &'a str,
    pub marker: Marker,
    /// Everything after the marker, with no leading or trailing slash. May be empty.
    pub tail: &'a str,
}

/// Splits the wildcard capture of `/i/{*rest}`.
///
/// Returns `None` when there is no marker segment at all, or when the marker is the first segment
/// (which would mean an empty image name). Both are 404s at the call site.
#[must_use]
pub fn split_mount(rest: &str) -> Option<Mount<'_>> {
    let mut found: Option<(usize, usize, Marker)> = None;
    let mut offset = 0usize;
    for segment in rest.split('/') {
        if let Some(marker) = Marker::from_segment(segment) {
            found = Some((offset, offset + segment.len(), marker));
        }
        // +1 for the '/' that followed this segment. Past the final segment this overshoots by
        // one, which is harmless: the loop is over.
        offset += segment.len() + 1;
    }
    let (start, end, marker) = found?;
    if start == 0 {
        return None;
    }
    Some(Mount {
        // `start - 1` drops the '/' that precedes the marker.
        name: &rest[..start - 1],
        marker,
        tail: rest.get(end + 1..).unwrap_or("").trim_end_matches('/'),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn split(s: &str) -> (String, Marker, String) {
        let m = split_mount(s).unwrap_or_else(|| panic!("{s} should split"));
        (m.name.to_string(), m.marker, m.tail.to_string())
    }

    #[test]
    fn splits_a_simple_name() {
        assert_eq!(
            split("nuclei/iiif/default/full/max/0/default.jpg"),
            (
                "nuclei".into(),
                Marker::Iiif,
                "default/full/max/0/default.jpg".into()
            )
        );
    }

    #[test]
    fn splits_a_nested_name() {
        assert_eq!(
            split("idr0062A/6001240/ziv/dimensions.json"),
            (
                "idr0062A/6001240".into(),
                Marker::Ziv,
                "dimensions.json".into()
            )
        );
    }

    #[test]
    fn an_empty_tail_is_valid() {
        assert_eq!(
            split("nuclei/viewer"),
            ("nuclei".into(), Marker::Viewer, String::new())
        );
        assert_eq!(
            split("nuclei/viewer/"),
            ("nuclei".into(), Marker::Viewer, String::new())
        );
    }

    /// The reason for splitting on the LAST occurrence rather than the first. A caller-named
    /// remote store (a later phase) puts a store path inside the name, and a store path may
    /// legitimately contain a segment called `iiif`.
    #[test]
    fn a_name_containing_a_marker_still_splits_correctly() {
        assert_eq!(
            split(
                "remote/https/example.org/iiif/data/img.zarr/iiif/default/full/max/0/default.jpg"
            ),
            (
                "remote/https/example.org/iiif/data/img.zarr".into(),
                Marker::Iiif,
                "default/full/max/0/default.jpg".into()
            )
        );
    }

    #[test]
    fn a_name_ending_in_a_marker_still_splits_correctly() {
        assert_eq!(
            split("a/iiif/iiif/default/info.json"),
            ("a/iiif".into(), Marker::Iiif, "default/info.json".into())
        );
    }

    #[test]
    fn rejects_a_path_with_no_marker() {
        assert!(split_mount("nuclei/full/max").is_none());
        assert!(split_mount("").is_none());
    }

    /// An empty image name is not a name. `/i/iiif/...` must 404, not resolve to "".
    #[test]
    fn rejects_an_empty_name() {
        assert!(split_mount("iiif/default/info.json").is_none());
        assert!(split_mount("viewer/").is_none());
    }
}
