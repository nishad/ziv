#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Rgb(pub u8, pub u8, pub u8);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Lut {
    Grey,
    Fixed(Rgb),
}

impl Lut {
    /// Map a 0..=255 intensity to a color by scaling the LUT's base color.
    pub fn apply(&self, intensity: u8) -> Rgb {
        match self {
            Lut::Grey => Rgb(intensity, intensity, intensity),
            Lut::Fixed(Rgb(r, g, b)) => {
                let scale = |c: u8| ((c as u16 * intensity as u16) / 255) as u8;
                Rgb(scale(*r), scale(*g), scale(*b))
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn grey_is_uniform() {
        assert_eq!(Lut::Grey.apply(128), Rgb(128, 128, 128));
    }

    #[test]
    fn fixed_scales_base_color() {
        assert_eq!(Lut::Fixed(Rgb(255, 0, 0)).apply(255), Rgb(255, 0, 0));
        assert_eq!(Lut::Fixed(Rgb(255, 0, 0)).apply(0), Rgb(0, 0, 0));
    }
}
