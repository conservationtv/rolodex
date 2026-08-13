use crate::{
    ByteSource, Error, Result,
    mp4::{BoxHeader, read_top_level_header},
};

#[derive(Debug, Clone, Copy)]
pub struct PipelineLimits {
    pub initial_read_bytes: usize,
    pub max_init_bytes: usize,
    pub max_fragment_bytes: usize,
    pub max_sample_bytes: usize,
    pub max_total_bytes_read: u64,
    pub max_top_level_boxes: u32,
    pub max_fragments: u32,
    pub max_samples_examined: u32,
    pub max_samples_decoded: u32,
}

impl Default for PipelineLimits {
    fn default() -> Self {
        Self {
            initial_read_bytes: 256 * 1024,
            max_init_bytes: 8 * 1024 * 1024,
            max_fragment_bytes: 8 * 1024 * 1024,
            max_sample_bytes: 32 * 1024 * 1024,
            max_total_bytes_read: 128 * 1024 * 1024,
            max_top_level_boxes: 20_000,
            max_fragments: 5_000,
            max_samples_examined: 100_000,
            max_samples_decoded: 32,
        }
    }
}

impl PipelineLimits {
    pub(crate) fn validate(self) -> Result<Self> {
        if self.initial_read_bytes < 16
            || self.initial_read_bytes as u64 > self.max_total_bytes_read
            || self.max_init_bytes < 16
            || self.max_fragment_bytes < 16
            || self.max_sample_bytes == 0
            || self.max_total_bytes_read == 0
            || self.max_top_level_boxes == 0
            || self.max_fragments == 0
            || self.max_samples_examined == 0
            || self.max_samples_decoded == 0
        {
            return Err(Error::Invalid(
                "pipeline limits must be non-zero and able to hold a box header",
            ));
        }
        Ok(self)
    }
}

pub(crate) struct RangeReader<'a, S: ByteSource + ?Sized> {
    source: &'a S,
    object_len: u64,
    prefix: Vec<u8>,
    window_offset: u64,
    window: Vec<u8>,
    limits: PipelineLimits,
    requests: u32,
    bytes_read: u64,
}

impl<'a, S: ByteSource + ?Sized> RangeReader<'a, S> {
    pub(crate) async fn new(source: &'a S, limits: PipelineLimits) -> Result<Self> {
        let prefix_len = usize::try_from(source.len().min(limits.initial_read_bytes as u64))
            .map_err(|_| Error::Limit("prefix length does not fit memory"))?;
        let prefix = source.range(0, prefix_len).await?;
        if prefix.len() != prefix_len {
            return Err(Error::Source(format!(
                "short prefix read: expected {prefix_len}, got {}",
                prefix.len()
            )));
        }
        Ok(Self {
            source,
            object_len: source.len(),
            prefix,
            window_offset: 0,
            window: Vec::new(),
            limits,
            requests: 1,
            bytes_read: prefix_len as u64,
        })
    }

    pub(crate) fn len(&self) -> u64 {
        self.object_len
    }

    pub(crate) async fn read(&mut self, offset: u64, length: usize) -> Result<Vec<u8>> {
        self.read_internal(offset, length, length).await
    }

    pub(crate) async fn read_ahead(
        &mut self,
        offset: u64,
        length: usize,
        read_ahead: usize,
    ) -> Result<Vec<u8>> {
        self.read_internal(offset, length, read_ahead.max(length))
            .await
    }

    async fn read_internal(
        &mut self,
        offset: u64,
        length: usize,
        fetch_length: usize,
    ) -> Result<Vec<u8>> {
        let end = offset
            .checked_add(length as u64)
            .ok_or(Error::Invalid("range end overflow"))?;
        if end > self.object_len {
            return Err(Error::Invalid("range extends beyond object"));
        }
        if end <= self.prefix.len() as u64 {
            let start = usize::try_from(offset).expect("prefix offset fits usize");
            return Ok(self.prefix[start..start + length].to_vec());
        }
        let window_end = self
            .window_offset
            .checked_add(self.window.len() as u64)
            .ok_or(Error::Invalid("read window end overflow"))?;
        if offset >= self.window_offset && end <= window_end {
            let start = usize::try_from(offset - self.window_offset)
                .map_err(|_| Error::Limit("read window offset does not fit memory"))?;
            return Ok(self.window[start..start + length].to_vec());
        }
        let prefix_start = usize::try_from(offset.min(self.prefix.len() as u64))
            .expect("prefix offset fits usize");
        let prefix_bytes = self.prefix.len().saturating_sub(prefix_start).min(length);
        let source_offset = offset
            .checked_add(prefix_bytes as u64)
            .ok_or(Error::Invalid("range source offset overflow"))?;
        let requested_source_length = length - prefix_bytes;
        let available = self
            .object_len
            .checked_sub(source_offset)
            .ok_or(Error::Invalid("range source offset after object"))?;
        let desired_source_length = fetch_length
            .saturating_sub(prefix_bytes)
            .max(requested_source_length);
        let source_length = usize::try_from(available.min(desired_source_length as u64))
            .map_err(|_| Error::Limit("read-ahead length does not fit memory"))?;
        let source_length = source_length.max(requested_source_length);
        let prospective = self
            .bytes_read
            .checked_add(source_length as u64)
            .ok_or(Error::Limit("total read byte counter overflow"))?;
        if prospective > self.limits.max_total_bytes_read {
            return Err(Error::Limit("total bytes read exceeds job limit"));
        }
        let tail = self.source.range(source_offset, source_length).await?;
        if tail.len() != source_length {
            return Err(Error::Source(format!(
                "short range read at {source_offset}: expected {source_length}, got {}",
                tail.len()
            )));
        }
        let mut data = Vec::new();
        data.try_reserve_exact(length)
            .map_err(|_| Error::Limit("range buffer allocation failed"))?;
        if prefix_bytes != 0 {
            data.extend_from_slice(&self.prefix[prefix_start..prefix_start + prefix_bytes]);
        }
        data.extend_from_slice(&tail[..requested_source_length]);
        self.window_offset = source_offset;
        self.window = tail;
        self.requests = self.requests.saturating_add(1);
        self.bytes_read = prospective;
        Ok(data)
    }

    pub(crate) async fn top_level_header_read_ahead(
        &mut self,
        offset: u64,
        read_ahead: usize,
    ) -> Result<BoxHeader> {
        let remaining = self
            .object_len
            .checked_sub(offset)
            .ok_or(Error::Invalid("top-level offset after object end"))?;
        if remaining < 8 {
            return Err(Error::Invalid("trailing bytes cannot contain a box header"));
        }
        let bytes = self
            .read_ahead(offset, remaining.min(16) as usize, read_ahead)
            .await?;
        read_top_level_header(&bytes, offset, self.object_len)
    }

    pub(crate) fn requests(&self) -> u32 {
        self.requests
    }

    pub(crate) fn bytes_read(&self) -> u64 {
        self.bytes_read
    }
}

#[cfg(test)]
mod tests {
    use std::cell::RefCell;

    use async_trait::async_trait;

    use super::*;

    struct RecordingSource {
        data: Vec<u8>,
        reads: RefCell<Vec<(u64, usize)>>,
    }

    #[async_trait(?Send)]
    impl ByteSource for RecordingSource {
        fn len(&self) -> u64 {
            self.data.len() as u64
        }

        async fn range(&self, offset: u64, length: usize) -> Result<Vec<u8>> {
            self.reads.borrow_mut().push((offset, length));
            let start = usize::try_from(offset).unwrap();
            Ok(self.data[start..start + length].to_vec())
        }
    }

    #[test]
    fn a_range_crossing_the_prefix_fetches_only_the_missing_tail() {
        let source = RecordingSource {
            data: (0..32).collect(),
            reads: RefCell::new(Vec::new()),
        };
        let limits = PipelineLimits {
            initial_read_bytes: 16,
            max_total_bytes_read: 32,
            ..PipelineLimits::default()
        };
        let mut reader = futures::executor::block_on(RangeReader::new(&source, limits)).unwrap();
        let data = futures::executor::block_on(reader.read(8, 16)).unwrap();

        assert_eq!(data, (8..24).collect::<Vec<_>>());
        assert_eq!(*source.reads.borrow(), vec![(0, 16), (16, 8)]);
        assert_eq!(reader.requests(), 2);
        assert_eq!(reader.bytes_read(), 24);
    }
}
