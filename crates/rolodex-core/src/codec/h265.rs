use crate::{
    Error, Result,
    frame::{DecodedFrame, PlaneData},
};

use super::{CodecConfig, VideoCodec, VideoDecoder};

#[derive(Default)]
pub struct H265Decoder {
    inner: rust_h265::Decoder,
}

impl H265Decoder {
    fn feed(&mut self, annex_b: &[u8]) -> Result<Option<DecodedFrame>> {
        let mut output = None;
        for nal in rust_h265::parse_annex_b(annex_b) {
            if let Some(frame) = self
                .inner
                .decode_nal(&nal)
                .map_err(|error| Error::Decode(error.to_string()))?
            {
                output = Some(convert_frame(frame));
                break;
            }
        }
        Ok(output)
    }
}

impl VideoDecoder for H265Decoder {
    fn initialise(&mut self, config: &CodecConfig) -> Result<()> {
        if config.codec != VideoCodec::H265 {
            return Err(Error::CodecMismatch {
                expected: VideoCodec::H265,
                actual: config.codec,
            });
        }
        let parameter_sets = config.parameter_sets_annex_b();
        let _ = self.feed(&parameter_sets)?;
        Ok(())
    }

    fn push_access_unit(&mut self, annex_b: &[u8]) -> Result<Option<DecodedFrame>> {
        self.feed(annex_b)
    }
}

fn convert_frame(frame: rust_h265::Frame) -> DecodedFrame {
    use rust_h265::PixelData;

    let convert = |plane| match plane {
        PixelData::U8(data) => PlaneData::U8(data),
        PixelData::U16(data) => PlaneData::U16(data),
    };

    DecodedFrame {
        width: frame.width,
        height: frame.height,
        bit_depth: frame.bit_depth,
        y: convert(frame.y),
        u: convert(frame.u),
        v: convert(frame.v),
    }
}
