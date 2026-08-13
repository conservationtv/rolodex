use crate::{Error, Result};

#[derive(Debug, Clone)]
pub enum PlaneData {
    U8(Vec<u8>),
    U16(Vec<u16>),
}

impl PlaneData {
    pub fn len(&self) -> usize {
        match self {
            Self::U8(data) => data.len(),
            Self::U16(data) => data.len(),
        }
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }
}

#[derive(Debug, Clone)]
pub struct DecodedFrame {
    pub width: u32,
    pub height: u32,
    pub bit_depth: u8,
    pub y: PlaneData,
    pub u: PlaneData,
    pub v: PlaneData,
}

impl DecodedFrame {
    pub fn validate(&self) -> Result<()> {
        if self.width < 2
            || self.height < 2
            || !self.width.is_multiple_of(2)
            || !self.height.is_multiple_of(2)
        {
            return Err(Error::Unsupported(
                "decoded frame must have even 4:2:0 dimensions",
            ));
        }
        if !(8..=16).contains(&self.bit_depth) {
            return Err(Error::Unsupported("decoded bit depth is outside 8..=16"));
        }
        let luma = (self.width as usize)
            .checked_mul(self.height as usize)
            .ok_or(Error::Limit("decoded frame dimensions overflow"))?;
        let chroma = luma / 4;
        if self.y.len() != luma || self.u.len() != chroma || self.v.len() != chroma {
            return Err(Error::Invalid(
                "decoder returned inconsistent plane lengths",
            ));
        }
        match (self.bit_depth > 8, &self.y, &self.u, &self.v) {
            (false, PlaneData::U8(_), PlaneData::U8(_), PlaneData::U8(_))
            | (true, PlaneData::U16(_), PlaneData::U16(_), PlaneData::U16(_)) => {}
            (false, PlaneData::U16(_), PlaneData::U16(_), PlaneData::U16(_)) => {
                return Err(Error::Invalid("8-bit frame used 16-bit plane storage"));
            }
            (true, PlaneData::U8(_), PlaneData::U8(_), PlaneData::U8(_)) => {
                return Err(Error::Invalid(
                    "high-bit-depth frame used 8-bit plane storage",
                ));
            }
            _ => return Err(Error::Invalid("decoder returned mixed plane storage types")),
        }
        Ok(())
    }
}
