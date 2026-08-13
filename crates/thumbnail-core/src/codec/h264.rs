use crate::{
    Error, Result,
    frame::{DecodedFrame, PlaneData},
};

use super::{CodecConfig, VideoCodec, VideoDecoder};

#[derive(Default)]
pub struct H264Decoder {
    inner: rusty_h264_decoder::Decoder,
}

impl VideoDecoder for H264Decoder {
    fn initialise(&mut self, config: &CodecConfig) -> Result<()> {
        if config.codec != VideoCodec::H264 {
            return Err(Error::CodecMismatch {
                expected: VideoCodec::H264,
                actual: config.codec,
            });
        }
        let parameter_sets = config.parameter_sets_annex_b();
        self.inner
            .decode(&parameter_sets)
            .map_err(|error| Error::Decode(error.to_string()))?;
        Ok(())
    }

    fn push_access_unit(&mut self, annex_b: &[u8]) -> Result<Option<DecodedFrame>> {
        let frame = self
            .inner
            .decode(annex_b)
            .map_err(|error| Error::Decode(error.to_string()))?;
        Ok(frame.map(|frame| DecodedFrame {
            width: frame.width as u32,
            height: frame.height as u32,
            bit_depth: 8,
            y: PlaneData::U8(frame.y),
            u: PlaneData::U8(frame.u),
            v: PlaneData::U8(frame.v),
        }))
    }
}
