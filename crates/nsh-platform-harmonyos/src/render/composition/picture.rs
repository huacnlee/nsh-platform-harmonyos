//! One initial, owned ArkUI content sample. Continuous inputs use NativeImage.

pub(crate) struct Picture {
    pub(crate) width: u32,
    pub(crate) height: u32,
    pub(crate) stride: u32,
    pub(crate) format: u32,
    pub(crate) alpha: u32,
    pub(crate) pixels: Vec<u8>,
}

impl Picture {
    pub(crate) fn validate(&self) -> Result<(), &'static str> {
        if self.width == 0
            || self.height == 0
            || self.width > 4096
            || self.height > 4096
            || !matches!(self.format, 3 | 4)
            || !matches!(self.alpha, 1..=3)
            || self.stride < self.width * 4
            || u64::from(self.stride) * u64::from(self.height) > 67_108_864
            || u64::from(self.stride) * u64::from(self.height) != self.pixels.len() as u64
        {
            return Err("invalid ArkUI content picture storage");
        }
        Ok(())
    }

    /// Normalize padded RGBA/BGRA to the compositor's packed premultiplied RGBA.
    pub(crate) fn into_rgba(mut self) -> Result<Vec<u8>, &'static str> {
        self.validate()?;
        let row = self.width as usize * 4;
        for y in 0..self.height as usize {
            let start = y * self.stride as usize;
            self.pixels.copy_within(start..start + row, y * row);
        }
        self.pixels.truncate(row * self.height as usize);
        if self.format == 4 || self.alpha != 2 {
            for pixel in self.pixels.as_chunks_mut::<4>().0 {
                if self.format == 4 {
                    pixel.swap(0, 2);
                }
                if self.alpha == 1 {
                    pixel[3] = 255;
                } else if self.alpha == 3 {
                    let alpha = u16::from(pixel[3]);
                    for channel in &mut pixel[..3] {
                        *channel = ((u16::from(*channel) * alpha + 127) / 255) as u8;
                    }
                }
            }
        }
        Ok(self.pixels)
    }
}

#[cfg(test)]
mod tests {
    use super::Picture;

    #[test]
    fn padded_bgra_rows_keep_top_left_order_and_premultiply_once() {
        let picture = Picture {
            width: 1,
            height: 2,
            stride: 8,
            format: 4,
            alpha: 3,
            pixels: vec![30, 20, 10, 128, 9, 9, 9, 9, 60, 50, 40, 255, 9, 9, 9, 9],
        };
        assert_eq!(
            picture.into_rgba().unwrap(),
            [5, 10, 15, 128, 40, 50, 60, 255]
        );
    }

    #[test]
    fn rejects_truncated_and_oversized_storage() {
        let mut picture = Picture {
            width: 1,
            height: 1,
            stride: 4,
            format: 3,
            alpha: 2,
            pixels: vec![1, 2, 3],
        };
        assert!(picture.validate().is_err());
        picture.pixels.push(4);
        assert!(picture.validate().is_ok());
        picture.stride = u32::MAX;
        assert!(picture.validate().is_err());
        picture.stride = 4;
        picture.format = 9;
        assert!(picture.validate().is_err());
    }
}
