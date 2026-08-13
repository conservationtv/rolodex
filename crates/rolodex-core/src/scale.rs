use crate::{
    Error, Result,
    frame::{DecodedFrame, PlaneData},
};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ScaledYuv420 {
    pub width: u32,
    pub height: u32,
    pub y: Vec<u8>,
    pub u: Vec<u8>,
    pub v: Vec<u8>,
}

pub fn output_dimensions(
    width: u32,
    height: u32,
    max_width: u32,
    max_height: u32,
) -> Result<(u32, u32)> {
    if width < 2 || height < 2 || max_width < 2 || max_height < 2 {
        return Err(Error::Invalid("image dimensions must be at least 2x2"));
    }

    let (mut out_width, mut out_height) = if width <= max_width && height <= max_height {
        (width, height)
    } else if u64::from(width) * u64::from(max_height) > u64::from(height) * u64::from(max_width) {
        (
            max_width,
            ((u64::from(height) * u64::from(max_width)) / u64::from(width)) as u32,
        )
    } else {
        (
            ((u64::from(width) * u64::from(max_height)) / u64::from(height)) as u32,
            max_height,
        )
    };

    // Keep 4:2:0 output dimensions valid. Cropping at most one scaled pixel is
    // preferable to allocating padded planes solely for the JPEG boundary.
    out_width &= !1;
    out_height &= !1;
    if out_width < 2 || out_height < 2 {
        return Err(Error::Unsupported(
            "aspect ratio produces a sub-2px thumbnail dimension",
        ));
    }
    Ok((out_width, out_height))
}

pub fn scale_yuv420(frame: &DecodedFrame, max_width: u32, max_height: u32) -> Result<ScaledYuv420> {
    let (width, height) = output_dimensions(frame.width, frame.height, max_width, max_height)?;
    scale_yuv420_to_dimensions(frame, width, height)
}

pub(crate) fn scale_yuv420_to_dimensions(
    frame: &DecodedFrame,
    width: u32,
    height: u32,
) -> Result<ScaledYuv420> {
    frame.validate()?;
    if width < 2 || height < 2 || !width.is_multiple_of(2) || !height.is_multiple_of(2) {
        return Err(Error::Invalid(
            "scaled dimensions must be even and at least 2x2",
        ));
    }

    let resize = |plane: &PlaneData, divisor: usize| {
        resize_plane(
            plane,
            frame.width as usize / divisor,
            frame.height as usize / divisor,
            width as usize / divisor,
            height as usize / divisor,
            frame.bit_depth,
        )
    };
    let y = resize(&frame.y, 1)?;
    let u = resize(&frame.u, 2)?;
    let v = resize(&frame.v, 2)?;

    Ok(ScaledYuv420 {
        width,
        height,
        y,
        u,
        v,
    })
}

fn resize_plane(
    input: &PlaneData,
    src_width: usize,
    src_height: usize,
    dst_width: usize,
    dst_height: usize,
    bit_depth: u8,
) -> Result<Vec<u8>> {
    let output_len = dst_width
        .checked_mul(dst_height)
        .ok_or(Error::Limit("scaled plane dimensions overflow"))?;
    let mut output = Vec::new();
    output
        .try_reserve_exact(output_len)
        .map_err(|_| Error::Limit("scaled plane allocation failed"))?;
    output.resize(output_len, 0);

    match input {
        PlaneData::U8(data) => {
            resize_area(
                data,
                src_width,
                src_height,
                dst_width,
                dst_height,
                &mut output,
                |v| u64::from(*v),
            )?;
        }
        PlaneData::U16(data) => {
            let max_sample = (1u64 << bit_depth) - 1;
            resize_area(
                data,
                src_width,
                src_height,
                dst_width,
                dst_height,
                &mut output,
                |v| u64::from(*v) * 255 / max_sample,
            )?;
        }
    }
    Ok(output)
}

fn resize_area<T, F>(
    input: &[T],
    src_width: usize,
    src_height: usize,
    dst_width: usize,
    dst_height: usize,
    output: &mut [u8],
    sample: F,
) -> Result<()>
where
    F: Fn(&T) -> u64,
{
    if input.len() != src_width.saturating_mul(src_height)
        || output.len() != dst_width.saturating_mul(dst_height)
    {
        return Err(Error::Invalid("plane dimensions do not match storage"));
    }

    for dst_y in 0..dst_height {
        let src_y0 = dst_y * src_height / dst_height;
        let mut src_y1 = (dst_y + 1) * src_height / dst_height;
        if src_y1 <= src_y0 {
            src_y1 = src_y0 + 1;
        }
        for dst_x in 0..dst_width {
            let src_x0 = dst_x * src_width / dst_width;
            let mut src_x1 = (dst_x + 1) * src_width / dst_width;
            if src_x1 <= src_x0 {
                src_x1 = src_x0 + 1;
            }
            let mut sum = 0u64;
            let mut count = 0u64;
            for src_y in src_y0..src_y1.min(src_height) {
                let row = src_y * src_width;
                for src_x in src_x0..src_x1.min(src_width) {
                    sum += sample(&input[row + src_x]);
                    count += 1;
                }
            }
            output[dst_y * dst_width + dst_x] = ((sum + count / 2) / count) as u8;
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fits_inside_bounds_and_keeps_even_dimensions() {
        assert_eq!(output_dimensions(1920, 1080, 480, 270).unwrap(), (480, 270));
        assert_eq!(output_dimensions(1080, 1920, 480, 270).unwrap(), (150, 270));
        assert_eq!(output_dimensions(320, 240, 480, 270).unwrap(), (320, 240));
    }

    #[test]
    fn downscales_constant_planes() {
        let frame = DecodedFrame {
            width: 4,
            height: 4,
            bit_depth: 8,
            y: PlaneData::U8(vec![90; 16]),
            u: PlaneData::U8(vec![120; 4]),
            v: PlaneData::U8(vec![140; 4]),
        };
        let scaled = scale_yuv420(&frame, 2, 2).unwrap();
        assert_eq!(scaled.y, vec![90; 4]);
        assert_eq!(scaled.u, vec![120]);
        assert_eq!(scaled.v, vec![140]);
    }

    #[test]
    fn converts_ten_bit_to_eight_bit() {
        let frame = DecodedFrame {
            width: 2,
            height: 2,
            bit_depth: 10,
            y: PlaneData::U16(vec![1023; 4]),
            u: PlaneData::U16(vec![512]),
            v: PlaneData::U16(vec![0]),
        };
        let scaled = scale_yuv420(&frame, 2, 2).unwrap();
        assert_eq!(scaled.y, vec![255; 4]);
        assert!((127..=128).contains(&scaled.u[0]));
        assert_eq!(scaled.v, vec![0]);
    }
}
