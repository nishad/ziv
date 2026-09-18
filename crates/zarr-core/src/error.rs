use thiserror::Error;

#[derive(Debug, Clone, Error)]
pub enum ZarrError {
    #[error("failed to open zarr store or node: {0}")]
    Open(String),
    #[error("no multiscales metadata found in group attributes")]
    NoMultiscales,
    #[error("unsupported OME-Zarr version: {0} (supported: 0.4, 0.5)")]
    UnsupportedVersion(String),
    #[error(
        "unsupported dtype: {0} (supported: uint8, uint16, uint32, uint64, int8, int16, int32, int64, float32, float64)"
    )]
    UnsupportedDtype(String),
    #[error("region read failed: {0}")]
    Read(String),
    #[error("unknown or misnamed axis: {0} (expected t/c/z/y/x)")]
    UnknownAxis(String),
    #[error(
        "pyramid levels disagree on dtype: level 0 (path {first_path}) is {first:?}, but level {level} (path {level_path}) is {found:?} — every multiscale level must share one dtype"
    )]
    InconsistentLevelDtype {
        level: usize,
        level_path: String,
        first_path: String,
        first: DType,
        found: DType,
    },
    #[error(
        "refusing to connect to internal/link-local/metadata host {host} ({ip}): blocked by SSRF guard (pass --allow-internal-hosts / set ZIV_ALLOW_INTERNAL_HOSTS=1 to override for trusted internal deployments)"
    )]
    BlockedHost { host: String, ip: String },
}

/// A Zarr array's element type, narrowed to the set ziv knows how to widen to `f64` and read back.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DType {
    U8,
    U16,
    U32,
    /// `zarrs` registers `uint64`/`<u8`/`>u8` through the exact same mechanism as `int64` (both
    /// `zarrs-0.23.14/src/array/data_type/{uint,int}.rs` register both endianness prefixes as V2
    /// aliases of one `DataType`), so classifying it cost the same one `classify_dtype` branch
    /// `I64` needed, and every other match site (`dtype_byte_width`, both `widen!` macros,
    /// `natural_range`) already had to grow an 8-byte arm for `I64` regardless — there was no
    /// smaller unit of work that added `I64` alone.
    ///
    /// Added alongside `I64` rather than for any reported defect of its own. It still gets its
    /// own committed fixture (`tests/fixtures/sample_u64_label.ome.zarr`, built by
    /// `build_i64_label_fixture.rs`'s `builds_u64_fixture`, opened by `tests/u64_label.rs`) rather
    /// than only the spot-check this module otherwise gives currently-unneeded multi-byte dtypes
    /// (see `dtype_be_i4_round_trip_decodes_correct_values`'s doc comment on `>i2`/`>u4`/`>f4`/
    /// `>f8`), because the fixture cost nothing extra: every value the label fixture writes is
    /// non-negative, so its on-disk bytes are identical whether declared `<i8` or `<u8` — the
    /// sibling fixture is the same bytes with one changed dtype string.
    ///
    /// Shares `I64`'s `f64`-widening precision limit, and loses more: every value here is first
    /// widened to `f64` exactly like `I64` (see `I64`'s doc comment for the exact aliasing), and
    /// a `u64` at or beyond `i64::MAX` (2^63) additionally saturates when `tiling::labels::
    /// colorize` casts the widened `f64` back with `as i64` — every such value reads back as
    /// `i64::MAX`, so every label identifier from 2^63 upward collapses onto one colour.
    U64,
    I8,
    I16,
    I32,
    /// Added for label images: numpy's default integer type is `int64`, so real OME-NGFF label
    /// arrays (IDR ships them; see `idr0101A-13457537`'s `labels/0`) are commonly stored as
    /// `<i8`/`>i8`, and a dtype ziv could not classify used to make the whole label silently
    /// vanish (see `ZarrImage::label_open_failures`) rather than fail loudly or, worse, report
    /// "no label images" when one genuinely exists. `crates/zarr-core/tests/i64_label.rs` covers
    /// it end to end against a committed fixture.
    ///
    /// **Precision limit, by design, not fixed here.** Every value on this path is widened to
    /// `f64` (`retrieve_widened`/`retrieve_widened_async_inner`), and label colouring rounds it
    /// back to `i64` (`tiling::labels::colorize`, `.round() as i64`). `f64` has 53 bits of
    /// integer mantissa, so an identifier up to and including 2^53 (9007199254740992)
    /// round-trips exactly, but 2^53 + 1 (9007199254740993) reads back as 2^53, and
    /// 9007199254740995 reads back as 9007199254740996 — two distinct label identifiers above
    /// 2^53 can alias onto the same colour, and a `table`-palette colour declared for the
    /// aliased-away value would then paint the wrong object and leave the real one transparent.
    /// A lossless fix would need an integer plane representation running parallel to the `f64`
    /// one the chunk cache and resampler already share throughout the render path, which is a
    /// much larger change than any defect reported against this codebase has needed — and
    /// segmentation tooling numbers objects sequentially from 0 or 1, so identifiers past 2^53
    /// are not a realistic case in practice. `tiling::labels::colorize`'s
    /// `values_above_2_53_can_alias_onto_the_same_colour_by_design` test pins this exact
    /// behaviour so it is never "fixed" by accident.
    I64,
    F32,
    F64,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn error_messages_are_specific() {
        assert_eq!(
            ZarrError::UnsupportedVersion("0.3".into()).to_string(),
            "unsupported OME-Zarr version: 0.3 (supported: 0.4, 0.5)"
        );
        assert_eq!(DType::U8, DType::U8);
    }

    #[test]
    fn unsupported_dtype_message_lists_full_supported_set() {
        assert_eq!(
            ZarrError::UnsupportedDtype("bool".into()).to_string(),
            "unsupported dtype: bool (supported: uint8, uint16, uint32, uint64, int8, int16, int32, int64, float32, float64)"
        );
    }
}
