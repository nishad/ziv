use thiserror::Error;

pub mod encode;
pub use encode::{encode_jpeg_rgb8, encode_png_rgb8, encode_rgb8};

pub mod transform;
pub use transform::{apply_quality, rotate_rgb8};

pub mod resample;
pub use resample::{resize_nearest, resize_rgb8};

pub mod labels;
pub use labels::{colorize, composite_over, label_rgba, parse_opacity, LabelPalette};

pub mod engine;
pub use engine::{
    ChannelDescriptor, ImageDimensions, LabelDescriptor, RenderSpec, TileEngine, ZarrTileEngine,
};

#[derive(Debug, Error)]
pub enum TileError {
    #[error("zarr error: {0}")]
    Zarr(#[from] zarr_core::ZarrError),
    #[error("iiif error: {0}")]
    Iiif(String),
    #[error("encode error: {0}")]
    Encode(String),
    #[error("out of range: {0}")]
    OutOfRange(String),
    /// A named projection identifier that this image does not define. ziv has no named-projection
    /// registry, so every `ProjectionId::Named` is unknown — and must 404 rather than silently
    /// rendering the default, which would make every identifier on earth a valid image id.
    #[error("unknown projection identifier: {0}")]
    UnknownProjection(String),
    /// A `label=` component naming a label image this image does not carry. A 404 for the same
    /// reason as `UnknownProjection`: the grammar was fine, the resource does not exist.
    #[error("unknown label image: {0}")]
    UnknownLabel(String),
}
