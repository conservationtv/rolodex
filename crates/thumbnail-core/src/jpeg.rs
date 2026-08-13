use jpeg_encoder::{Encoder, ImageBuffer, JpegColorType, SamplingFactor};

use crate::{Error, Result, scale::ScaledYuv420};

pub fn encode_jpeg(frame: &ScaledYuv420, quality: u8) -> Result<Vec<u8>> {
    if !(1..=100).contains(&quality) {
        return Err(Error::Invalid("JPEG quality must be in 1..=100"));
    }
    let width = u16::try_from(frame.width).map_err(|_| Error::Limit("JPEG width exceeds 65535"))?;
    let height =
        u16::try_from(frame.height).map_err(|_| Error::Limit("JPEG height exceeds 65535"))?;
    let pixels = (frame.width as usize)
        .checked_mul(frame.height as usize)
        .ok_or(Error::Limit("JPEG pixel count overflow"))?;
    if frame.y.len() != pixels || frame.u.len() != pixels / 4 || frame.v.len() != pixels / 4 {
        return Err(Error::Invalid(
            "scaled YUV planes have inconsistent lengths",
        ));
    }

    let mut output = Vec::new();
    let mut encoder = Encoder::new(&mut output, quality);
    encoder.set_sampling_factor(SamplingFactor::R_4_2_0);
    encoder
        .encode_image(PlanarYuv420Image {
            frame,
            width,
            height,
        })
        .map_err(|error| Error::Jpeg(error.to_string()))?;
    Ok(output)
}

struct PlanarYuv420Image<'a> {
    frame: &'a ScaledYuv420,
    width: u16,
    height: u16,
}

impl ImageBuffer for PlanarYuv420Image<'_> {
    fn get_jpeg_color_type(&self) -> JpegColorType {
        JpegColorType::Ycbcr
    }

    fn width(&self) -> u16 {
        self.width
    }

    fn height(&self) -> u16 {
        self.height
    }

    fn fill_buffers(&self, y: u16, buffers: &mut [Vec<u8>; 4]) {
        let width = usize::from(self.width);
        let y = usize::from(y);
        let luma_start = y * width;
        let chroma_width = width / 2;
        let chroma_start = (y / 2) * chroma_width;
        for x in 0..width {
            buffers[0].push(self.frame.y[luma_start + x]);
            buffers[1].push(self.frame.u[chroma_start + x / 2]);
            buffers[2].push(self.frame.v[chroma_start + x / 2]);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn produces_a_jfif() {
        let frame = ScaledYuv420 {
            width: 2,
            height: 2,
            y: vec![128; 4],
            u: vec![128],
            v: vec![128],
        };
        let jpeg = encode_jpeg(&frame, 78).unwrap();
        assert_eq!(&jpeg[..2], &[0xff, 0xd8]);
        assert_eq!(&jpeg[jpeg.len() - 2..], &[0xff, 0xd9]);
    }
}
