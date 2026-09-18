use thiserror::Error;

#[derive(Debug, Error, PartialEq)]
pub enum IiifError {
    #[error("bad region: {0}")]
    BadRegion(String),
    #[error("bad size: {0}")]
    BadSize(String),
    #[error("bad rotation: {0} (supported: 0, 90, 180, 270)")]
    BadRotation(String),
    #[error("bad quality: {0} (supported: default, color, gray, bitonal)")]
    BadQuality(String),
    #[error("bad format: {0} (supported: jpg, png)")]
    BadFormat(String),
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Region {
    Full,
    Square,
    Px { x: u64, y: u64, w: u64, h: u64 },
    Pct { x: f64, y: f64, w: f64, h: f64 },
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Size {
    Max,
    Width(u64),
    Height(u64),
    Wh(u64, u64),
    /// `!w,h` — scale to the LARGEST size that fits inside the `w x h` box while preserving the
    /// region's aspect ratio (IIIF `sizeByConfinedWh`, required at level 2). Distinct from
    /// `Wh`, which distorts to exactly `w x h`.
    Confined(u64, u64),
    Pct(f64),
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Format {
    Jpg,
    Png,
}

impl Format {
    /// The `Content-Type` to serve this format as.
    pub fn media_type(self) -> &'static str {
        match self {
            Format::Jpg => "image/jpeg",
            Format::Png => "image/png",
        }
    }

    /// The canonical extension, as it appears in the request path.
    pub fn extension(self) -> &'static str {
        match self {
            Format::Jpg => "jpg",
            Format::Png => "png",
        }
    }
}

/// The IIIF `quality` parameter. Level 2 requires `default`, `color` and `gray`; `bitonal` is
/// optional but trivial once `gray` exists, so it is supported too.
///
/// Anything else is REJECTED rather than ignored. Accepting an unrecognized quality and silently
/// rendering the default is indistinguishable, to a client, from the server honouring it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Quality {
    Default,
    Color,
    Gray,
    Bitonal,
}

impl Quality {
    /// The canonical spelling, as it appears in the request path.
    pub fn as_str(self) -> &'static str {
        match self {
            Quality::Default => "default",
            Quality::Color => "color",
            Quality::Gray => "gray",
            Quality::Bitonal => "bitonal",
        }
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct ImageRequest {
    pub region: Region,
    pub size: Size,
    pub rotation: u16,
    pub quality: Quality,
    pub format: Format,
}

/// Parses a IIIF Image API 3.0 `region` path segment.
///
/// ```
/// use iiif::{parse_region, Region};
///
/// assert_eq!(parse_region("full").unwrap(), Region::Full);
/// assert_eq!(parse_region("square").unwrap(), Region::Square);
/// assert_eq!(
///     parse_region("10,20,300,400").unwrap(),
///     Region::Px { x: 10, y: 20, w: 300, h: 400 }
/// );
/// assert_eq!(parse_region("pct:0,0,50,50").unwrap(), Region::Pct { x: 0.0, y: 0.0, w: 50.0, h: 50.0 });
///
/// // Malformed input is rejected, never silently coerced — this parser sits directly on
/// // untrusted HTTP path segments.
/// assert!(parse_region("10,20,300").is_err());
/// ```
pub fn parse_region(s: &str) -> Result<Region, IiifError> {
    match s {
        "full" => Ok(Region::Full),
        "square" => Ok(Region::Square),
        _ => {
            if let Some(rest) = s.strip_prefix("pct:") {
                let n: Vec<f64> = rest
                    .split(',')
                    .map(|p| p.parse().map_err(|_| IiifError::BadRegion(s.into())))
                    .collect::<Result<_, _>>()?;
                if n.len() != 4 {
                    return Err(IiifError::BadRegion(s.into()));
                }
                return Ok(Region::Pct {
                    x: n[0],
                    y: n[1],
                    w: n[2],
                    h: n[3],
                });
            }
            let n: Vec<u64> = s
                .split(',')
                .map(|p| p.parse().map_err(|_| IiifError::BadRegion(s.into())))
                .collect::<Result<_, _>>()?;
            if n.len() != 4 {
                return Err(IiifError::BadRegion(s.into()));
            }
            Ok(Region::Px {
                x: n[0],
                y: n[1],
                w: n[2],
                h: n[3],
            })
        }
    }
}

/// Parses a IIIF Image API 3.0 `size` path segment.
///
/// ```
/// use iiif::{parse_size, Size};
///
/// assert_eq!(parse_size("max").unwrap(), Size::Max);
/// assert_eq!(parse_size("512,").unwrap(), Size::Width(512));
/// assert_eq!(parse_size(",512").unwrap(), Size::Height(512));
/// assert_eq!(parse_size("512,512").unwrap(), Size::Wh(512, 512));
/// assert_eq!(parse_size("!512,512").unwrap(), Size::Confined(512, 512));
/// assert_eq!(parse_size("pct:25").unwrap(), Size::Pct(25.0));
///
/// assert!(parse_size("not-a-size").is_err());
/// ```
pub fn parse_size(s: &str) -> Result<Size, IiifError> {
    // `^size` requests UPSCALING (`sizeUpscaling`), an optional feature ziv does not implement.
    // Stripping the marker and serving the request anyway would answer 200 to something the
    // server cannot actually do, so it is refused at the edge instead.
    if s.starts_with('^') {
        return Err(IiifError::BadSize(s.into()));
    }
    match s {
        "max" => Ok(Size::Max),
        _ => {
            // `!w,h` (sizeByConfinedWh): fit inside the box, preserving aspect ratio.
            if let Some(rest) = s.strip_prefix('!') {
                let (w, h) = rest
                    .split_once(',')
                    .ok_or_else(|| IiifError::BadSize(s.into()))?;
                return Ok(Size::Confined(
                    w.parse().map_err(|_| IiifError::BadSize(s.into()))?,
                    h.parse().map_err(|_| IiifError::BadSize(s.into()))?,
                ));
            }
            if let Some(rest) = s.strip_prefix("pct:") {
                return rest
                    .parse::<f64>()
                    .map(Size::Pct)
                    .map_err(|_| IiifError::BadSize(s.into()));
            }
            match s.split_once(',') {
                Some(("", h)) => h
                    .parse()
                    .map(Size::Height)
                    .map_err(|_| IiifError::BadSize(s.into())),
                Some((w, "")) => w
                    .parse()
                    .map(Size::Width)
                    .map_err(|_| IiifError::BadSize(s.into())),
                Some((w, h)) => Ok(Size::Wh(
                    w.parse().map_err(|_| IiifError::BadSize(s.into()))?,
                    h.parse().map_err(|_| IiifError::BadSize(s.into()))?,
                )),
                None => Err(IiifError::BadSize(s.into())),
            }
        }
    }
}

/// Parses the `{quality}.{format}` path segment.
///
/// ```
/// use iiif::{parse_quality_format, Format, Quality};
///
/// assert_eq!(parse_quality_format("default.jpg").unwrap(), (Quality::Default, Format::Jpg));
/// assert_eq!(parse_quality_format("gray.png").unwrap(), (Quality::Gray, Format::Png));
///
/// // An unrecognized quality is an error, not a silent fall-back to `default`.
/// assert!(parse_quality_format("sharpen.jpg").is_err());
/// assert!(parse_quality_format("default.webp").is_err());
/// ```
pub fn parse_quality_format(s: &str) -> Result<(Quality, Format), IiifError> {
    let (quality, ext) = s
        .rsplit_once('.')
        .ok_or_else(|| IiifError::BadFormat(s.into()))?;
    let format = match ext {
        "jpg" | "jpeg" => Format::Jpg,
        "png" => Format::Png,
        _ => return Err(IiifError::BadFormat(ext.into())),
    };
    let quality = match quality {
        "default" => Quality::Default,
        "color" => Quality::Color,
        "gray" | "grey" => Quality::Gray,
        "bitonal" => Quality::Bitonal,
        _ => return Err(IiifError::BadQuality(quality.into())),
    };
    Ok((quality, format))
}

impl ImageRequest {
    pub fn parse(
        region: &str,
        size: &str,
        rotation: &str,
        quality_dot_format: &str,
    ) -> Result<ImageRequest, IiifError> {
        // `rotationBy90s` is required at level 2. `rotationArbitrary` and mirroring (`!n`) are
        // optional and not supported, so anything other than the four right angles is rejected —
        // including `0.0`-style decimals, which would otherwise parse to an accepted value while
        // meaning something the server does not actually implement.
        let rot: u16 = rotation
            .parse()
            .map_err(|_| IiifError::BadRotation(rotation.into()))?;
        if !matches!(rot, 0 | 90 | 180 | 270) {
            return Err(IiifError::BadRotation(rotation.into()));
        }
        let (quality, format) = parse_quality_format(quality_dot_format)?;
        Ok(ImageRequest {
            region: parse_region(region)?,
            size: parse_size(size)?,
            rotation: rot,
            quality,
            format,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_full_max_default_jpg() {
        let r = ImageRequest::parse("full", "max", "0", "default.jpg").unwrap();
        assert_eq!(r.region, Region::Full);
        assert_eq!(r.size, Size::Max);
        assert_eq!(r.quality, Quality::Default);
        assert_eq!(r.format, Format::Jpg);
    }

    #[test]
    fn parses_px_region_and_wh() {
        let r = ImageRequest::parse("256,256,512,512", "512,512", "0", "default.jpg").unwrap();
        assert_eq!(
            r.region,
            Region::Px {
                x: 256,
                y: 256,
                w: 512,
                h: 512
            }
        );
        assert_eq!(r.size, Size::Wh(512, 512));
    }

    #[test]
    fn parses_width_only_size() {
        assert_eq!(parse_size("256,").unwrap(), Size::Width(256));
        assert_eq!(parse_size(",256").unwrap(), Size::Height(256));
    }

    /// `rotationBy90s` is required at IIIF 3.0 level 2, so the four right angles must parse.
    #[test]
    fn accepts_the_four_right_angle_rotations() {
        for rot in ["0", "90", "180", "270"] {
            let r = ImageRequest::parse("full", "max", rot, "default.jpg")
                .unwrap_or_else(|e| panic!("rotation {rot} must be accepted: {e}"));
            assert_eq!(r.rotation, rot.parse::<u16>().unwrap());
        }
    }

    /// `rotationArbitrary` and mirroring (`!n`) are OPTIONAL features ziv does not implement, so
    /// they must be refused rather than silently rendered unrotated.
    #[test]
    fn rejects_arbitrary_rotation_and_mirroring() {
        for rot in ["45", "1", "359", "!90", "-90", "0.0", "360"] {
            assert!(
                matches!(
                    ImageRequest::parse("full", "max", rot, "default.jpg"),
                    Err(IiifError::BadRotation(_))
                ),
                "rotation {rot} must be rejected"
            );
        }
    }

    /// jpg and png are both required at level 2; everything else is optional and unimplemented.
    #[test]
    fn accepts_required_formats_and_rejects_the_rest() {
        assert_eq!(parse_quality_format("default.jpg").unwrap().1, Format::Jpg);
        assert_eq!(parse_quality_format("default.png").unwrap().1, Format::Png);
        for ext in ["webp", "tif", "gif", "pdf", "jp2"] {
            assert!(
                matches!(
                    parse_quality_format(&format!("default.{ext}")),
                    Err(IiifError::BadFormat(_))
                ),
                "format {ext} must be rejected"
            );
        }
    }

    /// An unrecognized quality must be an ERROR. Previously any string was accepted and then
    /// ignored, which a client cannot distinguish from the server honouring it.
    #[test]
    fn rejects_unknown_quality() {
        for q in ["sharpen", "oUusK8", "", "colour"] {
            assert!(
                matches!(
                    parse_quality_format(&format!("{q}.jpg")),
                    Err(IiifError::BadQuality(_))
                ),
                "quality {q:?} must be rejected"
            );
        }
    }

    #[test]
    fn parses_confined_size() {
        assert_eq!(parse_size("!643,496").unwrap(), Size::Confined(643, 496));
        assert!(parse_size("!643").is_err());
        assert!(parse_size("!a,b").is_err());
    }
}

/// Canonical string formatting for the request grammar's `Region`/`Size`, used only by the
/// property tests below to build round-trip fixtures (canonical `Region`/`Size` -> string ->
/// `Region`/`Size`) — production code never needs to re-serialize a parsed request, only parse
/// one, so this lives in `#[cfg(test)]` rather than the public API.
#[cfg(test)]
mod canonical {
    use super::{Region, Size};

    pub fn region_to_string(r: &Region) -> String {
        match r {
            Region::Full => "full".to_string(),
            Region::Square => "square".to_string(),
            Region::Px { x, y, w, h } => format!("{x},{y},{w},{h}"),
            Region::Pct { x, y, w, h } => format!("pct:{x},{y},{w},{h}"),
        }
    }

    pub fn size_to_string(s: &Size) -> String {
        match s {
            Size::Max => "max".to_string(),
            Size::Width(w) => format!("{w},"),
            Size::Height(h) => format!(",{h}"),
            Size::Wh(w, h) => format!("{w},{h}"),
            Size::Confined(w, h) => format!("!{w},{h}"),
            Size::Pct(p) => format!("pct:{p}"),
        }
    }
}

#[cfg(test)]
mod proptests {
    use super::canonical::{region_to_string, size_to_string};
    use super::*;
    use proptest::prelude::*;

    proptest! {
        /// `parse_region` must never panic on ANY input string, however malformed —
        /// untrusted HTTP path segments reach this parser directly.
        #[test]
        fn parse_region_never_panics(s in "\\PC*") {
            let _ = parse_region(&s);
        }

        /// `parse_size` must never panic on ANY input string.
        #[test]
        fn parse_size_never_panics(s in "\\PC*") {
            let _ = parse_size(&s);
        }

        /// `parse_quality_format` must never panic on ANY input string.
        #[test]
        fn parse_quality_format_never_panics(s in "\\PC*") {
            let _ = parse_quality_format(&s);
        }

        /// `ImageRequest::parse` (the full 4-segment grammar) must never panic on ANY
        /// combination of arbitrary segment strings.
        #[test]
        fn image_request_parse_never_panics(
            region in "\\PC*", size in "\\PC*", rotation in "\\PC*", qf in "\\PC*"
        ) {
            let _ = ImageRequest::parse(&region, &size, &rotation, &qf);
        }

        /// Canonical round-trip: a well-formed `Region` formatted to its canonical string
        /// and reparsed yields the same `Region`.
        #[test]
        fn region_canonical_round_trip(region in arb_region()) {
            let s = region_to_string(&region);
            prop_assert_eq!(parse_region(&s), Ok(region));
        }

        /// Canonical round-trip for `Size`.
        #[test]
        fn size_canonical_round_trip(size in arb_size()) {
            let s = size_to_string(&size);
            prop_assert_eq!(parse_size(&s), Ok(size));
        }

        /// parse -> format -> parse idempotence: reparsing the canonical form of an
        /// already-parsed region yields the identical value (a second round-trip is a
        /// no-op).
        #[test]
        fn region_parse_format_parse_idempotent(region in arb_region()) {
            let once = region_to_string(&region);
            let parsed = parse_region(&once).unwrap();
            let twice = region_to_string(&parsed);
            prop_assert_eq!(once, twice);
        }

        /// parse -> format -> parse idempotence for `Size`.
        #[test]
        fn size_parse_format_parse_idempotent(size in arb_size()) {
            let once = size_to_string(&size);
            let parsed = parse_size(&once).unwrap();
            let twice = size_to_string(&parsed);
            prop_assert_eq!(once, twice);
        }
    }

    /// Bounded, finite (non-NaN/non-infinite) percentage generator for `Region::Pct`/
    /// `Size::Pct` — percentages are always parsed via `f64::parse`, and IIIF's pct
    /// grammar is a plain decimal, so infinities/NaN are out of the grammar's domain
    /// (and can't round-trip through decimal formatting anyway).
    fn arb_pct() -> impl Strategy<Value = f64> {
        (0u32..=100_000).prop_map(|n| n as f64 / 1000.0)
    }

    fn arb_region() -> impl Strategy<Value = Region> {
        prop_oneof![
            Just(Region::Full),
            Just(Region::Square),
            (any::<u64>(), any::<u64>(), any::<u64>(), any::<u64>())
                .prop_map(|(x, y, w, h)| Region::Px { x, y, w, h }),
            (arb_pct(), arb_pct(), arb_pct(), arb_pct()).prop_map(|(x, y, w, h)| Region::Pct {
                x,
                y,
                w,
                h
            }),
        ]
    }

    fn arb_size() -> impl Strategy<Value = Size> {
        prop_oneof![
            Just(Size::Max),
            any::<u64>().prop_map(Size::Width),
            any::<u64>().prop_map(Size::Height),
            (any::<u64>(), any::<u64>()).prop_map(|(w, h)| Size::Wh(w, h)),
            arb_pct().prop_map(Size::Pct),
        ]
    }
}
