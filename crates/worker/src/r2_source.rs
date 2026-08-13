use async_trait::async_trait;
use thumbnail_core::{ByteSource, Error, Result};
use worker::{Bucket, Conditional, Range};

pub struct R2Source {
    bucket: Bucket,
    key: String,
    length: u64,
    etag: String,
}

impl R2Source {
    pub fn new(bucket: Bucket, key: String, length: u64, etag: String) -> Self {
        Self {
            bucket,
            key,
            length,
            etag,
        }
    }
}

#[async_trait(?Send)]
impl ByteSource for R2Source {
    fn len(&self) -> u64 {
        self.length
    }

    async fn range(&self, offset: u64, length: usize) -> Result<Vec<u8>> {
        let object = self
            .bucket
            .get(&self.key)
            .only_if(Conditional {
                etag_matches: Some(self.etag.clone()),
                ..Conditional::default()
            })
            .range(Range::OffsetWithLength {
                offset,
                length: length as u64,
            })
            .execute()
            .await
            .map_err(|error| Error::Source(format!("R2 GET failed: {error}")))?
            .ok_or_else(|| Error::Source("R2 source object disappeared".into()))?;
        let body = object
            .body()
            .ok_or_else(|| Error::Source("R2 ranged GET returned no body".into()))?;
        body.bytes()
            .await
            .map_err(|error| Error::Source(format!("R2 body read failed: {error}")))
    }
}
