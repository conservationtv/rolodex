#![forbid(unsafe_code)]

#[cfg(all(feature = "codec-h264", feature = "codec-h265"))]
compile_error!("select exactly one codec feature: codec-h264 or codec-h265");

#[cfg(not(any(feature = "codec-h264", feature = "codec-h265")))]
compile_error!("select exactly one codec feature: codec-h264 or codec-h265");

pub mod codec;
pub mod decimal_string;
pub mod error;
pub mod frame;
pub mod jpeg;
pub mod mp4;
pub mod processor;
mod reader;
pub mod scale;
pub mod source;
mod sprite;

pub use codec::{BUILD_CODEC, CodecConfig, VideoCodec};
pub use error::{Error, Result};
pub use processor::{
    ProcessorMetrics, ProcessorOptions, ProcessorOutput, RecordingMetadata, SourceIdentity,
    parse_duration_ns, process_recording,
};
pub use reader::PipelineLimits;
pub use source::ByteSource;
