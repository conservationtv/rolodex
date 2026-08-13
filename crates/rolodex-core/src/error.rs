use crate::codec::VideoCodec;

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("invalid media: {0}")]
    Invalid(&'static str),

    #[error("unsupported media: {0}")]
    Unsupported(&'static str),

    #[error("resource limit exceeded: {0}")]
    Limit(&'static str),

    #[error("codec mismatch: expected {expected}, found {actual}")]
    CodecMismatch {
        expected: VideoCodec,
        actual: VideoCodec,
    },

    #[error("decoder failed: {0}")]
    Decode(String),

    #[error("byte source failed: {0}")]
    Source(String),

    #[error("JPEG encoding failed: {0}")]
    Jpeg(String),

    #[error("no independently decodable video picture found")]
    NoRandomAccessPicture,
}

impl Error {
    /// Byte-source failures may improve when a Queue message is retried.
    pub fn is_retryable(&self) -> bool {
        matches!(self, Self::Source(_))
    }
}

pub type Result<T> = std::result::Result<T, Error>;
