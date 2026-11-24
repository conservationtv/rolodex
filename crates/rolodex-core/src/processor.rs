use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::{
    ByteSource, Error, Result,
    codec::{
        BUILD_CODEC, SelectedDecoder, VideoDecoder, annex_b_access_unit,
        sample_has_random_access_nal,
    },
    jpeg::encode_jpeg,
    mp4::{
        ArchiveMovie, ArchiveTrack, BoxHeader, IndexedFragment, IndexedTrackFragment,
        SampleLocation, TrackKind, build_canonical_init, fourcc_string, parse_archive_moov,
        parse_indexed_fragment,
    },
    reader::{FileCache, FileHandle, PipelineLimits},
    scale::{scale_yuv420, scale_yuv420_to_dimensions},
    sprite::{SpriteBuilder, SpriteLayout},
};

pub const PROCESSOR_VERSION: &str = "8";
pub const METADATA_SCHEMA_VERSION: u32 = 4;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SourceIdentity {
    pub object_key: String,
    pub etag: String,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
pub struct ThumbnailOptions {
    #[serde(
        default,
        with = "crate::decimal_string::optional",
        skip_serializing_if = "Option::is_none"
    )]
    pub interval_ns: Option<u64>,
    pub max_width: u32,
    pub max_height: u32,
    pub quality: u8,
}

impl Default for ThumbnailOptions {
    fn default() -> Self {
        Self {
            interval_ns: None,
            max_width: 480,
            max_height: 270,
            quality: 78,
        }
    }
}

impl ThumbnailOptions {
    pub fn validate(self) -> Result<Self> {
        if self.interval_ns == Some(0) {
            return Err(Error::Invalid("thumbnail interval must be positive"));
        }
        if self.max_width < 2 || self.max_height < 2 {
            return Err(Error::Invalid("thumbnail bounds must be at least 2x2"));
        }
        if self.max_width > u16::MAX as u32 || self.max_height > u16::MAX as u32 {
            return Err(Error::Limit("thumbnail dimensions exceed JPEG limits"));
        }
        if !(1..=100).contains(&self.quality) {
            return Err(Error::Invalid("JPEG quality must be in 1..=100"));
        }
        Ok(self)
    }
}

#[derive(Debug, Clone, Copy)]
pub struct ProcessorOptions {
    pub thumbnail: Option<ThumbnailOptions>,
    pub max_thumbnail_frames: u32,
    pub max_sprite_pixels: u64,
    pub fragment_read_ahead_bytes: usize,
    pub max_samples_indexed: u64,
    pub limits: PipelineLimits,
    /// Whether indexing refuses a recording whose video codec isn't the one this
    /// build decodes. Only thumbnails decode, so a caller that indexes without
    /// rendering them, such as a local repair, can index either codec.
    pub require_build_codec: bool,
}

impl Default for ProcessorOptions {
    fn default() -> Self {
        let limits = PipelineLimits {
            max_samples_decoded: 100_000,
            ..PipelineLimits::default()
        };
        Self {
            thumbnail: Some(ThumbnailOptions::default()),
            max_thumbnail_frames: 1_000,
            max_sprite_pixels: 20_000_000,
            fragment_read_ahead_bytes: 16 * 1024,
            max_samples_indexed: 1_000_000,
            limits,
            require_build_codec: true,
        }
    }
}

impl ProcessorOptions {
    pub fn validate(self) -> Result<Self> {
        if let Some(thumbnail) = self.thumbnail {
            thumbnail.validate()?;
        }
        self.limits.validate()?;
        if self.fragment_read_ahead_bytes < 16 {
            return Err(Error::Invalid(
                "fragment read-ahead must hold a complete box header",
            ));
        }
        if self.max_samples_indexed == 0 {
            return Err(Error::Invalid("maximum indexed sample count is zero"));
        }
        if self.max_thumbnail_frames == 0 || self.max_sprite_pixels < 4 {
            return Err(Error::Invalid(
                "sprite frame and pixel limits must be positive",
            ));
        }
        Ok(self)
    }
}

pub fn parse_duration_ns(value: &str) -> Result<u64> {
    let value = value.trim();
    let split = value
        .find(|character: char| !character.is_ascii_digit())
        .ok_or(Error::Invalid("duration requires a unit such as 10s or 1m"))?;
    let (number, unit) = value.split_at(split);
    if number.is_empty() || unit.is_empty() || unit.chars().any(char::is_whitespace) {
        return Err(Error::Invalid("invalid duration"));
    }
    let number = number
        .parse::<u64>()
        .map_err(|_| Error::Invalid("duration number is invalid"))?;
    if number == 0 {
        return Err(Error::Invalid("duration must be positive"));
    }
    let multiplier = match unit {
        "ns" => 1,
        "us" | "µs" => 1_000,
        "ms" => 1_000_000,
        "s" => 1_000_000_000,
        "m" => 60 * 1_000_000_000,
        "h" => 60 * 60 * 1_000_000_000,
        _ => {
            return Err(Error::Invalid(
                "duration unit must be ns, us, ms, s, m, or h",
            ));
        }
    };
    number
        .checked_mul(multiplier)
        .ok_or(Error::Limit("duration exceeds u64 nanoseconds"))
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct PlaybackPart {
    pub source: String,
    pub offset: u64,
    pub length: u64,
    #[serde(with = "crate::decimal_string::scalar")]
    pub presentation_start_ns: i64,
    pub starts_with_sync: bool,
}

/// Why lossless repair did not produce an alternate playback layout.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum RepairSkipReason {
    /// A track besides the video track is not audio.
    UnsupportedTracks,
    UnsupportedGop,
    NotNeeded,
    OversizedPart,
    ByteBudgetExhausted,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct ProcessorMetrics {
    /// None when repair was not attempted or completed successfully.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub repair_skip_reason: Option<RepairSkipReason>,
    pub source_bytes: u64,
    pub range_requests: u32,
    pub bytes_read: u64,
    pub top_level_boxes_examined: u32,
    pub parts_indexed: u32,
    pub samples_indexed: u64,
    pub thumbnail_samples_examined: u32,
    pub thumbnail_samples_decoded: u32,
    pub thumbnail_frames: u32,
    pub thumbnail_interval_ns: u64,
    pub source_width: u32,
    pub source_height: u32,
    pub source_bit_depth: u8,
    pub thumbnail_width: u32,
    pub thumbnail_height: u32,
    pub thumbnail_cell_width: u32,
    pub thumbnail_cell_height: u32,
    pub thumbnail_columns: u32,
    pub thumbnail_rows: u32,
    pub thumbnail_bytes: u64,
    pub thumbnail_failures: u32,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct RecordingMetadata {
    pub schema_version: u32,
    pub processor_version: String,
    pub source: MetadataSource,
    pub recording: RecordingDetails,
    pub init: InitMetadata,
    pub tracks: Vec<TrackMetadata>,
    pub parts: Vec<PartMetadata>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub playback_parts: Vec<PlaybackPart>,
    pub thumbnail: ThumbnailMetadata,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ThumbnailMetadata {
    #[serde(
        default,
        with = "crate::decimal_string::optional",
        skip_serializing_if = "Option::is_none"
    )]
    pub interval_ns: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
    pub frame_count: u32,
    pub columns: u32,
    pub rows: u32,
    pub frame_width: u32,
    pub frame_height: u32,
    pub sprite_width: u32,
    pub sprite_height: u32,
    pub frames: Vec<ThumbnailFrameMetadata>,
}

impl ThumbnailMetadata {
    /// Metadata for a thumbnail that was not generated, either because
    /// generation was disabled (`error: None`) or because it failed.
    pub fn unavailable(interval_ns: Option<u64>, error: Option<String>) -> Self {
        Self {
            interval_ns,
            error,
            frame_count: 0,
            columns: 0,
            rows: 0,
            frame_width: 0,
            frame_height: 0,
            sprite_width: 0,
            sprite_height: 0,
            frames: Vec::new(),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ThumbnailFrameMetadata {
    pub index: u32,
    #[serde(with = "crate::decimal_string::scalar")]
    pub target_ns: u64,
    #[serde(with = "crate::decimal_string::scalar")]
    pub presentation_time_ns: i64,
    pub sample_offset: u64,
    pub sample_length: u32,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct MetadataSource {
    pub object_key: String,
    pub etag: String,
    pub byte_length: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct RecordingDetails {
    #[serde(with = "crate::decimal_string::scalar")]
    pub video_duration_ns: u64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub stream_id: Option<String>,
    #[serde(
        default,
        with = "crate::decimal_string::optional",
        skip_serializing_if = "Option::is_none"
    )]
    pub segment_number: Option<u64>,
    #[serde(
        default,
        with = "crate::decimal_string::optional",
        skip_serializing_if = "Option::is_none"
    )]
    pub start_dts_ns: Option<i64>,
    #[serde(
        default,
        with = "crate::decimal_string::optional",
        skip_serializing_if = "Option::is_none"
    )]
    pub start_utc_ns: Option<i64>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct InitMetadata {
    pub sha256: String,
    pub byte_length: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct TrackMetadata {
    pub id: u32,
    pub kind: String,
    pub timescale: u32,
    pub codec: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub width: Option<u16>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub height: Option<u16>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct PartMetadata {
    pub offset: u64,
    pub length: u64,
    pub sequence_number: u32,
    #[serde(with = "crate::decimal_string::scalar")]
    pub start_ns: i64,
    #[serde(
        default,
        with = "crate::decimal_string::optional",
        skip_serializing_if = "Option::is_none"
    )]
    pub start_utc_ns: Option<i64>,
    #[serde(with = "crate::decimal_string::scalar")]
    pub duration_ns: u64,
    #[serde(with = "crate::decimal_string::scalar")]
    pub presentation_start_ns: i64,
    #[serde(with = "crate::decimal_string::scalar")]
    pub presentation_duration_ns: u64,
    pub video_starts_with_sync: bool,
    #[serde(
        default,
        with = "crate::decimal_string::optional",
        skip_serializing_if = "Option::is_none"
    )]
    pub first_video_sync_ns: Option<i64>,
    #[serde(
        default,
        with = "crate::decimal_string::optional",
        skip_serializing_if = "Option::is_none"
    )]
    pub first_video_sync_presentation_ns: Option<i64>,
    pub track_fragments: Vec<TrackFragmentMetadata>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct TrackFragmentMetadata {
    pub track_id: u32,
    #[serde(with = "crate::decimal_string::scalar")]
    pub base_decode_time: u64,
    pub sample_count: u32,
    #[serde(with = "crate::decimal_string::scalar")]
    pub decode_start_ns: i64,
    #[serde(with = "crate::decimal_string::scalar")]
    pub decode_duration_ns: u64,
    #[serde(with = "crate::decimal_string::scalar")]
    pub presentation_start_ns: i64,
    #[serde(with = "crate::decimal_string::scalar")]
    pub presentation_duration_ns: u64,
    #[serde(
        default,
        with = "crate::decimal_string::optional",
        skip_serializing_if = "Option::is_none"
    )]
    pub first_sync_decode_ns: Option<i64>,
    #[serde(
        default,
        with = "crate::decimal_string::optional",
        skip_serializing_if = "Option::is_none"
    )]
    pub first_sync_presentation_ns: Option<i64>,
}

struct TrackFragmentSummary {
    metadata: TrackFragmentMetadata,
    decode_start_ns: i64,
    decode_end_ns: i64,
    presentation_start_ns: i64,
    presentation_end_ns: i64,
}

#[derive(Debug, Clone)]
pub struct ProcessorOutput {
    pub thumbnail_jpeg: Option<Vec<u8>>,
    pub metadata: RecordingMetadata,
    pub init_mp4: Vec<u8>,
    pub metrics: ProcessorMetrics,
}

/// Metadata and decoder state produced without reading video sample payloads.
///
/// The public fields are the durable index artifacts. Private fields retain the
/// parsed information needed for later passes over the same source: thumbnails
/// here, and repairs in `rolodex-repair`, which reads them through the accessors.
pub struct RecordingIndex {
    pub metadata: RecordingMetadata,
    pub init_mp4: Vec<u8>,
    pub metrics: ProcessorMetrics,
    movie: ArchiveMovie,
    video_samples: Vec<TimedVideoSample>,
    video_timing: VideoTiming,
    options: ProcessorOptions,
    cache: FileCache,
}

impl RecordingIndex {
    /// The parsed movie header: its tracks and their fragment defaults.
    pub fn movie(&self) -> &ArchiveMovie {
        &self.movie
    }

    /// Every video sample of the recording, in decode order.
    pub fn video_samples(&self) -> &[TimedVideoSample] {
        &self.video_samples
    }

    /// The validated options the recording was indexed with.
    pub fn options(&self) -> ProcessorOptions {
        self.options
    }

    /// The finished output: the index's artifacts, with the sprite rendered for it.
    pub fn into_output(self, thumbnail_jpeg: Option<Vec<u8>>) -> ProcessorOutput {
        ProcessorOutput {
            thumbnail_jpeg,
            metadata: self.metadata,
            init_mp4: self.init_mp4,
            metrics: self.metrics,
        }
    }
}

struct PendingFragment {
    header: BoxHeader,
    fragment: IndexedFragment,
}

/// A video sample on the recording's decode timeline, and where its payload is.
#[derive(Debug, Clone, Copy)]
pub struct TimedVideoSample {
    pub location: SampleLocation,
    pub decode_start_ns: i64,
    decode_end_ns: i64,
    presentation_start_ns: i64,
}

#[derive(Debug, Clone, Copy)]
struct VideoTiming {
    duration_ns: u64,
    presentation_origin_ns: i64,
}

pub async fn process_recording<S: ByteSource + ?Sized>(
    source: &S,
    identity: SourceIdentity,
    options: ProcessorOptions,
) -> Result<ProcessorOutput> {
    process_recording_with_decoder::<S, SelectedDecoder>(source, identity, options).await
}

async fn process_recording_with_decoder<S, D>(
    source: &S,
    identity: SourceIdentity,
    options: ProcessorOptions,
) -> Result<ProcessorOutput>
where
    S: ByteSource + ?Sized,
    D: VideoDecoder,
{
    let mut index = index_recording_inner(source, identity, options).await?;
    let thumbnail_jpeg = render_recording_thumbnail_with_decoder::<S, D>(source, &mut index).await;
    Ok(index.into_output(thumbnail_jpeg))
}

/// Render the sprite the index's options ask for, recording its metadata and
/// read metrics in the index. Best-effort: a failure leaves the index with an
/// unavailable thumbnail carrying the error, and no sprite.
pub async fn render_recording_thumbnail<S: ByteSource + ?Sized>(
    source: &S,
    index: &mut RecordingIndex,
) -> Option<Vec<u8>> {
    render_recording_thumbnail_with_decoder::<S, SelectedDecoder>(source, index).await
}

async fn render_recording_thumbnail_with_decoder<S: ByteSource + ?Sized, D: VideoDecoder>(
    source: &S,
    index: &mut RecordingIndex,
) -> Option<Vec<u8>> {
    let thumbnail_options = index.options.thumbnail;
    let mut metrics = index.metrics.clone();
    let (thumbnail_jpeg, thumbnail_metadata) = if let Some(thumbnail_options) = thumbnail_options {
        match render_thumbnail_sprite_with_decoder::<S, D>(
            source,
            index,
            thumbnail_options,
            None,
            &mut metrics,
        )
        .await
        {
            Ok((jpeg, metadata)) => (Some(jpeg), metadata),
            Err(error) => {
                metrics.thumbnail_failures = 1;
                (
                    None,
                    ThumbnailMetadata::unavailable(
                        thumbnail_options.interval_ns,
                        Some(error.to_string()),
                    ),
                )
            }
        }
    } else {
        (None, ThumbnailMetadata::unavailable(None, None))
    };
    metrics.thumbnail_bytes = thumbnail_jpeg.as_ref().map_or(0, |jpeg| jpeg.len() as u64);
    index.metrics = metrics;
    index.metadata.thumbnail = thumbnail_metadata;
    thumbnail_jpeg
}

pub async fn index_recording<S: ByteSource + ?Sized>(
    source: &S,
    identity: SourceIdentity,
    options: ProcessorOptions,
) -> Result<RecordingIndex> {
    index_recording_inner(source, identity, options).await
}

async fn index_recording_inner<S>(
    source: &S,
    identity: SourceIdentity,
    options: ProcessorOptions,
) -> Result<RecordingIndex>
where
    S: ByteSource + ?Sized,
{
    let options = options.validate()?;
    if source.is_empty() {
        return Err(Error::Invalid("source object is empty"));
    }
    let mut metrics = ProcessorMetrics {
        source_bytes: source.len(),
        ..ProcessorMetrics::default()
    };
    let mut reader = FileHandle::new(source, options.limits).await?;
    let mut offset = 0;
    let mut saw_ftyp = false;
    let mut movie = None;
    let mut moov_bytes = None;
    let mut pending = None;
    let mut parts = Vec::new();
    let mut video_samples = Vec::new();

    while offset < reader.len() {
        if metrics.top_level_boxes_examined >= options.limits.max_top_level_boxes {
            return Err(Error::Limit("too many top-level boxes"));
        }
        let header = reader
            .top_level_header_read_ahead(offset, options.fragment_read_ahead_bytes)
            .await?;
        offset = header.end()?;
        metrics.top_level_boxes_examined += 1;

        if pending.is_some() && header.kind != *b"mdat" {
            return Err(Error::Invalid("moof is not immediately followed by mdat"));
        }

        match &header.kind {
            b"ftyp" => {
                if saw_ftyp || movie.is_some() {
                    return Err(Error::Invalid("unexpected ftyp position"));
                }
                saw_ftyp = true;
            }
            b"moov" => {
                if !saw_ftyp || movie.is_some() || !parts.is_empty() {
                    return Err(Error::Invalid("unexpected moov position"));
                }
                let size = usize::try_from(header.size)
                    .map_err(|_| Error::Limit("moov size does not fit memory"))?;
                if size > options.limits.max_init_bytes {
                    return Err(Error::Limit("moov box is too large"));
                }
                let bytes = reader.read(header.start, size).await?;
                let parsed = parse_archive_moov(&bytes)?;
                if options.require_build_codec && parsed.video.config.codec != BUILD_CODEC {
                    return Err(Error::CodecMismatch {
                        expected: BUILD_CODEC,
                        actual: parsed.video.config.codec,
                    });
                }
                movie = Some(parsed);
                moov_bytes = Some(bytes);
            }
            b"moof" => {
                let movie = movie
                    .as_ref()
                    .ok_or(Error::Unsupported("moof appears before moov"))?;
                if pending.is_some() {
                    return Err(Error::Invalid("consecutive moof boxes without mdat"));
                }
                metrics.parts_indexed = metrics.parts_indexed.saturating_add(1);
                if metrics.parts_indexed > options.limits.max_fragments {
                    return Err(Error::Limit("too many fragments in recording"));
                }
                let size = usize::try_from(header.size)
                    .map_err(|_| Error::Limit("fragment size does not fit memory"))?;
                if size > options.limits.max_fragment_bytes {
                    return Err(Error::Limit("fragment metadata box is too large"));
                }
                let bytes = reader.read(header.start, size).await?;
                let defaults = movie
                    .tracks
                    .iter()
                    .map(|track| (track.track_id, track.defaults))
                    .collect::<Vec<_>>();
                let fragment = parse_indexed_fragment(&bytes, header.start, &defaults)?;
                metrics.samples_indexed = metrics
                    .samples_indexed
                    .checked_add(
                        fragment
                            .tracks
                            .iter()
                            .map(|track| track.samples.len() as u64)
                            .sum::<u64>(),
                    )
                    .ok_or(Error::Limit("indexed sample counter overflow"))?;
                if metrics.samples_indexed > options.max_samples_indexed {
                    return Err(Error::Limit("too many samples in recording"));
                }
                pending = Some(PendingFragment { header, fragment });
            }
            b"mdat" => {
                let pending_fragment = pending
                    .take()
                    .ok_or(Error::Invalid("mdat is not preceded by moof"))?;
                let movie = movie
                    .as_ref()
                    .ok_or(Error::Unsupported("mdat appears before moov"))?;
                if pending_fragment.header.end()? != header.start {
                    return Err(Error::Invalid("moof and mdat are not byte-contiguous"));
                }
                validate_sample_ranges(&pending_fragment.fragment, header)?;
                collect_video_samples(&pending_fragment.fragment, movie, &mut video_samples)?;
                parts.push(summarise_part(&pending_fragment, header, movie)?);
            }
            _ => {}
        }
    }

    if pending.is_some() {
        return Err(Error::Invalid("recording ends after moof without mdat"));
    }
    let movie = movie.ok_or(Error::Invalid("source has no moov box"))?;
    let moov_bytes = moov_bytes.ok_or(Error::Invalid("source has no initialization section"))?;
    if parts.is_empty() {
        return Err(Error::Invalid("source contains no media parts"));
    }
    let mut active_track_ids = Vec::new();
    active_track_ids
        .try_reserve_exact(movie.tracks.len())
        .map_err(|_| Error::Limit("active track allocation failed"))?;
    for fragment in parts.iter().flat_map(|part| &part.track_fragments) {
        if !active_track_ids.contains(&fragment.track_id) {
            active_track_ids.push(fragment.track_id);
        }
    }
    active_track_ids.sort_unstable();
    active_track_ids.dedup();
    let init_mp4 = build_canonical_init(&moov_bytes, &active_track_ids)?;
    let video_timing = video_timing(&video_samples)?;
    metrics.range_requests = reader.requests();
    metrics.bytes_read = reader.bytes_read();
    let cache = reader.cache();

    let init_hash = hex(&Sha256::digest(&init_mp4));
    let metadata = RecordingMetadata {
        schema_version: METADATA_SCHEMA_VERSION,
        processor_version: PROCESSOR_VERSION.into(),
        source: MetadataSource {
            object_key: identity.object_key,
            etag: identity.etag,
            byte_length: source.len(),
        },
        recording: RecordingDetails {
            video_duration_ns: video_timing.duration_ns,
            stream_id: movie
                .media_mtx
                .as_ref()
                .map(|value| uuid_string(&value.stream_id)),
            segment_number: movie.media_mtx.as_ref().map(|value| value.segment_number),
            start_dts_ns: movie.media_mtx.as_ref().map(|value| value.start_dts_ns),
            start_utc_ns: movie.media_mtx.as_ref().map(|value| value.start_ntp_ns),
        },
        init: InitMetadata {
            sha256: init_hash,
            byte_length: init_mp4.len() as u64,
        },
        tracks: movie
            .tracks
            .iter()
            .filter(|track| active_track_ids.binary_search(&track.track_id).is_ok())
            .map(track_metadata)
            .collect(),
        parts,
        playback_parts: Vec::new(),
        thumbnail: ThumbnailMetadata::unavailable(None, None),
    };

    Ok(RecordingIndex {
        metadata,
        init_mp4,
        metrics,
        movie,
        video_samples,
        video_timing,
        options,
        cache,
    })
}

fn validate_sample_ranges(fragment: &IndexedFragment, mdat: BoxHeader) -> Result<()> {
    let payload_start = mdat
        .start
        .checked_add(mdat.header_size)
        .ok_or(Error::Invalid("mdat payload offset overflow"))?;
    let end = mdat.end()?;
    for track in &fragment.tracks {
        for sample in &track.samples {
            let sample_end = sample
                .location
                .offset
                .checked_add(u64::from(sample.location.size))
                .ok_or(Error::Invalid("sample end overflow"))?;
            if sample.location.offset < payload_start || sample_end > end {
                return Err(Error::Invalid("fragment sample is outside its mdat"));
            }
        }
    }
    Ok(())
}

fn summarise_part(
    pending: &PendingFragment,
    mdat: BoxHeader,
    movie: &ArchiveMovie,
) -> Result<PartMetadata> {
    let mut track_fragments = Vec::new();
    let mut decode_start = i64::MAX;
    let mut decode_end = i64::MIN;
    let mut presentation_start = i64::MAX;
    let mut presentation_end = i64::MIN;
    let mut video_starts_with_sync = false;
    let mut first_video_sync = None;
    let mut first_video_sync_presentation = None;

    for fragment in &pending.fragment.tracks {
        let track = movie
            .tracks
            .iter()
            .find(|track| track.track_id == fragment.track_id)
            .ok_or(Error::Invalid("fragment references an unknown track"))?;
        let summary = summarise_track_fragment(fragment, track)?;
        decode_start = decode_start.min(summary.decode_start_ns);
        decode_end = decode_end.max(summary.decode_end_ns);
        presentation_start = presentation_start.min(summary.presentation_start_ns);
        presentation_end = presentation_end.max(summary.presentation_end_ns);
        if track.track_id == movie.video.track_id {
            video_starts_with_sync = fragment
                .samples
                .first()
                .is_some_and(|sample| sample.location.is_sync);
            first_video_sync = summary.metadata.first_sync_decode_ns;
            first_video_sync_presentation = summary.metadata.first_sync_presentation_ns;
        }
        track_fragments.push(summary.metadata);
    }
    if decode_start == i64::MAX || decode_end <= decode_start {
        return Err(Error::Invalid("part has no positive decode interval"));
    }
    if presentation_start == i64::MAX || presentation_end <= presentation_start {
        return Err(Error::Invalid("part has no positive presentation interval"));
    }
    let duration = u64::try_from(decode_end - decode_start)
        .map_err(|_| Error::Invalid("part decode duration is negative"))?;
    let presentation_duration = u64::try_from(presentation_end - presentation_start)
        .map_err(|_| Error::Invalid("part presentation duration is negative"))?;
    let start_utc_ns = movie
        .media_mtx
        .as_ref()
        .map(|value| {
            value
                .start_ntp_ns
                .checked_add(decode_start)
                .ok_or(Error::Invalid("part wall-clock time overflow"))
        })
        .transpose()?;
    let length = mdat
        .end()?
        .checked_sub(pending.header.start)
        .ok_or(Error::Invalid("part byte range underflow"))?;
    Ok(PartMetadata {
        offset: pending.header.start,
        length,
        sequence_number: pending.fragment.sequence_number,
        start_ns: decode_start,
        start_utc_ns,
        duration_ns: duration,
        presentation_start_ns: presentation_start,
        presentation_duration_ns: presentation_duration,
        video_starts_with_sync,
        first_video_sync_ns: first_video_sync,
        first_video_sync_presentation_ns: first_video_sync_presentation,
        track_fragments,
    })
}

fn summarise_track_fragment(
    fragment: &IndexedTrackFragment,
    track: &ArchiveTrack,
) -> Result<TrackFragmentSummary> {
    let mut decode_time = i128::from(fragment.base_decode_time);
    let decode_start_ticks = decode_time;
    let mut presentation_start_ticks = i128::MAX;
    let mut presentation_end_ticks = i128::MIN;
    let mut first_sync_decode = None;
    let mut first_sync_presentation = None;
    for sample in &fragment.samples {
        let presentation = decode_time
            .checked_add(i128::from(sample.composition_time_offset))
            .ok_or(Error::Invalid("sample presentation time overflow"))?;
        let sample_end = presentation
            .checked_add(i128::from(sample.duration))
            .ok_or(Error::Invalid("sample presentation end overflow"))?;
        presentation_start_ticks = presentation_start_ticks.min(presentation);
        presentation_end_ticks = presentation_end_ticks.max(sample_end);
        if track.kind == TrackKind::Video && sample.location.is_sync && first_sync_decode.is_none()
        {
            first_sync_decode = Some(ticks_to_ns(decode_time, track.timescale)?);
            first_sync_presentation = Some(ticks_to_ns(presentation, track.timescale)?);
        }
        decode_time = decode_time
            .checked_add(i128::from(sample.duration))
            .ok_or(Error::Invalid("sample decode time overflow"))?;
    }
    let decode_start_ns = ticks_to_ns(decode_start_ticks, track.timescale)?;
    let decode_end_ns = ticks_to_ns(decode_time, track.timescale)?;
    let presentation_start_ns = ticks_to_ns(presentation_start_ticks, track.timescale)?;
    let presentation_end_ns = ticks_to_ns(presentation_end_ticks, track.timescale)?;
    if decode_end_ns <= decode_start_ns {
        return Err(Error::Invalid(
            "track fragment has no positive decode duration",
        ));
    }
    if presentation_end_ns <= presentation_start_ns {
        return Err(Error::Invalid("track fragment has no positive duration"));
    }
    Ok(TrackFragmentSummary {
        metadata: TrackFragmentMetadata {
            track_id: fragment.track_id,
            base_decode_time: fragment.base_decode_time,
            sample_count: u32::try_from(fragment.samples.len())
                .map_err(|_| Error::Limit("track fragment sample count exceeds u32"))?,
            decode_start_ns,
            decode_duration_ns: u64::try_from(decode_end_ns - decode_start_ns)
                .map_err(|_| Error::Invalid("track fragment decode duration is negative"))?,
            presentation_start_ns,
            presentation_duration_ns: u64::try_from(presentation_end_ns - presentation_start_ns)
                .map_err(|_| Error::Invalid("track fragment duration is negative"))?,
            first_sync_decode_ns: first_sync_decode,
            first_sync_presentation_ns: first_sync_presentation,
        },
        decode_start_ns,
        decode_end_ns,
        presentation_start_ns,
        presentation_end_ns,
    })
}

fn ticks_to_ns(ticks: i128, timescale: u32) -> Result<i64> {
    let value = ticks
        .checked_mul(1_000_000_000)
        .ok_or(Error::Invalid("timestamp conversion overflow"))?
        / i128::from(timescale);
    i64::try_from(value).map_err(|_| Error::Limit("timestamp exceeds i64 nanoseconds"))
}

fn collect_video_samples(
    fragment: &IndexedFragment,
    movie: &ArchiveMovie,
    output: &mut Vec<TimedVideoSample>,
) -> Result<()> {
    let Some(video) = fragment
        .tracks
        .iter()
        .find(|track| track.track_id == movie.video.track_id)
    else {
        return Ok(());
    };
    let track = movie
        .tracks
        .iter()
        .find(|track| track.track_id == movie.video.track_id)
        .ok_or(Error::Invalid("movie is missing its video track"))?;
    let mut decode_time = i128::from(video.base_decode_time);
    for sample in &video.samples {
        let decode_end = decode_time
            .checked_add(i128::from(sample.duration))
            .ok_or(Error::Invalid("video sample decode time overflow"))?;
        let presentation = decode_time
            .checked_add(i128::from(sample.composition_time_offset))
            .ok_or(Error::Invalid("video sample presentation time overflow"))?;
        output.push(TimedVideoSample {
            location: sample.location,
            decode_start_ns: ticks_to_ns(decode_time, track.timescale)?,
            decode_end_ns: ticks_to_ns(decode_end, track.timescale)?,
            presentation_start_ns: ticks_to_ns(presentation, track.timescale)?,
        });
        decode_time = decode_end;
    }
    Ok(())
}

pub async fn render_thumbnail_sprite<S: ByteSource + ?Sized>(
    source: &S,
    index: &RecordingIndex,
    options: ThumbnailOptions,
) -> Result<(Vec<u8>, ThumbnailMetadata)> {
    let mut metrics = index.metrics.clone();
    render_thumbnail_sprite_with_decoder::<S, SelectedDecoder>(
        source,
        index,
        options,
        None,
        &mut metrics,
    )
    .await
}

/// Render a sprite while expressing frame presentation times relative to an
/// explicit origin. Sample selection still follows the indexed media timeline.
pub async fn render_thumbnail_sprite_with_presentation_origin<S: ByteSource + ?Sized>(
    source: &S,
    index: &RecordingIndex,
    options: ThumbnailOptions,
    presentation_origin_ns: i64,
) -> Result<(Vec<u8>, ThumbnailMetadata)> {
    let mut metrics = index.metrics.clone();
    render_thumbnail_sprite_with_decoder::<S, SelectedDecoder>(
        source,
        index,
        options,
        Some(presentation_origin_ns),
        &mut metrics,
    )
    .await
}

async fn render_thumbnail_sprite_with_decoder<S, D>(
    source: &S,
    index: &RecordingIndex,
    thumbnail: ThumbnailOptions,
    metadata_origin_ns: Option<i64>,
    metrics: &mut ProcessorMetrics,
) -> Result<(Vec<u8>, ThumbnailMetadata)>
where
    S: ByteSource + ?Sized,
    D: VideoDecoder,
{
    let thumbnail = thumbnail.validate()?;
    let mut reader = FileHandle::from_cache(
        source,
        index.options.limits,
        &index.cache,
        metrics.range_requests,
        metrics.bytes_read,
    )?;
    let result =
        build_thumbnail_sprite::<S, D>(&mut reader, index, metrics, thumbnail, metadata_origin_ns)
            .await;
    metrics.range_requests = reader.requests();
    metrics.bytes_read = reader.bytes_read();
    result
}

async fn build_thumbnail_sprite<S, D>(
    reader: &mut FileHandle<'_, S>,
    index: &RecordingIndex,
    metrics: &mut ProcessorMetrics,
    thumbnail: ThumbnailOptions,
    metadata_origin_ns: Option<i64>,
) -> Result<(Vec<u8>, ThumbnailMetadata)>
where
    S: ByteSource + ?Sized,
    D: VideoDecoder,
{
    let movie = &index.movie;
    let samples = &index.video_samples;
    let timing = index.video_timing;
    let options = index.options;
    let video_duration_ns = timing.duration_ns;
    let presentation_origin = timing.presentation_origin_ns;
    let frame_count_u64 = thumbnail
        .interval_ns
        .map_or(1, |interval| (video_duration_ns / interval).max(1));
    if frame_count_u64 > u64::from(options.max_thumbnail_frames) {
        return Err(Error::Limit("thumbnail frame count exceeds job limit"));
    }
    let frame_count = u32::try_from(frame_count_u64)
        .map_err(|_| Error::Limit("thumbnail frame count exceeds u32"))?;
    let interval_ns = thumbnail.interval_ns.unwrap_or(video_duration_ns);
    metrics.thumbnail_interval_ns = thumbnail.interval_ns.unwrap_or(0);

    let mut presentation_order = (0..samples.len()).collect::<Vec<_>>();
    presentation_order.sort_by_key(|&index| (samples[index].presentation_start_ns, index));
    let mut presentation_cursor = 0usize;
    let mut selections = Vec::new();
    selections
        .try_reserve_exact(frame_count as usize)
        .map_err(|_| Error::Limit("thumbnail selection allocation failed"))?;
    for index in 0..frame_count {
        let elapsed = u64::from(index)
            .checked_mul(interval_ns)
            .ok_or(Error::Limit("thumbnail target time overflow"))?;
        let target = presentation_origin
            .checked_add(
                i64::try_from(elapsed)
                    .map_err(|_| Error::Limit("thumbnail target exceeds i64 nanoseconds"))?,
            )
            .ok_or(Error::Limit("thumbnail presentation target overflow"))?;
        while presentation_cursor + 1 < presentation_order.len()
            && samples[presentation_order[presentation_cursor + 1]].presentation_start_ns <= target
        {
            presentation_cursor += 1;
        }
        selections.push((index, elapsed, presentation_order[presentation_cursor]));
    }

    let mut frame_metadata = Vec::new();
    frame_metadata
        .try_reserve_exact(frame_count as usize)
        .map_err(|_| Error::Limit("thumbnail metadata allocation failed"))?;
    let mut selections = selections.into_iter();
    let (index, elapsed, sample_index) = selections
        .next()
        .expect("positive frame count produces a selection");
    let frame =
        decode_thumbnail_sample::<S, D>(reader, movie, samples, sample_index, metrics, options)
            .await?;
    frame.validate()?;
    metrics.source_width = frame.width;
    metrics.source_height = frame.height;
    metrics.source_bit_depth = frame.bit_depth;
    let (scaled, layout) = scale_first_sprite_frame(&frame, frame_count, options, thumbnail)?;
    metrics.thumbnail_width = layout.width;
    metrics.thumbnail_height = layout.height;
    metrics.thumbnail_cell_width = layout.cell_width;
    metrics.thumbnail_cell_height = layout.cell_height;
    metrics.thumbnail_columns = layout.columns;
    metrics.thumbnail_rows = layout.rows;
    let mut builder = SpriteBuilder::new(layout, frame_count)?;
    builder.push(&scaled)?;
    metrics.thumbnail_frames = metrics.thumbnail_frames.saturating_add(1);
    let selected = samples[sample_index];
    frame_metadata.push(ThumbnailFrameMetadata {
        index,
        target_ns: elapsed,
        presentation_time_ns: rebase_presentation_time(
            selected.presentation_start_ns,
            metadata_origin_ns,
        )?,
        sample_offset: selected.location.offset,
        sample_length: selected.location.size,
    });

    for (index, elapsed, sample_index) in selections {
        let frame =
            decode_thumbnail_sample::<S, D>(reader, movie, samples, sample_index, metrics, options)
                .await?;
        frame.validate()?;
        if frame.width != metrics.source_width
            || frame.height != metrics.source_height
            || frame.bit_depth != metrics.source_bit_depth
        {
            return Err(Error::Unsupported(
                "decoded video format changed within recording",
            ));
        }
        let scaled = scale_yuv420_to_dimensions(&frame, layout.cell_width, layout.cell_height)?;
        builder.push(&scaled)?;
        metrics.thumbnail_frames = metrics.thumbnail_frames.saturating_add(1);
        let selected = samples[sample_index];
        frame_metadata.push(ThumbnailFrameMetadata {
            index,
            target_ns: elapsed,
            presentation_time_ns: rebase_presentation_time(
                selected.presentation_start_ns,
                metadata_origin_ns,
            )?,
            sample_offset: selected.location.offset,
            sample_length: selected.location.size,
        });
    }

    let sheet = builder.finish()?;
    let jpeg = encode_jpeg(&sheet, thumbnail.quality)?;
    Ok((
        jpeg,
        ThumbnailMetadata {
            interval_ns: thumbnail.interval_ns,
            error: None,
            frame_count,
            columns: layout.columns,
            rows: layout.rows,
            frame_width: layout.cell_width,
            frame_height: layout.cell_height,
            sprite_width: layout.width,
            sprite_height: layout.height,
            frames: frame_metadata,
        },
    ))
}

fn rebase_presentation_time(value: i64, origin: Option<i64>) -> Result<i64> {
    origin.map_or(Ok(value), |origin| {
        value
            .checked_sub(origin)
            .ok_or(Error::Invalid("thumbnail presentation time underflow"))
    })
}

fn video_timing(samples: &[TimedVideoSample]) -> Result<VideoTiming> {
    let first_sample = samples
        .first()
        .ok_or(Error::Invalid("recording contains no video samples"))?;
    let (decode_start, decode_end, presentation_origin) = samples[1..].iter().fold(
        (
            first_sample.decode_start_ns,
            first_sample.decode_end_ns,
            first_sample.presentation_start_ns,
        ),
        |(start, end, presentation), sample| {
            (
                start.min(sample.decode_start_ns),
                end.max(sample.decode_end_ns),
                presentation.min(sample.presentation_start_ns),
            )
        },
    );
    let video_duration_ns = u64::try_from(
        decode_end
            .checked_sub(decode_start)
            .ok_or(Error::Invalid("video duration overflow"))?,
    )
    .map_err(|_| Error::Invalid("video duration is negative"))?;
    if video_duration_ns == 0 {
        return Err(Error::Invalid("video has zero duration"));
    }
    Ok(VideoTiming {
        duration_ns: video_duration_ns,
        presentation_origin_ns: presentation_origin,
    })
}

fn scale_first_sprite_frame(
    frame: &crate::frame::DecodedFrame,
    frame_count: u32,
    options: ProcessorOptions,
    thumbnail: ThumbnailOptions,
) -> Result<(crate::scale::ScaledYuv420, SpriteLayout)> {
    let mut max_width = thumbnail.max_width;
    let mut max_height = thumbnail.max_height;
    for _ in 0..8 {
        let scaled = scale_yuv420(frame, max_width, max_height)?;
        if let Ok(layout) = SpriteLayout::new(
            frame_count,
            scaled.width,
            scaled.height,
            options.max_sprite_pixels,
        ) {
            return Ok((scaled, layout));
        }

        let unconstrained = SpriteLayout::new(frame_count, scaled.width, scaled.height, u64::MAX)?;
        let slots = u64::from(unconstrained.columns)
            .checked_mul(u64::from(unconstrained.rows))
            .ok_or(Error::Limit("sprite grid size overflow"))?;
        let allowed_cell_pixels = options.max_sprite_pixels / slots;
        if allowed_cell_pixels < 4 {
            return Err(Error::Limit(
                "sprite pixel limit cannot hold all thumbnail frames",
            ));
        }
        let current_cell_pixels = u64::from(scaled.width) * u64::from(scaled.height);
        let ratio = (allowed_cell_pixels as f64 / current_cell_pixels as f64)
            .sqrt()
            .min(0.95);
        let mut next_width = ((f64::from(scaled.width) * ratio) as u32) & !1;
        let mut next_height = ((f64::from(scaled.height) * ratio) as u32) & !1;
        if next_width >= scaled.width {
            next_width = scaled.width.saturating_sub(2) & !1;
        }
        if next_height >= scaled.height {
            next_height = scaled.height.saturating_sub(2) & !1;
        }
        if next_width < 2 || next_height < 2 {
            return Err(Error::Limit(
                "sprite pixel limit produces sub-2px thumbnail cells",
            ));
        }
        max_width = next_width;
        max_height = next_height;
    }
    Err(Error::Limit(
        "could not fit sprite within its pixel and JPEG limits",
    ))
}

async fn decode_thumbnail_sample<S, D>(
    reader: &mut FileHandle<'_, S>,
    movie: &ArchiveMovie,
    samples: &[TimedVideoSample],
    selected_index: usize,
    metrics: &mut ProcessorMetrics,
    options: ProcessorOptions,
) -> Result<crate::frame::DecodedFrame>
where
    S: ByteSource + ?Sized,
    D: VideoDecoder,
{
    let mut search_end = selected_index
        .checked_add(1)
        .ok_or(Error::Limit("thumbnail sample index overflow"))?;
    let selected_presentation = samples[selected_index].presentation_start_ns;
    let (decode_start, first_bytes) = loop {
        let Some(start) = samples[..search_end].iter().rposition(|sample| {
            sample.location.is_sync && sample.presentation_start_ns <= selected_presentation
        }) else {
            return Err(Error::NoRandomAccessPicture);
        };
        let bytes =
            read_thumbnail_sample(reader, samples[start].location, metrics, options).await?;
        if sample_has_random_access_nal(
            &bytes,
            movie.video.config.nal_length_size,
            movie.video.config.codec,
        )? {
            break (start, bytes);
        }
        if start == 0 {
            return Err(Error::NoRandomAccessPicture);
        }
        search_end = start;
    };

    let mut decoder = D::default();
    decoder.initialise(&movie.video.config)?;
    let mut first_bytes = Some(first_bytes);
    for index in decode_start..samples.len() {
        // Pictures leading an open-GOP random-access point occur later in
        // decode order but earlier in presentation order. They are not part of
        // a fresh decode sequence and some strict decoders reject their absent
        // pre-random-access references.
        if index != decode_start
            && samples[index].presentation_start_ns < samples[decode_start].presentation_start_ns
        {
            continue;
        }
        let bytes = if index == decode_start {
            first_bytes
                .take()
                .ok_or(Error::Invalid("random-access sample is missing"))?
        } else {
            read_thumbnail_sample(reader, samples[index].location, metrics, options).await?
        };
        let annex_b = annex_b_access_unit(&bytes, movie.video.config.nal_length_size)?;
        metrics.thumbnail_samples_decoded = metrics.thumbnail_samples_decoded.saturating_add(1);
        if metrics.thumbnail_samples_decoded > options.limits.max_samples_decoded {
            return Err(Error::Limit(
                "too many access units decoded while generating sprite",
            ));
        }
        let frame = decoder.push_access_unit(&annex_b)?;
        if index >= selected_index
            && let Some(frame) = frame
        {
            return Ok(frame);
        }
    }
    Err(Error::Decode(
        "decoder produced no frame for selected thumbnail sample".into(),
    ))
}

async fn read_thumbnail_sample<S: ByteSource + ?Sized>(
    reader: &mut FileHandle<'_, S>,
    location: SampleLocation,
    metrics: &mut ProcessorMetrics,
    options: ProcessorOptions,
) -> Result<Vec<u8>> {
    metrics.thumbnail_samples_examined = metrics.thumbnail_samples_examined.saturating_add(1);
    if metrics.thumbnail_samples_examined > options.limits.max_samples_examined {
        return Err(Error::Limit(
            "too many video samples examined while generating sprite",
        ));
    }
    let size = usize::try_from(location.size)
        .map_err(|_| Error::Limit("sample size does not fit memory"))?;
    if size > options.limits.max_sample_bytes {
        return Err(Error::Limit("candidate video sample is too large"));
    }
    reader
        .read_ahead(location.offset, size, options.fragment_read_ahead_bytes)
        .await
}

fn track_metadata(track: &ArchiveTrack) -> TrackMetadata {
    TrackMetadata {
        id: track.track_id,
        kind: track.kind.as_str().into(),
        timescale: track.timescale,
        codec: fourcc_string(track.codec),
        width: track.width,
        height: track.height,
    }
}

pub fn hex(bytes: &[u8]) -> String {
    const DIGITS: &[u8; 16] = b"0123456789abcdef";
    let mut output = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        output.push(char::from(DIGITS[usize::from(byte >> 4)]));
        output.push(char::from(DIGITS[usize::from(byte & 0x0f)]));
    }
    output
}

fn uuid_string(bytes: &[u8; 16]) -> String {
    let value = hex(bytes);
    format!(
        "{}-{}-{}-{}-{}",
        &value[0..8],
        &value[8..12],
        &value[12..16],
        &value[16..20],
        &value[20..32]
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        CodecConfig,
        frame::{DecodedFrame, PlaneData},
        source::MemorySource,
    };

    #[derive(Default)]
    struct OneFrameDelayedDecoder {
        pushes: u32,
    }

    impl VideoDecoder for OneFrameDelayedDecoder {
        fn initialise(&mut self, _config: &CodecConfig) -> Result<()> {
            Ok(())
        }

        fn push_access_unit(&mut self, _annex_b: &[u8]) -> Result<Option<DecodedFrame>> {
            self.pushes += 1;
            if self.pushes < 2 {
                return Ok(None);
            }
            Ok(Some(DecodedFrame {
                width: 4,
                height: 4,
                bit_depth: 8,
                y: PlaneData::U8(vec![96; 16]),
                u: PlaneData::U8(vec![128; 4]),
                v: PlaneData::U8(vec![128; 4]),
            }))
        }
    }

    #[derive(Default)]
    struct FailingDecoder;

    impl VideoDecoder for FailingDecoder {
        fn initialise(&mut self, _config: &CodecConfig) -> Result<()> {
            Ok(())
        }

        fn push_access_unit(&mut self, _annex_b: &[u8]) -> Result<Option<DecodedFrame>> {
            Err(Error::Decode("fixture decoder failure".into()))
        }
    }

    fn synthetic_source() -> MemorySource {
        #[cfg(feature = "codec-h264")]
        let relative = "../../fixtures/h264/synthetic-320x180.mp4";
        #[cfg(feature = "codec-h265")]
        let relative = "../../fixtures/h265/synthetic-320x180.mp4";
        let bytes = std::fs::read(format!("{}/{}", env!("CARGO_MANIFEST_DIR"), relative)).unwrap();
        MemorySource::new(bytes)
    }

    fn synthetic_options() -> ProcessorOptions {
        ProcessorOptions::default()
    }

    fn synthetic_identity() -> SourceIdentity {
        SourceIdentity {
            object_key: "fixture.mp4".into(),
            etag: "fixture".into(),
        }
    }

    #[test]
    fn continues_after_random_access_until_decoder_emits() {
        let source = synthetic_source();
        let output = futures::executor::block_on(process_recording_with_decoder::<
            _,
            OneFrameDelayedDecoder,
        >(
            &source,
            synthetic_identity(),
            synthetic_options(),
        ))
        .unwrap();

        assert_eq!(output.metrics.thumbnail_samples_decoded, 2);
        assert_eq!(output.metrics.thumbnail_samples_examined, 2);
        assert_eq!(
            (output.metrics.source_width, output.metrics.source_height),
            (4, 4)
        );
    }

    #[test]
    fn thumbnail_limit_failure_does_not_block_metadata_artifacts() {
        let source = synthetic_source();
        let mut options = synthetic_options();
        options.limits.max_samples_decoded = 1;
        let output = futures::executor::block_on(process_recording_with_decoder::<
            _,
            OneFrameDelayedDecoder,
        >(&source, synthetic_identity(), options))
        .unwrap();

        assert!(output.thumbnail_jpeg.is_none());
        assert_eq!(output.metadata.thumbnail.frame_count, 0);
        assert!(
            output
                .metadata
                .thumbnail
                .error
                .as_deref()
                .is_some_and(|error| error.contains("too many access units decoded"))
        );
        assert!(!output.metadata.parts.is_empty());
        assert!(!output.init_mp4.is_empty());
        assert_eq!(output.metrics.thumbnail_failures, 1);
    }

    #[test]
    fn decoder_failure_does_not_block_metadata_artifacts() {
        let source = synthetic_source();
        let output =
            futures::executor::block_on(process_recording_with_decoder::<_, FailingDecoder>(
                &source,
                synthetic_identity(),
                synthetic_options(),
            ))
            .unwrap();

        assert!(output.thumbnail_jpeg.is_none());
        assert_eq!(output.metadata.schema_version, 4);
        assert_eq!(output.metadata.processor_version, "8");
        assert!(output.metadata.recording.video_duration_ns > 0);
        assert_eq!(output.metadata.thumbnail.frame_count, 0);
        assert_eq!(output.metadata.thumbnail.frames, Vec::new());
        assert_eq!(
            output.metadata.thumbnail.error.as_deref(),
            Some("decoder failed: fixture decoder failure")
        );
        assert!(!output.metadata.parts.is_empty());
        assert!(!output.init_mp4.is_empty());
        assert_eq!(output.metrics.thumbnail_failures, 1);
    }

    #[test]
    fn thumbnail_generation_can_be_disabled_for_metadata_only_indexing() {
        let source = synthetic_source();
        let output =
            futures::executor::block_on(process_recording_with_decoder::<_, FailingDecoder>(
                &source,
                synthetic_identity(),
                ProcessorOptions {
                    thumbnail: None,
                    ..ProcessorOptions::default()
                },
            ))
            .unwrap();

        assert!(output.thumbnail_jpeg.is_none());
        assert_eq!(output.metadata.thumbnail.error, None);
        assert_eq!(output.metrics.thumbnail_samples_decoded, 0);
        assert_eq!(output.metrics.thumbnail_failures, 0);
        assert!(!output.metadata.parts.is_empty());
    }

    #[test]
    fn processes_mediamtx_recording_into_three_artifacts() {
        #[cfg(feature = "codec-h264")]
        let relative = "../../fixtures/h264/mediamtx-v1.18.2-640x360.mp4";
        #[cfg(feature = "codec-h265")]
        let relative = "../../fixtures/h265/mediamtx-v1.18.2-640x360.mp4";
        let bytes = std::fs::read(format!("{}/{}", env!("CARGO_MANIFEST_DIR"), relative)).unwrap();
        let source = MemorySource::new(bytes);
        let output = futures::executor::block_on(process_recording(
            &source,
            SourceIdentity {
                object_key: "recordings/camera/test.mp4".into(),
                etag: "fixture-etag".into(),
            },
            ProcessorOptions::default(),
        ))
        .unwrap();

        assert!(!output.thumbnail_jpeg.as_ref().unwrap().is_empty());
        assert!(output.init_mp4.windows(4).any(|window| window == b"moov"));
        assert!(!output.init_mp4.windows(4).any(|window| window == b"mtxi"));
        assert_eq!(output.metadata.schema_version, 4);
        assert_eq!(output.metadata.source.etag, "fixture-etag");
        assert!(output.metadata.recording.start_utc_ns.is_some());
        assert!(output.metadata.recording.video_duration_ns > 0);
        assert!(!output.metadata.parts.is_empty());
        #[cfg(feature = "codec-h264")]
        let expected_offset = 744;
        #[cfg(feature = "codec-h265")]
        let expected_offset = 814;
        #[cfg(feature = "codec-h264")]
        let expected_duration = 200_000_000;
        #[cfg(feature = "codec-h265")]
        let expected_duration = 1_000_000;
        assert_eq!(output.metadata.parts[0].offset, expected_offset);
        assert!(output.metadata.parts[0].video_starts_with_sync);
        assert_eq!(output.metadata.parts[0].start_ns, 0);
        assert_eq!(output.metadata.parts[0].duration_ns, expected_duration);
        assert_eq!(output.metadata.init.sha256.len(), 64);
        assert_eq!(output.metadata.thumbnail.frame_count, 1);
        assert_eq!(output.metadata.thumbnail.error, None);
        assert_eq!(output.metadata.thumbnail.interval_ns, None);
        assert_eq!(output.metrics.thumbnail_frames, 1);
        assert_eq!(
            output.metrics.parts_indexed as usize,
            output.metadata.parts.len()
        );
        let decode_starts = output
            .metadata
            .parts
            .iter()
            .map(|part| part.start_ns)
            .collect::<Vec<_>>();
        assert!(decode_starts.windows(2).all(|pair| pair[0] < pair[1]));
        #[cfg(feature = "codec-h265")]
        {
            let presentation_starts = output
                .metadata
                .parts
                .iter()
                .map(|part| part.presentation_start_ns)
                .collect::<Vec<_>>();
            assert!(presentation_starts.windows(2).any(|pair| pair[0] > pair[1]));
        }
        let value = serde_json::to_value(&output.metadata).unwrap();
        assert!(value["recording"]["video_duration_ns"].is_string());
        assert!(value["thumbnail"].get("video_duration_ns").is_none());
        let json = serde_json::to_vec(&output.metadata).unwrap();
        let decoded: RecordingMetadata = serde_json::from_slice(&json).unwrap();
        assert_eq!(decoded, output.metadata);
    }

    #[test]
    fn creates_an_interval_sprite_with_floor_frame_count() {
        #[cfg(feature = "codec-h264")]
        let relative = "../../fixtures/h264/mediamtx-v1.18.2-640x360.mp4";
        #[cfg(feature = "codec-h265")]
        let relative = "../../fixtures/h265/mediamtx-v1.18.2-640x360.mp4";
        let bytes = std::fs::read(format!("{}/{}", env!("CARGO_MANIFEST_DIR"), relative)).unwrap();
        let source = MemorySource::new(bytes);
        let options = ProcessorOptions {
            thumbnail: Some(ThumbnailOptions {
                interval_ns: Some(parse_duration_ns("200ms").unwrap()),
                ..ThumbnailOptions::default()
            }),
            max_sprite_pixels: 400_000,
            ..ProcessorOptions::default()
        };
        let output = futures::executor::block_on(process_recording(
            &source,
            SourceIdentity {
                object_key: "recordings/camera/test.mp4".into(),
                etag: "fixture-etag".into(),
            },
            options,
        ))
        .unwrap();

        #[cfg(feature = "codec-h264")]
        let expected_frames = 5;
        #[cfg(feature = "codec-h265")]
        let expected_frames = 9;
        assert_eq!(output.metadata.thumbnail.frame_count, expected_frames);
        assert_eq!(
            output.metadata.thumbnail.frames.len(),
            expected_frames as usize
        );
        assert_eq!(output.metadata.thumbnail.interval_ns, Some(200_000_000));
        assert_eq!(output.metrics.thumbnail_frames, expected_frames);
        assert!(output.metadata.thumbnail.columns > 1);
        assert!(output.metadata.thumbnail.rows > 1);
        assert!(output.metadata.thumbnail.frame_width < 480);
        assert!(
            u64::from(output.metadata.thumbnail.sprite_width)
                * u64::from(output.metadata.thumbnail.sprite_height)
                <= 400_000
        );
        assert_eq!(&output.thumbnail_jpeg.as_ref().unwrap()[..2], &[0xff, 0xd8]);
    }

    #[test]
    fn indexing_and_thumbnail_rendering_are_composable() {
        #[cfg(feature = "codec-h264")]
        let relative = "../../fixtures/h264/mediamtx-v1.18.2-640x360.mp4";
        #[cfg(feature = "codec-h265")]
        let relative = "../../fixtures/h265/mediamtx-v1.18.2-640x360.mp4";
        let bytes = std::fs::read(format!("{}/{}", env!("CARGO_MANIFEST_DIR"), relative)).unwrap();
        let index = futures::executor::block_on(index_recording(
            bytes.as_slice(),
            synthetic_identity(),
            ProcessorOptions {
                thumbnail: None,
                ..ProcessorOptions::default()
            },
        ))
        .unwrap();
        assert_eq!(index.metadata.thumbnail.frame_count, 0);
        assert_eq!(index.metrics.thumbnail_samples_decoded, 0);

        let (jpeg, metadata) = futures::executor::block_on(render_thumbnail_sprite(
            bytes.as_slice(),
            &index,
            ThumbnailOptions::default(),
        ))
        .unwrap();
        assert_eq!(&jpeg[..2], &[0xff, 0xd8]);
        assert_eq!(metadata.frame_count, 1);
    }

    #[test]
    fn creates_one_frame_when_interval_exceeds_video_duration() {
        #[cfg(feature = "codec-h264")]
        let relative = "../../fixtures/h264/mediamtx-v1.18.2-640x360.mp4";
        #[cfg(feature = "codec-h265")]
        let relative = "../../fixtures/h265/mediamtx-v1.18.2-640x360.mp4";
        let bytes = std::fs::read(format!("{}/{}", env!("CARGO_MANIFEST_DIR"), relative)).unwrap();
        let source = MemorySource::new(bytes);
        let interval = parse_duration_ns("1h").unwrap();
        let output = futures::executor::block_on(process_recording(
            &source,
            synthetic_identity(),
            ProcessorOptions {
                thumbnail: Some(ThumbnailOptions {
                    interval_ns: Some(interval),
                    ..ThumbnailOptions::default()
                }),
                ..ProcessorOptions::default()
            },
        ))
        .unwrap();

        assert_eq!(output.metadata.thumbnail.frame_count, 1);
        assert_eq!(output.metadata.thumbnail.frames.len(), 1);
        assert_eq!(output.metadata.thumbnail.frames[0].target_ns, 0);
        assert_eq!(output.metadata.thumbnail.interval_ns, Some(interval));
        assert_eq!(output.metrics.thumbnail_frames, 1);
    }

    #[test]
    fn parses_human_thumbnail_durations() {
        assert_eq!(parse_duration_ns("10s").unwrap(), 10_000_000_000);
        assert_eq!(parse_duration_ns("1m").unwrap(), 60_000_000_000);
        assert_eq!(parse_duration_ns("250ms").unwrap(), 250_000_000);
        assert!(parse_duration_ns("10").is_err());
        assert!(parse_duration_ns("0s").is_err());
        assert!(parse_duration_ns("1d").is_err());
    }
}
