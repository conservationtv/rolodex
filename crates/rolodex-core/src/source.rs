use async_trait::async_trait;

use crate::Result;

/// A random-access object whose length is known before parsing starts.
#[async_trait(?Send)]
pub trait ByteSource {
    fn len(&self) -> u64;

    fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// Return exactly `length` bytes from `offset`, or an error.
    async fn range(&self, offset: u64, length: usize) -> Result<Vec<u8>>;
}

/// In-memory source used by focused tests and small decoder spikes.
#[derive(Debug, Clone)]
pub struct MemorySource {
    data: Vec<u8>,
}

impl MemorySource {
    pub fn new(data: Vec<u8>) -> Self {
        Self { data }
    }
}

#[async_trait(?Send)]
impl ByteSource for MemorySource {
    fn len(&self) -> u64 {
        self.data.len() as u64
    }

    async fn range(&self, offset: u64, length: usize) -> Result<Vec<u8>> {
        let start = usize::try_from(offset)
            .map_err(|_| crate::Error::Source("range offset does not fit usize".into()))?;
        let end = start
            .checked_add(length)
            .ok_or_else(|| crate::Error::Source("range end overflow".into()))?;
        let bytes = self
            .data
            .get(start..end)
            .ok_or_else(|| crate::Error::Source("range outside object".into()))?;
        Ok(bytes.to_vec())
    }
}
