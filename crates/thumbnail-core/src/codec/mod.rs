mod annex_b;

#[cfg(feature = "codec-h264")]
mod h264;
#[cfg(feature = "codec-h265")]
mod h265;

use core::fmt;

use serde::{Deserialize, Serialize};

use crate::{Result, frame::DecodedFrame};

pub(crate) use annex_b::annex_b_access_unit;
pub use annex_b::{mp4_sample_to_annex_b, sample_has_random_access_nal};

#[cfg(feature = "codec-h264")]
pub use h264::H264Decoder as SelectedDecoder;
#[cfg(feature = "codec-h265")]
pub use h265::H265Decoder as SelectedDecoder;

#[cfg(feature = "codec-h264")]
pub const BUILD_CODEC: VideoCodec = VideoCodec::H264;
#[cfg(feature = "codec-h265")]
pub const BUILD_CODEC: VideoCodec = VideoCodec::H265;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum VideoCodec {
    H264,
    H265,
}

impl fmt::Display for VideoCodec {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::H264 => f.write_str("h264"),
            Self::H265 => f.write_str("h265"),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CodecConfig {
    pub codec: VideoCodec,
    pub nal_length_size: usize,
    /// Parameter sets without Annex-B start codes.
    pub parameter_sets: Vec<Vec<u8>>,
}

impl CodecConfig {
    pub fn parameter_sets_annex_b(&self) -> Vec<u8> {
        let capacity = self.parameter_sets.iter().map(|nal| 4 + nal.len()).sum();
        let mut output = Vec::with_capacity(capacity);
        for nal in &self.parameter_sets {
            output.extend_from_slice(&[0, 0, 0, 1]);
            output.extend_from_slice(nal);
        }
        output
    }
}

pub trait VideoDecoder: Default {
    fn initialise(&mut self, config: &CodecConfig) -> Result<()>;

    fn push_access_unit(&mut self, annex_b: &[u8]) -> Result<Option<DecodedFrame>>;
}
