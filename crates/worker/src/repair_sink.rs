use async_trait::async_trait;
use rolodex_core::{Error, Result, remux::RepairSink};
use sha2::{Digest, Sha256};
use worker::{Bucket, HttpMetadata};

pub struct R2RepairSink {
    bucket: Bucket,
    prefix: String,
}
impl R2RepairSink {
    pub fn new(bucket: Bucket, prefix: String) -> Self {
        Self { bucket, prefix }
    }
}
#[async_trait(?Send)]
impl RepairSink for R2RepairSink {
    async fn put(&self, bytes: Vec<u8>) -> Result<String> {
        let hash = rolodex_core::hex(&Sha256::digest(&bytes));
        let key = format!("{}{}.mp4", self.prefix, hash);
        self.bucket
            .put(&key, bytes)
            .http_metadata(HttpMetadata {
                content_type: Some("video/mp4".into()),
                cache_control: Some("public, max-age=31536000, immutable".into()),
                ..HttpMetadata::default()
            })
            .execute()
            .await
            .map_err(|e| Error::Source(format!("R2 repair PUT failed: {e}")))?;
        Ok(key)
    }
}
