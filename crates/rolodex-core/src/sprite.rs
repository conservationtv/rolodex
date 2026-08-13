use crate::{Error, Result, scale::ScaledYuv420};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct SpriteLayout {
    pub columns: u32,
    pub rows: u32,
    pub cell_width: u32,
    pub cell_height: u32,
    pub width: u32,
    pub height: u32,
}

impl SpriteLayout {
    pub(crate) fn new(
        frame_count: u32,
        cell_width: u32,
        cell_height: u32,
        max_pixels: u64,
    ) -> Result<Self> {
        if frame_count == 0 {
            return Err(Error::Invalid("sprite sheet has no frames"));
        }
        if cell_width < 2
            || cell_height < 2
            || !cell_width.is_multiple_of(2)
            || !cell_height.is_multiple_of(2)
        {
            return Err(Error::Invalid(
                "sprite cells must have even dimensions of at least 2x2",
            ));
        }

        let mut best: Option<(u64, u64, u32, Self)> = None;
        for columns in 1..=frame_count {
            let rows = frame_count.div_ceil(columns);
            let Some(width) = columns.checked_mul(cell_width) else {
                continue;
            };
            let Some(height) = rows.checked_mul(cell_height) else {
                continue;
            };
            if width > u16::MAX as u32 || height > u16::MAX as u32 {
                continue;
            }
            let Some(pixels) = u64::from(width).checked_mul(u64::from(height)) else {
                continue;
            };
            if pixels > max_pixels {
                continue;
            }
            let layout = Self {
                columns,
                rows,
                cell_width,
                cell_height,
                width,
                height,
            };
            // Prefer a visually square sheet, then the fewest unused cells.
            let score = u64::from(width.abs_diff(height));
            let unused = u64::from(columns) * u64::from(rows) - u64::from(frame_count);
            let candidate = (score, unused, columns, layout);
            if best.as_ref().is_none_or(|current| {
                (candidate.0, candidate.1, candidate.2) < (current.0, current.1, current.2)
            }) {
                best = Some(candidate);
            }
        }
        best.map(|(_, _, _, layout)| layout).ok_or(Error::Limit(
            "sprite sheet exceeds its pixel or JPEG dimension limit",
        ))
    }
}

pub(crate) struct SpriteBuilder {
    layout: SpriteLayout,
    frame_count: u32,
    next_frame: u32,
    sheet: ScaledYuv420,
}

impl SpriteBuilder {
    pub(crate) fn new(layout: SpriteLayout, frame_count: u32) -> Result<Self> {
        let luma = usize::try_from(
            u64::from(layout.width)
                .checked_mul(u64::from(layout.height))
                .ok_or(Error::Limit("sprite dimensions overflow"))?,
        )
        .map_err(|_| Error::Limit("sprite dimensions do not fit memory"))?;
        let chroma = luma / 4;
        let y = filled_plane(luma, 16, "sprite luma allocation failed")?;
        let u = filled_plane(chroma, 128, "sprite chroma allocation failed")?;
        let v = filled_plane(chroma, 128, "sprite chroma allocation failed")?;
        Ok(Self {
            layout,
            frame_count,
            next_frame: 0,
            sheet: ScaledYuv420 {
                width: layout.width,
                height: layout.height,
                y,
                u,
                v,
            },
        })
    }

    pub(crate) fn push(&mut self, frame: &ScaledYuv420) -> Result<()> {
        if self.next_frame >= self.frame_count {
            return Err(Error::Invalid("too many frames for sprite sheet"));
        }
        if frame.width != self.layout.cell_width || frame.height != self.layout.cell_height {
            return Err(Error::Unsupported(
                "decoded frame dimensions changed within recording",
            ));
        }
        let column = self.next_frame % self.layout.columns;
        let row = self.next_frame / self.layout.columns;
        copy_plane(
            &frame.y,
            frame.width,
            frame.height,
            &mut self.sheet.y,
            self.sheet.width,
            column * frame.width,
            row * frame.height,
        )?;
        copy_plane(
            &frame.u,
            frame.width / 2,
            frame.height / 2,
            &mut self.sheet.u,
            self.sheet.width / 2,
            column * frame.width / 2,
            row * frame.height / 2,
        )?;
        copy_plane(
            &frame.v,
            frame.width / 2,
            frame.height / 2,
            &mut self.sheet.v,
            self.sheet.width / 2,
            column * frame.width / 2,
            row * frame.height / 2,
        )?;
        self.next_frame += 1;
        Ok(())
    }

    pub(crate) fn finish(self) -> Result<ScaledYuv420> {
        if self.next_frame != self.frame_count {
            return Err(Error::Invalid("sprite sheet is missing frames"));
        }
        Ok(self.sheet)
    }
}

fn filled_plane(length: usize, value: u8, message: &'static str) -> Result<Vec<u8>> {
    let mut output = Vec::new();
    output
        .try_reserve_exact(length)
        .map_err(|_| Error::Limit(message))?;
    output.resize(length, value);
    Ok(output)
}

fn copy_plane(
    source: &[u8],
    width: u32,
    height: u32,
    destination: &mut [u8],
    destination_width: u32,
    x: u32,
    y: u32,
) -> Result<()> {
    let width = width as usize;
    let height = height as usize;
    let destination_width = destination_width as usize;
    let x = x as usize;
    let y = y as usize;
    if source.len() != width.saturating_mul(height) {
        return Err(Error::Invalid("sprite source plane has an invalid size"));
    }
    for row in 0..height {
        let source_start = row * width;
        let destination_start = (y + row)
            .checked_mul(destination_width)
            .and_then(|value| value.checked_add(x))
            .ok_or(Error::Limit("sprite plane offset overflow"))?;
        let destination_end = destination_start
            .checked_add(width)
            .ok_or(Error::Limit("sprite plane range overflow"))?;
        let destination_row = destination
            .get_mut(destination_start..destination_end)
            .ok_or(Error::Invalid("sprite cell falls outside destination"))?;
        destination_row.copy_from_slice(&source[source_start..source_start + width]);
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn frame(value: u8) -> ScaledYuv420 {
        ScaledYuv420 {
            width: 4,
            height: 2,
            y: vec![value; 8],
            u: vec![value; 2],
            v: vec![value; 2],
        }
    }

    #[test]
    fn lays_out_and_copies_cells() {
        let layout = SpriteLayout::new(5, 4, 2, 1_000).unwrap();
        assert_eq!((layout.columns, layout.rows), (2, 3));
        let mut builder = SpriteBuilder::new(layout, 5).unwrap();
        builder.push(&frame(20)).unwrap();
        builder.push(&frame(40)).unwrap();
        builder.push(&frame(60)).unwrap();
        builder.push(&frame(80)).unwrap();
        builder.push(&frame(100)).unwrap();
        let sheet = builder.finish().unwrap();
        assert_eq!(&sheet.y[0..4], &[20; 4]);
        assert_eq!(&sheet.y[4..8], &[40; 4]);
        assert_eq!(&sheet.y[16..20], &[60; 4]);
        assert_eq!(&sheet.y[32..36], &[100; 4]);
        assert_eq!(&sheet.y[36..40], &[16; 4]);
    }

    #[test]
    fn rejects_a_sheet_over_the_pixel_limit() {
        let error = SpriteLayout::new(2, 480, 270, 100).unwrap_err();
        assert!(matches!(error, Error::Limit(_)));
    }
}
