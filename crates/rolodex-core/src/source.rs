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

#[async_trait(?Send)]
impl ByteSource for [u8] {
    fn len(&self) -> u64 {
        self.len() as u64
    }

    async fn range(&self, offset: u64, length: usize) -> Result<Vec<u8>> {
        range_from_slice(self, offset, length)
    }
}

fn range_from_slice(data: &[u8], offset: u64, length: usize) -> Result<Vec<u8>> {
    let start = usize::try_from(offset)
        .map_err(|_| crate::Error::Source("range offset does not fit usize".into()))?;
    let end = start
        .checked_add(length)
        .ok_or_else(|| crate::Error::Source("range end overflow".into()))?;
    let bytes = data
        .get(start..end)
        .ok_or_else(|| crate::Error::Source("range outside object".into()))?;
    Ok(bytes.to_vec())
}

#[cfg(feature = "std-fs")]
mod filesystem {
    use std::{
        fs::{self, File},
        io::{Read, Seek, SeekFrom},
        path::Path,
        sync::{
            Arc, Mutex,
            atomic::{AtomicU64, Ordering},
        },
        time::{SystemTime, UNIX_EPOCH},
    };

    use async_trait::async_trait;

    use crate::{Error, Result, source::ByteSource};

    pub type FileReadCounter = Arc<AtomicU64>;

    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    pub struct FileStamp {
        pub byte_length: u64,
        pub modified_utc_ns: u64,
    }

    pub struct FileSource {
        file: Mutex<File>,
        length: u64,
        bytes_read: FileReadCounter,
    }

    impl FileSource {
        pub fn open(path: &Path) -> std::result::Result<Self, String> {
            Self::open_with_counter(path, Arc::new(AtomicU64::new(0)))
        }

        pub fn open_with_counter(
            path: &Path,
            bytes_read: FileReadCounter,
        ) -> std::result::Result<Self, String> {
            let file =
                File::open(path).map_err(|error| format!("open {}: {error}", path.display()))?;
            let length = file
                .metadata()
                .map_err(|error| format!("stat {}: {error}", path.display()))?
                .len();
            Ok(Self {
                file: Mutex::new(file),
                length,
                bytes_read,
            })
        }

        pub fn bytes_read(&self) -> u64 {
            self.bytes_read.load(Ordering::Relaxed)
        }

        pub fn read_counter() -> FileReadCounter {
            Arc::new(AtomicU64::new(0))
        }

        pub fn counter_value(counter: &FileReadCounter) -> u64 {
            counter.load(Ordering::Relaxed)
        }
    }

    #[async_trait(?Send)]
    impl ByteSource for FileSource {
        fn len(&self) -> u64 {
            self.length
        }

        async fn range(&self, offset: u64, length: usize) -> Result<Vec<u8>> {
            let mut file = self
                .file
                .lock()
                .map_err(|_| Error::Source("local file lock poisoned".into()))?;
            file.seek(SeekFrom::Start(offset))
                .map_err(|error| Error::Source(format!("seek to {offset}: {error}")))?;
            let mut bytes = vec![0; length];
            file.read_exact(&mut bytes).map_err(|error| {
                Error::Source(format!("read {length} bytes at {offset}: {error}"))
            })?;
            self.bytes_read.fetch_add(length as u64, Ordering::Relaxed);
            Ok(bytes)
        }
    }

    pub fn file_stamp(path: &Path) -> std::result::Result<FileStamp, String> {
        let metadata =
            fs::metadata(path).map_err(|error| format!("stat {}: {error}", path.display()))?;
        let modified = metadata
            .modified()
            .map_err(|error| format!("mtime {}: {error}", path.display()))?;
        Ok(FileStamp {
            byte_length: metadata.len(),
            modified_utc_ns: system_time_ns(modified)?,
        })
    }

    pub fn system_time_ns(value: SystemTime) -> std::result::Result<u64, String> {
        let duration = value
            .duration_since(UNIX_EPOCH)
            .map_err(|_| "file modification time is before the Unix epoch".to_string())?;
        u64::try_from(duration.as_nanos())
            .map_err(|_| "file modification time exceeds u64 nanoseconds".to_string())
    }
}

#[cfg(feature = "std-fs")]
pub use filesystem::{FileReadCounter, FileSource, FileStamp, file_stamp, system_time_ns};

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn byte_slices_are_borrowing_random_access_sources() {
        let bytes = b"abcdef";
        assert_eq!(ByteSource::len(bytes.as_slice()), 6);
        assert_eq!(
            futures::executor::block_on(bytes.as_slice().range(2, 3)).unwrap(),
            b"cde"
        );
    }

    #[cfg(feature = "std-fs")]
    #[test]
    fn file_source_counts_reads_and_reports_a_stamp() {
        use std::{env, fs, time::SystemTime};

        let path = env::temp_dir().join(format!(
            "rolodex-file-source-{}-{}",
            std::process::id(),
            system_time_ns(SystemTime::now()).unwrap()
        ));
        fs::write(&path, b"abcdef").unwrap();
        let source = FileSource::open(&path).unwrap();
        assert_eq!(file_stamp(&path).unwrap().byte_length, 6);
        assert_eq!(
            futures::executor::block_on(source.range(1, 2)).unwrap(),
            b"bc"
        );
        assert_eq!(source.bytes_read(), 2);
        fs::remove_file(path).unwrap();
    }
}
