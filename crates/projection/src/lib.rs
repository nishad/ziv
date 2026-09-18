pub mod composite;
pub mod defaults;
pub mod lut;

pub use composite::{composite, window_to_u8};
pub use defaults::{default_projection, parse_hex_color, percentile_window};
pub use lut::{Lut, Rgb};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ZSelector {
    Plane(u64),
}

#[derive(Debug, Clone, PartialEq)]
pub struct ChannelView {
    pub index: usize,
    pub window: (f64, f64),
    pub lut: Lut,
    pub enabled: bool,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Projection {
    pub t: u64,
    pub z: ZSelector,
    pub channels: Vec<ChannelView>,
}
