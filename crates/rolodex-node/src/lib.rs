//! Repairs a local recording before it is uploaded, for the conservationtv
//! agent: `repairRecording(input, output)` writes the recording at `input` to
//! `output` with its oversized parts split at keyframes, as
//! `rolodex_repair::write_repaired_recording` does, on a worker thread.
//!
//! The repair runs inside the agent's process, so a panic is caught and
//! rejects the call's promise rather than unwinding into Node; the addon is
//! built with the `release-node` profile, which unwinds where `release` aborts.

use std::{
    fs::{self, File},
    io::{self, BufWriter, Write},
    panic::{AssertUnwindSafe, catch_unwind},
    path::{Path, PathBuf},
};

use napi::{Env, Error, Result, Task, bindgen_prelude::AsyncTask};
use napi_derive::napi;
use rolodex_core::{
    PipelineLimits, ProcessorOptions, SourceIdentity, index_recording, source::FileSource,
};
use rolodex_repair::{FileRepair, RepairSkipReason, write_repaired_recording};

/// What a repair did.
#[napi(string_enum = "lowercase")]
pub enum RepairOutcome {
    /// The repaired recording was written to the output path.
    Repaired,
    /// Nothing was written: the recording needs no repair, or can't have one.
    Unchanged,
}

/// Why a recording was left unchanged.
#[napi(string_enum = "snake_case")]
pub enum RepairSkip {
    /// Every independently decodable range is already small enough.
    NotNeeded,
    /// A track besides the video is not audio.
    UnsupportedTracks,
    /// A single GOP is too long or too large to cut.
    UnsupportedGop,
    /// A part that would be split is larger than a repair reads at once.
    OversizedPart,
    /// Not raised for local files, which have no byte budget.
    ByteBudgetExhausted,
}

#[napi(object)]
pub struct RepairSummary {
    pub outcome: RepairOutcome,
    /// Why the recording was left unchanged, when it was.
    pub reason: Option<RepairSkip>,
    /// The recording's parts.
    pub parts: u32,
    /// The parts rewritten as keyframe-aligned pieces.
    pub parts_split: u32,
    /// The pieces written in their place.
    pub pieces: u32,
}

pub struct Repair {
    input: PathBuf,
    output: PathBuf,
}

impl Task for Repair {
    type Output = RepairSummary;
    type JsValue = RepairSummary;

    fn compute(&mut self) -> Result<RepairSummary> {
        let mut output = LazyFile::new(&self.output);
        let result = catch_unwind(AssertUnwindSafe(|| repair(&self.input, &mut output)))
            .unwrap_or_else(|panic| {
                let message = panic
                    .downcast_ref::<&str>()
                    .map(|message| message.to_string())
                    .or_else(|| panic.downcast_ref::<String>().cloned())
                    .unwrap_or_else(|| "unknown panic".into());
                Err(Error::from_reason(format!(
                    "repairing {} panicked: {message}",
                    self.input.display()
                )))
            });
        if result.is_err() {
            output.discard();
        }
        result
    }

    fn resolve(&mut self, _env: Env, output: RepairSummary) -> Result<RepairSummary> {
        Ok(output)
    }
}

/// Repair the fMP4 recording at `input`, writing the repaired recording to
/// `output` only when there is a repair to make. The output is synced to disk
/// before the promise resolves, ready to be renamed over the input; it is
/// removed again if the repair fails part-way.
#[napi(ts_return_type = "Promise<RepairSummary>")]
pub fn repair_recording(input: String, output: String) -> AsyncTask<Repair> {
    AsyncTask::new(Repair {
        input: input.into(),
        output: output.into(),
    })
}

fn repair(input: &Path, output: &mut LazyFile<'_>) -> Result<RepairSummary> {
    let source = FileSource::open(input).map_err(Error::from_reason)?;
    let options = ProcessorOptions {
        thumbnail: None,
        require_build_codec: false,
        // The byte budget bounds ranged reads from object storage; a local file has none.
        limits: PipelineLimits {
            max_total_bytes_read: u64::MAX,
            ..ProcessorOptions::default().limits
        },
        ..ProcessorOptions::default()
    };
    let identity = SourceIdentity {
        object_key: input.display().to_string(),
        etag: String::new(),
    };
    let repair = futures_executor::block_on(async {
        let index = index_recording(&source, identity, options).await?;
        write_repaired_recording(&source, &index, output).await
    })
    .map_err(|error| Error::from_reason(format!("{}: {error}", input.display())))?;
    output.finish()?;
    Ok(match repair {
        FileRepair::Repaired {
            parts,
            parts_split,
            pieces,
        } => RepairSummary {
            outcome: RepairOutcome::Repaired,
            reason: None,
            parts: count(parts),
            parts_split: count(parts_split),
            pieces: count(pieces),
        },
        FileRepair::Unchanged(reason) => RepairSummary {
            outcome: RepairOutcome::Unchanged,
            reason: Some(skip(reason)),
            parts: 0,
            parts_split: 0,
            pieces: 0,
        },
    })
}

fn count(value: usize) -> u32 {
    u32::try_from(value).unwrap_or(u32::MAX)
}

fn skip(reason: RepairSkipReason) -> RepairSkip {
    match reason {
        RepairSkipReason::NotNeeded => RepairSkip::NotNeeded,
        RepairSkipReason::UnsupportedTracks => RepairSkip::UnsupportedTracks,
        RepairSkipReason::UnsupportedGop => RepairSkip::UnsupportedGop,
        RepairSkipReason::OversizedPart => RepairSkip::OversizedPart,
        RepairSkipReason::ByteBudgetExhausted => RepairSkip::ByteBudgetExhausted,
    }
}

/// The output file, created on the first write, so an unchanged recording leaves
/// nothing behind.
struct LazyFile<'a> {
    path: &'a Path,
    file: Option<BufWriter<File>>,
    created: bool,
}

impl<'a> LazyFile<'a> {
    fn new(path: &'a Path) -> Self {
        Self {
            path,
            file: None,
            created: false,
        }
    }

    fn file(&mut self) -> io::Result<&mut BufWriter<File>> {
        if self.file.is_none() {
            let file = File::create(self.path)?;
            self.created = true;
            self.file = Some(BufWriter::with_capacity(1 << 20, file));
        }
        Ok(self.file.as_mut().expect("output file was just created"))
    }

    /// Flush the output, if it was created, and sync it to disk.
    fn finish(&mut self) -> Result<()> {
        let Some(file) = self.file.as_mut() else {
            return Ok(());
        };
        let path = self.path.display();
        file.flush()
            .and_then(|()| file.get_ref().sync_all())
            .map_err(|error| Error::from_reason(format!("write {path}: {error}")))
    }

    /// Remove the output, if this repair created it.
    fn discard(&mut self) {
        self.file = None;
        if self.created {
            let _ = fs::remove_file(self.path);
        }
    }
}

impl Write for LazyFile<'_> {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        self.file()?.write(bytes)
    }

    fn flush(&mut self) -> io::Result<()> {
        match &mut self.file {
            Some(file) => file.flush(),
            None => Ok(()),
        }
    }
}
