#![forbid(unsafe_code)]

use async_trait::async_trait;
use chrono::{DateTime, Utc};
use rolodex_core::{
    ByteSource, ProcessorOptions, RecordingIndex, RecordingMetadata, SourceIdentity,
    index_recording, mp4::rewrite_fragment_timing, processor::ThumbnailMetadata,
    render_thumbnail_sprite_with_presentation_origin,
};
use serde::{Deserialize, Serialize};

pub use rolodex_core::ThumbnailOptions;

pub const DESCRIPTOR_VERSION: u32 = 2;
pub const DEFAULT_MAX_MEDIA_BYTES: u64 = 512 * 1024 * 1024;
pub const DEFAULT_THUMBNAIL_INTERVAL_NS: u64 = 10_000_000_000;

pub fn default_thumbnail_options() -> ThumbnailOptions {
    ThumbnailOptions {
        interval_ns: Some(DEFAULT_THUMBNAIL_INTERVAL_NS),
        ..ThumbnailOptions::default()
    }
}

pub type Result<T> = std::result::Result<T, Error>;

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("invalid clip request: {0}")]
    InvalidRequest(&'static str),
    #[error("the requested time range is not covered by complete recording parts")]
    RangeNotCovered,
    #[error("source {id} changed after clip discovery")]
    SourceChanged { id: String },
    #[error("source {id} canonical init changed after clip discovery")]
    InitChanged { id: String },
    #[error("selected sources have different canonical initialization sections")]
    InitMismatch,
    #[error("selected clip media is {actual} bytes, exceeding the {limit} byte limit")]
    SizeLimit { actual: u64, limit: u64 },
    #[error("clip arithmetic overflow: {0}")]
    Overflow(&'static str),
    #[error("artifact serialization failed: {0}")]
    Serialization(#[from] serde_json::Error),
    #[error("artifact store failed: {0}")]
    Store(String),
    #[error("source {id}: {source}")]
    Source {
        id: String,
        #[source]
        source: rolodex_core::Error,
    },
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ClipRequest {
    pub stream: String,
    #[serde(with = "rolodex_core::decimal_string::scalar")]
    pub from_utc_ns: u64,
    #[serde(with = "rolodex_core::decimal_string::scalar")]
    pub to_utc_ns: u64,
    pub thumbnail: ThumbnailOptions,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct CandidateFile {
    pub id: String,
    #[serde(with = "rolodex_core::decimal_string::scalar")]
    pub start_utc_ns: u64,
    pub stamp: SourceStamp,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct SourceStamp {
    #[serde(with = "rolodex_core::decimal_string::scalar")]
    pub byte_length: u64,
    #[serde(with = "rolodex_core::decimal_string::scalar")]
    pub modified_utc_ns: u64,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
pub struct TimeRangeNs {
    #[serde(with = "rolodex_core::decimal_string::scalar")]
    pub from_utc_ns: u64,
    #[serde(with = "rolodex_core::decimal_string::scalar")]
    pub to_utc_ns: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ClipDescriptor {
    pub version: u32,
    pub stream: String,
    pub requested: TimeRangeNs,
    pub actual: TimeRangeNs,
    pub thumbnail: ThumbnailOptions,
    #[serde(with = "rolodex_core::decimal_string::scalar")]
    pub max_media_bytes: u64,
    pub sources: Vec<SourceSlice>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct SourceSlice {
    pub id: String,
    pub stamp: SourceStamp,
    pub init_sha256: String,
    pub init_len: u64,
    pub discontinuity_before: bool,
    pub parts: Vec<PartSlice>,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
pub struct PartSlice {
    pub offset: u64,
    pub length: u64,
    #[serde(with = "rolodex_core::decimal_string::scalar")]
    pub duration_ns: u64,
    #[serde(with = "rolodex_core::decimal_string::scalar")]
    pub start_utc_ns: u64,
    pub starts_with_sync: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Artifact {
    pub content_type: &'static str,
    pub bytes: Vec<u8>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ClipArtifacts {
    pub media: Artifact,
    pub manifest: Artifact,
    pub descriptor: Artifact,
    pub thumbnail: Option<Artifact>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum UploadOutcome {
    Created,
    AlreadyExists,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UploadedArtifact {
    pub key: String,
    pub outcome: UploadOutcome,
    pub byte_length: u64,
}

#[async_trait(?Send)]
pub trait ArtifactStore {
    async fn upload(&self, name: &str, artifact: &Artifact) -> Result<UploadOutcome>;
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ProcessedDescriptor {
    #[serde(flatten)]
    pub clip: ClipDescriptor,
    pub thumbnail_result: ThumbnailResult,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ThumbnailResult {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub file: Option<String>,
    #[serde(flatten)]
    pub metadata: ThumbnailMetadata,
}

#[async_trait(?Send)]
pub trait SourceOpener {
    async fn stamp(&self, id: &str) -> Result<SourceStamp>;
    async fn open(&self, id: &str) -> Result<Box<dyn ByteSource>>;
}

pub async fn upload(
    store: &dyn ArtifactStore,
    descriptor: &ClipDescriptor,
    artifacts: &ClipArtifacts,
) -> Result<Vec<UploadedArtifact>> {
    let prefix = artifact_prefix(descriptor)?;
    let mut uploaded = Vec::with_capacity(if artifacts.thumbnail.is_some() { 4 } else { 3 });
    upload_one(store, &prefix, "clip.mp4", &artifacts.media, &mut uploaded).await?;
    if let Some(thumbnail) = &artifacts.thumbnail {
        upload_one(store, &prefix, "thumbnail.jpg", thumbnail, &mut uploaded).await?;
    }
    upload_one(
        store,
        &prefix,
        "descriptor.json",
        &artifacts.descriptor,
        &mut uploaded,
    )
    .await?;
    upload_one(
        store,
        &prefix,
        "index.m3u8",
        &artifacts.manifest,
        &mut uploaded,
    )
    .await?;
    Ok(uploaded)
}

async fn upload_one(
    store: &dyn ArtifactStore,
    prefix: &str,
    name: &str,
    artifact: &Artifact,
    uploaded: &mut Vec<UploadedArtifact>,
) -> Result<()> {
    let key = format!("{prefix}/{name}");
    let outcome = store.upload(&key, artifact).await?;
    uploaded.push(UploadedArtifact {
        key,
        outcome,
        byte_length: artifact.bytes.len() as u64,
    });
    Ok(())
}

pub fn artifact_prefix(descriptor: &ClipDescriptor) -> Result<String> {
    validate_stream_path(&descriptor.stream)?;
    let from = format_key_timestamp(descriptor.actual.from_utc_ns)?;
    let duration_ns = descriptor
        .actual
        .to_utc_ns
        .checked_sub(descriptor.actual.from_utc_ns)
        .ok_or(Error::InvalidRequest("actual clip range is reversed"))?;
    Ok(format!(
        "clips/{}/{}_{}",
        descriptor.stream,
        from,
        format_duration(duration_ns)
    ))
}

fn format_key_timestamp(utc_ns: u64) -> Result<String> {
    Ok(utc_datetime(utc_ns)?
        .format("%Y%m%dT%H%M%S%.9fZ")
        .to_string())
}

struct IndexedSource {
    candidate: CandidateFile,
    init_sha256: String,
    init_len: u64,
    parts: Vec<IndexedPart>,
}

#[derive(Clone, Copy)]
struct IndexedPart {
    slice: PartSlice,
    first_sync_utc_ns: Option<u64>,
}

struct OpenedSource {
    source: Box<dyn ByteSource>,
    metadata: RecordingMetadata,
}

async fn open_verified(
    opener: &dyn SourceOpener,
    id: &str,
    stamp: &SourceStamp,
) -> Result<(Box<dyn ByteSource>, RecordingIndex)> {
    if opener.stamp(id).await? != *stamp {
        return Err(Error::SourceChanged { id: id.into() });
    }
    let source = opener.open(id).await?;
    if source.len() != stamp.byte_length {
        return Err(Error::SourceChanged { id: id.into() });
    }
    let index = index_recording(
        source.as_ref(),
        SourceIdentity {
            object_key: id.into(),
            etag: format!("{}:{}", stamp.byte_length, stamp.modified_utc_ns),
        },
        ProcessorOptions {
            thumbnail: None,
            ..ProcessorOptions::default()
        },
    )
    .await
    .map_err(|source| Error::Source {
        id: id.into(),
        source,
    })?;
    Ok((source, index))
}

pub async fn find(
    opener: &dyn SourceOpener,
    listing: &[CandidateFile],
    request: &ClipRequest,
) -> Result<ClipDescriptor> {
    find_with_limit(opener, listing, request, DEFAULT_MAX_MEDIA_BYTES).await
}

pub async fn find_with_limit(
    opener: &dyn SourceOpener,
    listing: &[CandidateFile],
    request: &ClipRequest,
    max_media_bytes: u64,
) -> Result<ClipDescriptor> {
    validate_request_with_limit(request, max_media_bytes)?;
    let candidates = select_candidates(listing, request.from_utc_ns, request.to_utc_ns)?;
    let mut indexed = Vec::with_capacity(candidates.len());
    for candidate in candidates {
        let (_, index) = open_verified(opener, &candidate.id, &candidate.stamp).await?;
        let mut parts = Vec::with_capacity(index.metadata.parts.len());
        for part in &index.metadata.parts {
            let Some(start) = part.start_utc_ns else {
                continue;
            };
            let Ok(start_utc_ns) = u64::try_from(start) else {
                continue;
            };
            let first_sync_utc_ns = part
                .first_video_sync_ns
                .and_then(|sync| index.metadata.recording.start_utc_ns?.checked_add(sync))
                .and_then(|sync| u64::try_from(sync).ok());
            parts.push(IndexedPart {
                slice: PartSlice {
                    offset: part.offset,
                    length: part.length,
                    duration_ns: part.duration_ns,
                    start_utc_ns,
                    starts_with_sync: part.video_starts_with_sync,
                },
                first_sync_utc_ns,
            });
        }
        if parts.iter().any(|part| part_end(part).is_err()) {
            return Err(Error::Source {
                id: candidate.id.clone(),
                source: rolodex_core::Error::Invalid("part wall-clock end overflow"),
            });
        }
        if parts.iter().any(|part| {
            part_end(part).is_ok_and(|end| {
                end > request.from_utc_ns && part.slice.start_utc_ns < request.to_utc_ns
            })
        }) {
            indexed.push(IndexedSource {
                candidate: candidate.clone(),
                init_sha256: index.metadata.init.sha256,
                init_len: index.metadata.init.byte_length,
                parts,
            });
        }
    }

    build_descriptor(indexed, request, max_media_bytes)
}

pub async fn process(
    opener: &dyn SourceOpener,
    descriptor: &ClipDescriptor,
) -> Result<ClipArtifacts> {
    if descriptor.version != DESCRIPTOR_VERSION {
        return Err(Error::InvalidRequest("unsupported descriptor version"));
    }
    validate_stream_path(&descriptor.stream)?;
    if descriptor.max_media_bytes == 0 {
        return Err(Error::InvalidRequest("media size limit is zero"));
    }
    if descriptor.thumbnail.interval_ns.is_none() {
        return Err(Error::InvalidRequest("thumbnail interval is absent"));
    }
    descriptor
        .thumbnail
        .validate()
        .map_err(|_| Error::InvalidRequest("thumbnail options are invalid"))?;
    let expected_init = descriptor
        .sources
        .first()
        .map(|source| source.init_sha256.as_str())
        .ok_or(Error::InvalidRequest("descriptor has no sources"))?;
    if descriptor
        .sources
        .iter()
        .any(|source| source.init_sha256 != expected_init)
    {
        return Err(Error::InitMismatch);
    }

    let mut opened = Vec::with_capacity(descriptor.sources.len());
    let mut init = None;
    for selected in &descriptor.sources {
        let (source, current) = open_verified(opener, &selected.id, &selected.stamp).await?;
        if current.metadata.init.sha256 != selected.init_sha256
            || current.metadata.init.byte_length != selected.init_len
        {
            return Err(Error::InitChanged {
                id: selected.id.clone(),
            });
        }
        if current.metadata.init.sha256 != expected_init {
            return Err(Error::InitMismatch);
        }
        if init.is_none() {
            init = Some(current.init_mp4);
        }
        opened.push(OpenedSource {
            source,
            metadata: current.metadata,
        });
    }

    let init = init.ok_or(Error::InvalidRequest("descriptor has no sources"))?;
    let media_len = descriptor
        .sources
        .iter()
        .try_fold(init.len() as u64, |total, source| {
            source.parts.iter().try_fold(total, |total, part| {
                total
                    .checked_add(part.length)
                    .ok_or(Error::Overflow("assembled media length"))
            })
        })?;
    enforce_media_limit(media_len, descriptor.max_media_bytes)?;
    let capacity =
        usize::try_from(media_len).map_err(|_| Error::Overflow("assembled media capacity"))?;
    let mut media = Vec::with_capacity(capacity);
    media.extend_from_slice(&init);
    let mut sequence_number = 0u32;
    let mut source_output_start_ns = 0u64;
    for (source_index, (opened_source, selected)) in
        opened.iter().zip(&descriptor.sources).enumerate()
    {
        if source_index != 0 {
            let slowest_timescale = opened_source
                .metadata
                .tracks
                .iter()
                .map(|track| track.timescale)
                .min()
                .ok_or_else(|| Error::SourceChanged {
                    id: selected.id.clone(),
                })?;
            let boundary_padding_ns = 1_000_000_000u64.div_ceil(u64::from(slowest_timescale));
            source_output_start_ns = source_output_start_ns
                .checked_add(boundary_padding_ns)
                .ok_or(Error::InvalidRequest("clip timeline duration overflow"))?;
        }
        let first_selected = selected
            .parts
            .first()
            .ok_or(Error::InvalidRequest("source slice has no parts"))?;
        let source_anchor_ns = opened_source
            .metadata
            .parts
            .iter()
            .find(|part| {
                part.offset == first_selected.offset && part.length == first_selected.length
            })
            .map(|part| part.start_ns)
            .ok_or_else(|| Error::SourceChanged {
                id: selected.id.clone(),
            })?;
        for part in &selected.parts {
            let current_part = opened_source
                .metadata
                .parts
                .iter()
                .find(|current| current.offset == part.offset && current.length == part.length)
                .ok_or_else(|| Error::SourceChanged {
                    id: selected.id.clone(),
                })?;
            let length =
                usize::try_from(part.length).map_err(|_| Error::Overflow("source part length"))?;
            let mut bytes = opened_source
                .source
                .range(part.offset, length)
                .await
                .map_err(|source| Error::Source {
                    id: selected.id.clone(),
                    source,
                })?;
            let track_base_decode_times = current_part
                .track_fragments
                .iter()
                .map(|track| {
                    let timescale = opened_source
                        .metadata
                        .tracks
                        .iter()
                        .find(|candidate| candidate.id == track.track_id)
                        .map(|track| track.timescale)
                        .ok_or_else(|| Error::SourceChanged {
                            id: selected.id.clone(),
                        })?;
                    Ok((
                        track.track_id,
                        normalize_decode_time(
                            track.base_decode_time,
                            source_anchor_ns,
                            source_output_start_ns,
                            timescale,
                        )?,
                    ))
                })
                .collect::<Result<Vec<_>>>()?;
            rewrite_fragment_timing(&mut bytes, sequence_number, &track_base_decode_times)
                .map_err(|source| Error::Source {
                    id: selected.id.clone(),
                    source,
                })?;
            media.extend_from_slice(&bytes);
            sequence_number = sequence_number
                .checked_add(1)
                .ok_or(Error::InvalidRequest("fragment sequence number overflow"))?;
        }
        source_output_start_ns =
            selected
                .parts
                .iter()
                .try_fold(source_output_start_ns, |total, part| {
                    total
                        .checked_add(part.duration_ns)
                        .ok_or(Error::InvalidRequest("clip timeline duration overflow"))
                })?;
    }

    let clip_index = index_recording(
        media.as_slice(),
        SourceIdentity {
            object_key: "clip.mp4".into(),
            etag: expected_init.into(),
        },
        ProcessorOptions {
            thumbnail: None,
            ..ProcessorOptions::default()
        },
    )
    .await
    .map_err(|source| Error::Source {
        id: "clip.mp4".into(),
        source,
    })?;
    let presentation_origin = clip_index
        .metadata
        .parts
        .iter()
        .map(|part| part.presentation_start_ns)
        .min()
        .ok_or(Error::InvalidRequest("assembled clip has no parts"))?;
    let (thumbnail, thumbnail_metadata) = match render_thumbnail_sprite_with_presentation_origin(
        media.as_slice(),
        &clip_index,
        descriptor.thumbnail,
        presentation_origin,
    )
    .await
    {
        Ok((bytes, metadata)) => (Some(bytes), metadata),
        Err(error) => (
            None,
            ThumbnailMetadata::unavailable(
                descriptor.thumbnail.interval_ns,
                Some(error.to_string()),
            ),
        ),
    };
    let thumbnail = thumbnail.map(|bytes| Artifact {
        content_type: "image/jpeg",
        bytes,
    });
    let sidecar = ProcessedDescriptor {
        clip: descriptor.clone(),
        thumbnail_result: ThumbnailResult {
            file: thumbnail.as_ref().map(|_| "thumbnail.jpg".into()),
            metadata: thumbnail_metadata,
        },
    };
    let descriptor_bytes = serde_json::to_vec_pretty(&sidecar)?;
    let manifest = build_manifest(descriptor, init.len() as u64)?;

    Ok(ClipArtifacts {
        media: Artifact {
            content_type: "video/mp4",
            bytes: media,
        },
        manifest: Artifact {
            content_type: "application/vnd.apple.mpegurl",
            bytes: manifest.into_bytes(),
        },
        descriptor: Artifact {
            content_type: "application/json",
            bytes: descriptor_bytes,
        },
        thumbnail,
    })
}

fn enforce_media_limit(actual: u64, limit: u64) -> Result<()> {
    if actual > limit {
        return Err(Error::SizeLimit { actual, limit });
    }
    Ok(())
}

fn normalize_decode_time(
    source_base_ticks: u64,
    source_anchor_ns: i64,
    output_start_ns: u64,
    timescale: u32,
) -> Result<u64> {
    let anchor_ticks = i128::from(source_anchor_ns)
        .checked_mul(i128::from(timescale))
        .ok_or(Error::InvalidRequest("source timeline conversion overflow"))?
        / 1_000_000_000i128;
    let output_ticks = i128::from(output_start_ns)
        .checked_mul(i128::from(timescale))
        .ok_or(Error::InvalidRequest("output timeline conversion overflow"))?
        / 1_000_000_000i128;
    let normalized = i128::from(source_base_ticks)
        .checked_sub(anchor_ticks)
        .and_then(|value| value.checked_add(output_ticks))
        .ok_or(Error::InvalidRequest("normalized decode time overflow"))?;
    u64::try_from(normalized)
        .map_err(|_| Error::InvalidRequest("normalized decode time is negative"))
}

fn build_manifest(descriptor: &ClipDescriptor, init_len: u64) -> Result<String> {
    let target_duration = descriptor
        .sources
        .iter()
        .flat_map(|source| &source.parts)
        .map(|part| part.duration_ns.div_ceil(1_000_000_000))
        .max()
        .ok_or(Error::InvalidRequest("descriptor has no parts"))?;
    let mut output = format!(
        "#EXTM3U\n#EXT-X-VERSION:7\n#EXT-X-TARGETDURATION:{target_duration}\n#EXT-X-MEDIA-SEQUENCE:0\n#EXT-X-PLAYLIST-TYPE:VOD\n#EXT-X-MAP:URI=\"clip.mp4\",BYTERANGE=\"{init_len}@0\"\n"
    );
    let mut offset = init_len;
    for source in &descriptor.sources {
        if source.discontinuity_before {
            output.push_str("#EXT-X-DISCONTINUITY\n");
        }
        for part in &source.parts {
            output.push_str("#EXT-X-PROGRAM-DATE-TIME:");
            output.push_str(&format_program_date_time(part.start_utc_ns)?);
            output.push('\n');
            output.push_str("#EXTINF:");
            output.push_str(&format_duration(part.duration_ns));
            output.push_str(",\n#EXT-X-BYTERANGE:");
            output.push_str(&format!("{}@{}\nclip.mp4\n", part.length, offset));
            offset = offset
                .checked_add(part.length)
                .ok_or(Error::InvalidRequest("manifest byte offset overflow"))?;
        }
    }
    output.push_str("#EXT-X-ENDLIST\n");
    Ok(output)
}

fn format_duration(duration_ns: u64) -> String {
    let seconds = duration_ns / 1_000_000_000;
    let nanos = duration_ns % 1_000_000_000;
    if nanos == 0 {
        return seconds.to_string();
    }
    format!("{seconds}.{nanos:09}").trim_end_matches('0').into()
}

fn format_program_date_time(utc_ns: u64) -> Result<String> {
    Ok(utc_datetime(utc_ns)?
        .format("%Y-%m-%dT%H:%M:%S%.9fZ")
        .to_string())
}

fn utc_datetime(utc_ns: u64) -> Result<DateTime<Utc>> {
    let seconds = i64::try_from(utc_ns / 1_000_000_000)
        .map_err(|_| Error::InvalidRequest("UTC timestamp exceeds i64 seconds"))?;
    let nanos = (utc_ns % 1_000_000_000) as u32;
    DateTime::<Utc>::from_timestamp(seconds, nanos)
        .ok_or(Error::InvalidRequest("UTC timestamp is out of range"))
}

pub fn validate_request(request: &ClipRequest) -> Result<()> {
    validate_request_with_limit(request, DEFAULT_MAX_MEDIA_BYTES)
}

fn validate_request_with_limit(request: &ClipRequest, max_media_bytes: u64) -> Result<()> {
    validate_stream_path(&request.stream)?;
    if request.stream.trim() != request.stream {
        return Err(Error::InvalidRequest("stream has surrounding whitespace"));
    }
    if request.from_utc_ns >= request.to_utc_ns {
        return Err(Error::InvalidRequest("from must be before to"));
    }
    if max_media_bytes == 0 {
        return Err(Error::InvalidRequest("media size limit is zero"));
    }
    if request.thumbnail.interval_ns.is_none() {
        return Err(Error::InvalidRequest("thumbnail interval is absent"));
    }
    request
        .thumbnail
        .validate()
        .map_err(|_| Error::InvalidRequest("thumbnail options are invalid"))?;
    Ok(())
}

fn validate_stream_path(stream: &str) -> Result<()> {
    if stream.is_empty()
        || stream.contains('\\')
        || stream
            .split('/')
            .any(|component| component.is_empty() || component == "." || component == "..")
    {
        return Err(Error::InvalidRequest(
            "stream is not a safe object-key path",
        ));
    }
    Ok(())
}

fn select_candidates(
    listing: &[CandidateFile],
    from_utc_ns: u64,
    to_utc_ns: u64,
) -> Result<Vec<CandidateFile>> {
    let mut sorted = listing.to_vec();
    sorted
        .sort_by(|left, right| (left.start_utc_ns, &left.id).cmp(&(right.start_utc_ns, &right.id)));
    let start = sorted
        .iter()
        .rposition(|candidate| candidate.start_utc_ns <= from_utc_ns)
        .unwrap_or(0);
    let Some(end) = sorted
        .iter()
        .rposition(|candidate| candidate.start_utc_ns < to_utc_ns)
    else {
        return Err(Error::RangeNotCovered);
    };
    if start > end {
        return Err(Error::RangeNotCovered);
    }
    Ok(sorted.drain(start..=end).collect())
}

fn build_descriptor(
    indexed: Vec<IndexedSource>,
    request: &ClipRequest,
    max_media_bytes: u64,
) -> Result<ClipDescriptor> {
    let flat = indexed
        .iter()
        .enumerate()
        .flat_map(|(source_index, source)| {
            source
                .parts
                .iter()
                .enumerate()
                .map(move |(part_index, part)| (source_index, part_index, part))
        })
        .collect::<Vec<_>>();
    let start = flat
        .iter()
        .filter(|(_, _, part)| {
            part.first_sync_utc_ns
                .is_some_and(|sync| sync <= request.from_utc_ns)
        })
        .max_by_key(|(_, _, part)| part.first_sync_utc_ns)
        .or_else(|| {
            flat.iter()
                .find(|(_, _, part)| part.first_sync_utc_ns.is_some())
        })
        .copied()
        .ok_or(Error::RangeNotCovered)?;
    let start_position = flat
        .iter()
        .position(|(source, part, _)| *source == start.0 && *part == start.1)
        .ok_or(Error::RangeNotCovered)?;
    let end_position = flat[start_position..]
        .iter()
        .position(|(_, _, part)| part_end(part).is_ok_and(|end| end >= request.to_utc_ns))
        .map(|relative| start_position + relative)
        .ok_or(Error::RangeNotCovered)?;
    let selected = &flat[start_position..=end_position];

    let mut media_bytes = 0u64;
    let mut sources = Vec::new();
    for (source_index, source) in indexed.iter().enumerate() {
        let source_parts = selected
            .iter()
            .filter(|(selected_source, _, _)| *selected_source == source_index)
            .map(|(_, _, part)| part.slice)
            .collect::<Vec<_>>();
        if source_parts.is_empty() {
            continue;
        }
        let source_media_bytes = source_parts.iter().try_fold(0u64, |total, part| {
            total
                .checked_add(part.length)
                .ok_or(Error::Overflow("selected source media length"))
        })?;
        media_bytes = media_bytes
            .checked_add(source_media_bytes)
            .and_then(|value| {
                value.checked_add(if sources.is_empty() {
                    source.init_len
                } else {
                    0
                })
            })
            .ok_or(Error::Overflow("selected clip media length"))?;
        if media_bytes > max_media_bytes {
            return Err(Error::SizeLimit {
                actual: media_bytes,
                limit: max_media_bytes,
            });
        }
        let discontinuity_before = !sources.is_empty();
        sources.push(SourceSlice {
            id: source.candidate.id.clone(),
            stamp: source.candidate.stamp.clone(),
            init_sha256: source.init_sha256.clone(),
            init_len: source.init_len,
            discontinuity_before,
            parts: source_parts,
        });
    }
    let first = selected.first().ok_or(Error::RangeNotCovered)?.2;
    let last = selected.last().ok_or(Error::RangeNotCovered)?.2;
    Ok(ClipDescriptor {
        version: DESCRIPTOR_VERSION,
        stream: request.stream.clone(),
        requested: TimeRangeNs {
            from_utc_ns: request.from_utc_ns,
            to_utc_ns: request.to_utc_ns,
        },
        actual: TimeRangeNs {
            from_utc_ns: first.slice.start_utc_ns,
            to_utc_ns: part_end(last)?,
        },
        thumbnail: request.thumbnail,
        max_media_bytes,
        sources,
    })
}

fn part_end(part: &IndexedPart) -> Result<u64> {
    part.slice
        .start_utc_ns
        .checked_add(part.slice.duration_ns)
        .ok_or(Error::InvalidRequest("part wall-clock end overflow"))
}

#[cfg(any(test, feature = "test-util"))]
#[doc(hidden)]
pub mod test_util {
    use super::{Error, Result};

    pub fn shift_mediamtx_start_ntp(bytes: &[u8], shift_ns: i64) -> Result<Vec<u8>> {
        let mut shifted = bytes.to_vec();
        let mtxi = shifted
            .windows(4)
            .position(|window| window == b"mtxi")
            .ok_or(Error::InvalidRequest(
                "fixture has no MediaMTX identity box",
            ))?;
        let start = mtxi
            .checked_add(40)
            .ok_or(Error::Overflow("MediaMTX fixture timestamp offset"))?;
        let end = start
            .checked_add(8)
            .ok_or(Error::Overflow("MediaMTX fixture timestamp end"))?;
        let field = shifted.get(start..end).ok_or(Error::InvalidRequest(
            "fixture MediaMTX identity box is truncated",
        ))?;
        let original = i64::from_be_bytes(
            field
                .try_into()
                .map_err(|_| Error::InvalidRequest("invalid MediaMTX timestamp field"))?,
        );
        let shifted_value = original
            .checked_add(shift_ns)
            .ok_or(Error::Overflow("MediaMTX fixture timestamp shift"))?;
        shifted[start..end].copy_from_slice(&shifted_value.to_be_bytes());
        Ok(shifted)
    }
}

#[cfg(test)]
mod tests {
    use std::{cell::RefCell, collections::HashMap};

    use rolodex_core::{process_recording, source::MemorySource};

    use super::*;

    struct MemoryOpener(HashMap<String, Vec<u8>>);

    #[derive(Default)]
    struct RecordingStore(RefCell<Vec<String>>);

    #[async_trait(?Send)]
    impl ArtifactStore for RecordingStore {
        async fn upload(&self, name: &str, _artifact: &Artifact) -> Result<UploadOutcome> {
            self.0.borrow_mut().push(name.into());
            Ok(UploadOutcome::Created)
        }
    }

    #[async_trait(?Send)]
    impl SourceOpener for MemoryOpener {
        async fn stamp(&self, id: &str) -> Result<SourceStamp> {
            let bytes = self.0.get(id).ok_or_else(|| Error::Source {
                id: id.into(),
                source: rolodex_core::Error::Source("fixture not found".into()),
            })?;
            let modified_utc_ns = match id {
                "recording.mp4" => 123,
                "first.mp4" => 1,
                "second.mp4" => 2,
                _ => 0,
            };
            Ok(SourceStamp {
                byte_length: bytes.len() as u64,
                modified_utc_ns,
            })
        }

        async fn open(&self, id: &str) -> Result<Box<dyn ByteSource>> {
            let bytes = self.0.get(id).cloned().ok_or_else(|| Error::Source {
                id: id.into(),
                source: rolodex_core::Error::Source("fixture not found".into()),
            })?;
            Ok(Box::new(MemorySource::new(bytes)))
        }
    }

    fn fixture() -> Vec<u8> {
        #[cfg(feature = "codec-h264")]
        let relative = "../../fixtures/h264/mediamtx-v1.18.2-640x360.mp4";
        #[cfg(feature = "codec-h265")]
        let relative = "../../fixtures/h265/mediamtx-v1.18.2-640x360.mp4";
        std::fs::read(format!("{}/{}", env!("CARGO_MANIFEST_DIR"), relative)).unwrap()
    }

    fn fixture_context() -> (MemoryOpener, CandidateFile, u64, u64) {
        let bytes = fixture();
        let source = MemorySource::new(bytes.clone());
        let output = futures::executor::block_on(process_recording(
            &source,
            SourceIdentity {
                object_key: "recording.mp4".into(),
                etag: "fixture".into(),
            },
            ProcessorOptions::default(),
        ))
        .unwrap();
        let start = u64::try_from(output.metadata.parts[0].start_utc_ns.unwrap()).unwrap();
        let end = output
            .metadata
            .parts
            .last()
            .and_then(|part| {
                u64::try_from(part.start_utc_ns.unwrap())
                    .unwrap()
                    .checked_add(part.duration_ns)
            })
            .unwrap();
        let candidate = CandidateFile {
            id: "recording.mp4".into(),
            start_utc_ns: start,
            stamp: SourceStamp {
                byte_length: bytes.len() as u64,
                modified_utc_ns: 123,
            },
        };
        (
            MemoryOpener(HashMap::from([("recording.mp4".into(), bytes)])),
            candidate,
            start,
            end,
        )
    }

    fn shifted_fixture(bytes: &[u8], shift_ns: i64) -> Vec<u8> {
        test_util::shift_mediamtx_start_ntp(bytes, shift_ns).unwrap()
    }

    fn describe_fixture(id: &str, bytes: &[u8], modified_utc_ns: u64) -> (CandidateFile, u64, u64) {
        let source = MemorySource::new(bytes.to_vec());
        let output = futures::executor::block_on(process_recording(
            &source,
            SourceIdentity {
                object_key: id.into(),
                etag: "fixture".into(),
            },
            ProcessorOptions {
                thumbnail: None,
                ..ProcessorOptions::default()
            },
        ))
        .unwrap();
        let start = u64::try_from(output.metadata.parts[0].start_utc_ns.unwrap()).unwrap();
        let last = output.metadata.parts.last().unwrap();
        let end = u64::try_from(last.start_utc_ns.unwrap())
            .unwrap()
            .checked_add(last.duration_ns)
            .unwrap();
        (
            CandidateFile {
                id: id.into(),
                start_utc_ns: start,
                stamp: SourceStamp {
                    byte_length: bytes.len() as u64,
                    modified_utc_ns,
                },
            },
            start,
            end,
        )
    }

    fn request(from_utc_ns: u64, to_utc_ns: u64) -> ClipRequest {
        ClipRequest {
            stream: "camera-12".into(),
            from_utc_ns,
            to_utc_ns,
            thumbnail: default_thumbnail_options(),
        }
    }

    #[test]
    fn finds_and_round_trips_a_range_inside_one_file() {
        let (opener, candidate, start, end) = fixture_context();
        let descriptor =
            futures::executor::block_on(find(&opener, &[candidate], &request(start, end - 1)))
                .unwrap();

        assert_eq!(descriptor.version, DESCRIPTOR_VERSION);
        assert_eq!(descriptor.max_media_bytes, DEFAULT_MAX_MEDIA_BYTES);
        assert_eq!(descriptor.actual.from_utc_ns, start);
        assert!(descriptor.actual.to_utc_ns >= end - 1);
        assert_eq!(descriptor.sources.len(), 1);
        assert!(!descriptor.sources[0].parts.is_empty());
        let json = serde_json::to_vec(&descriptor).unwrap();
        let decoded: ClipDescriptor = serde_json::from_slice(&json).unwrap();
        assert_eq!(decoded, descriptor);
        let value = serde_json::to_value(&descriptor).unwrap();
        assert!(value["actual"]["from_utc_ns"].is_string());
        assert!(value["max_media_bytes"].is_string());
    }

    #[test]
    fn rejects_a_range_past_complete_media() {
        let (opener, candidate, start, end) = fixture_context();
        let error =
            futures::executor::block_on(find(&opener, &[candidate], &request(start, end + 1)))
                .unwrap_err();
        assert!(matches!(error, Error::RangeNotCovered));
    }

    #[test]
    fn snaps_a_request_before_the_first_sync_to_the_first_part() {
        let (opener, candidate, start, end) = fixture_context();
        let descriptor =
            futures::executor::block_on(find(&opener, &[candidate], &request(start - 1, end - 1)))
                .unwrap();
        assert_eq!(descriptor.actual.from_utc_ns, start);
    }

    #[test]
    fn finds_a_range_spanning_two_recordings() {
        let first = fixture();
        let (first_candidate, first_start, first_end) = describe_fixture("first.mp4", &first, 1);
        let second = shifted_fixture(&first, i64::try_from(first_end - first_start).unwrap());
        let (second_candidate, second_start, second_end) =
            describe_fixture("second.mp4", &second, 2);
        let opener = MemoryOpener(HashMap::from([
            ("first.mp4".into(), first),
            ("second.mp4".into(), second),
        ]));

        let mut spanning_request = request(first_start, second_end - 1);
        spanning_request.thumbnail.interval_ns = Some(200_000_000);
        let descriptor = futures::executor::block_on(find(
            &opener,
            &[second_candidate, first_candidate],
            &spanning_request,
        ))
        .unwrap();

        assert_eq!(second_start, first_end);
        assert_eq!(descriptor.sources.len(), 2);
        assert!(!descriptor.sources[0].discontinuity_before);
        assert!(descriptor.sources[1].discontinuity_before);

        let expected_duration = descriptor
            .sources
            .iter()
            .flat_map(|source| &source.parts)
            .map(|part| part.duration_ns)
            .sum::<u64>();
        let expected_frames = expected_duration / spanning_request.thumbnail.interval_ns.unwrap();
        let artifacts = futures::executor::block_on(process(&opener, &descriptor)).unwrap();
        let standalone = futures::executor::block_on(process_recording(
            &MemorySource::new(artifacts.media.bytes.clone()),
            SourceIdentity {
                object_key: "standalone-clip.mp4".into(),
                etag: "test".into(),
            },
            ProcessorOptions {
                thumbnail: None,
                ..ProcessorOptions::default()
            },
        ))
        .unwrap();
        assert!(
            standalone
                .metadata
                .recording
                .video_duration_ns
                .abs_diff(expected_duration)
                <= 1_000_000
        );
        assert!(
            standalone
                .metadata
                .parts
                .windows(2)
                .all(|parts| parts[0].start_ns < parts[1].start_ns)
        );
        assert!(
            standalone
                .metadata
                .parts
                .iter()
                .enumerate()
                .all(|(index, part)| part.sequence_number == index as u32)
        );
        let sidecar: ProcessedDescriptor =
            serde_json::from_slice(&artifacts.descriptor.bytes).unwrap();
        assert_eq!(
            u64::from(sidecar.thumbnail_result.metadata.frame_count),
            expected_frames
        );
        assert_eq!(
            std::str::from_utf8(&artifacts.manifest.bytes)
                .unwrap()
                .matches("#EXT-X-DISCONTINUITY")
                .count(),
            1
        );
    }

    #[test]
    fn rejects_a_range_wholly_inside_a_recording_gap() {
        let first = fixture();
        let (first_candidate, first_start, first_end) = describe_fixture("first.mp4", &first, 1);
        let gap_ns = 5_000_000_000i64;
        let second = shifted_fixture(
            &first,
            i64::try_from(first_end - first_start).unwrap() + gap_ns,
        );
        let (second_candidate, second_start, _) = describe_fixture("second.mp4", &second, 2);
        let opener = MemoryOpener(HashMap::from([
            ("first.mp4".into(), first),
            ("second.mp4".into(), second),
        ]));

        let error = futures::executor::block_on(find(
            &opener,
            &[first_candidate, second_candidate],
            &request(first_end + 1, second_start - 1),
        ))
        .unwrap_err();
        assert!(matches!(error, Error::RangeNotCovered));
    }

    #[test]
    fn enforces_selected_media_size_limit() {
        let (opener, candidate, start, end) = fixture_context();
        let error = futures::executor::block_on(find_with_limit(
            &opener,
            &[candidate],
            &request(start, end - 1),
            1,
        ))
        .unwrap_err();
        assert!(matches!(error, Error::SizeLimit { .. }));
    }

    #[test]
    fn process_uses_the_limit_carried_by_the_descriptor() {
        let (opener, candidate, start, end) = fixture_context();
        let high_limit = DEFAULT_MAX_MEDIA_BYTES * 2;
        let mut descriptor = futures::executor::block_on(find_with_limit(
            &opener,
            &[candidate],
            &request(start, end - 1),
            high_limit,
        ))
        .unwrap();
        assert_eq!(descriptor.max_media_bytes, high_limit);
        futures::executor::block_on(process(&opener, &descriptor)).unwrap();

        let media_len = descriptor.sources[0].init_len
            + descriptor
                .sources
                .iter()
                .flat_map(|source| &source.parts)
                .map(|part| part.length)
                .sum::<u64>();
        descriptor.max_media_bytes = media_len - 1;
        let error = futures::executor::block_on(process(&opener, &descriptor)).unwrap_err();
        assert!(
            matches!(error, Error::SizeLimit { actual, limit } if actual == media_len && limit == media_len - 1)
        );

        assert!(enforce_media_limit(DEFAULT_MAX_MEDIA_BYTES + 1, high_limit).is_ok());
    }

    #[test]
    fn rejects_unsafe_stream_paths_before_discovery() {
        for stream in [
            "",
            "/camera",
            "camera/",
            "camera//12",
            ".",
            "..",
            "../camera",
            "..\\camera",
        ] {
            let mut request = request(1, 2);
            request.stream = stream.into();
            assert!(matches!(
                validate_request(&request),
                Err(Error::InvalidRequest(
                    "stream is not a safe object-key path"
                ))
            ));
        }
    }

    #[test]
    fn selected_media_overflow_has_a_real_overflow_error() {
        let request = request(1, 2);
        let indexed = vec![IndexedSource {
            candidate: CandidateFile {
                id: "source.mp4".into(),
                start_utc_ns: 1,
                stamp: SourceStamp {
                    byte_length: u64::MAX,
                    modified_utc_ns: 1,
                },
            },
            init_sha256: "hash".into(),
            init_len: 1,
            parts: vec![IndexedPart {
                slice: PartSlice {
                    offset: 0,
                    length: u64::MAX,
                    duration_ns: 1,
                    start_utc_ns: 1,
                    starts_with_sync: true,
                },
                first_sync_utc_ns: Some(1),
            }],
        }];
        assert!(matches!(
            build_descriptor(indexed, &request, u64::MAX),
            Err(Error::Overflow("selected clip media length"))
        ));
    }

    #[test]
    fn rejects_source_size_changes() {
        let (opener, mut candidate, start, end) = fixture_context();
        candidate.stamp.byte_length += 1;
        let error =
            futures::executor::block_on(find(&opener, &[candidate], &request(start, end - 1)))
                .unwrap_err();
        assert!(matches!(error, Error::SourceChanged { .. }));
    }

    #[test]
    fn candidate_selection_is_chronological_and_bounded() {
        let candidate = |id: &str, start_utc_ns| CandidateFile {
            id: id.into(),
            start_utc_ns,
            stamp: SourceStamp {
                byte_length: 1,
                modified_utc_ns: 1,
            },
        };
        let selected = select_candidates(
            &[
                candidate("later", 30),
                candidate("first", 10),
                candidate("middle", 20),
            ],
            21,
            31,
        )
        .unwrap();
        assert_eq!(
            selected
                .iter()
                .map(|item| item.id.as_str())
                .collect::<Vec<_>>(),
            vec!["middle", "later"]
        );
    }

    #[test]
    fn processes_selected_parts_into_a_playable_artifact_set() {
        let (opener, candidate, start, end) = fixture_context();
        let descriptor =
            futures::executor::block_on(find(&opener, &[candidate], &request(start, end - 1)))
                .unwrap();
        let artifacts = futures::executor::block_on(process(&opener, &descriptor)).unwrap();

        assert_eq!(artifacts.media.content_type, "video/mp4");
        assert_eq!(
            artifacts.manifest.content_type,
            "application/vnd.apple.mpegurl"
        );
        assert_eq!(artifacts.descriptor.content_type, "application/json");
        assert_eq!(
            &artifacts.thumbnail.as_ref().unwrap().bytes[..2],
            &[0xff, 0xd8]
        );

        let source = opener.0.get("recording.mp4").unwrap();
        let mut artifact_offset = descriptor.sources[0].init_len as usize;
        for part in &descriptor.sources[0].parts {
            let source_start = part.offset as usize;
            let source_end = source_start + part.length as usize;
            let artifact_end = artifact_offset + part.length as usize;
            assert_eq!(
                &artifacts.media.bytes[artifact_offset..artifact_end],
                &source[source_start..source_end]
            );
            artifact_offset = artifact_end;
        }
        assert_eq!(artifact_offset, artifacts.media.bytes.len());

        let manifest = std::str::from_utf8(&artifacts.manifest.bytes).unwrap();
        assert!(manifest.contains("#EXT-X-MAP:URI=\"clip.mp4\""));
        assert!(manifest.ends_with("#EXT-X-ENDLIST\n"));
        assert_eq!(
            manifest.matches("#EXT-X-BYTERANGE:").count(),
            descriptor.sources[0].parts.len()
        );
        let sidecar: ProcessedDescriptor =
            serde_json::from_slice(&artifacts.descriptor.bytes).unwrap();
        assert_eq!(sidecar.clip, descriptor);
        assert_eq!(
            sidecar.thumbnail_result.file.as_deref(),
            Some("thumbnail.jpg")
        );
        assert!(
            sidecar
                .thumbnail_result
                .metadata
                .frames
                .iter()
                .all(|frame| frame.presentation_time_ns >= 0)
        );
    }

    #[test]
    fn thumbnail_failure_does_not_block_clip_artifacts() {
        let (opener, candidate, start, end) = fixture_context();
        let mut clip_request = request(start, end - 1);
        clip_request.thumbnail.interval_ns = Some(1);
        let descriptor =
            futures::executor::block_on(find(&opener, &[candidate], &clip_request)).unwrap();
        let artifacts = futures::executor::block_on(process(&opener, &descriptor)).unwrap();

        assert!(artifacts.thumbnail.is_none());
        assert!(!artifacts.media.bytes.is_empty());
        let sidecar: ProcessedDescriptor =
            serde_json::from_slice(&artifacts.descriptor.bytes).unwrap();
        assert!(sidecar.thumbnail_result.file.is_none());
        assert!(
            sidecar
                .thumbnail_result
                .metadata
                .error
                .as_deref()
                .is_some_and(|error| error.contains("frame count exceeds"))
        );
    }

    #[test]
    fn process_rejects_stamp_and_init_changes() {
        let (opener, candidate, start, end) = fixture_context();
        let descriptor =
            futures::executor::block_on(find(&opener, &[candidate], &request(start, end - 1)))
                .unwrap();

        let mut changed_stamp = descriptor.clone();
        changed_stamp.sources[0].stamp.modified_utc_ns += 1;
        let error = futures::executor::block_on(process(&opener, &changed_stamp)).unwrap_err();
        assert!(matches!(error, Error::SourceChanged { .. }));

        let mut changed_init = descriptor;
        changed_init.sources[0].init_sha256 = "0".repeat(64);
        let error = futures::executor::block_on(process(&opener, &changed_init)).unwrap_err();
        assert!(matches!(error, Error::InitChanged { .. }));
    }

    #[test]
    fn process_rejects_mixed_descriptor_inits_before_reading() {
        let (opener, candidate, start, end) = fixture_context();
        let mut descriptor =
            futures::executor::block_on(find(&opener, &[candidate], &request(start, end - 1)))
                .unwrap();
        let mut second = descriptor.sources[0].clone();
        second.init_sha256 = "0".repeat(64);
        descriptor.sources.push(second);

        let error = futures::executor::block_on(process(&opener, &descriptor)).unwrap_err();
        assert!(matches!(error, Error::InitMismatch));
    }

    #[test]
    fn uploads_manifest_last_under_a_deterministic_prefix() {
        let (opener, candidate, start, end) = fixture_context();
        let descriptor =
            futures::executor::block_on(find(&opener, &[candidate], &request(start, end - 1)))
                .unwrap();
        let artifacts = futures::executor::block_on(process(&opener, &descriptor)).unwrap();
        let store = RecordingStore::default();
        let uploaded =
            futures::executor::block_on(upload(&store, &descriptor, &artifacts)).unwrap();
        let names = store.0.into_inner();

        assert_eq!(names.len(), 4);
        assert!(names[0].ends_with("/clip.mp4"));
        assert!(names[1].ends_with("/thumbnail.jpg"));
        assert!(names[2].ends_with("/descriptor.json"));
        assert!(names[3].ends_with("/index.m3u8"));
        assert!(names[0].starts_with("clips/camera-12/"));
        assert_eq!(
            uploaded
                .iter()
                .map(|item| item.key.as_str())
                .collect::<Vec<_>>(),
            names.iter().map(String::as_str).collect::<Vec<_>>()
        );
    }

    #[test]
    fn manifest_has_stable_golden_format() {
        let descriptor = ClipDescriptor {
            version: DESCRIPTOR_VERSION,
            stream: "camera".into(),
            requested: TimeRangeNs {
                from_utc_ns: 0,
                to_utc_ns: 3_500_000_000,
            },
            actual: TimeRangeNs {
                from_utc_ns: 0,
                to_utc_ns: 3_500_000_000,
            },
            thumbnail: default_thumbnail_options(),
            max_media_bytes: DEFAULT_MAX_MEDIA_BYTES,
            sources: vec![SourceSlice {
                id: "source.mp4".into(),
                stamp: SourceStamp {
                    byte_length: 300,
                    modified_utc_ns: 0,
                },
                init_sha256: "hash".into(),
                init_len: 100,
                discontinuity_before: false,
                parts: vec![
                    PartSlice {
                        offset: 100,
                        length: 80,
                        duration_ns: 1_500_000_000,
                        start_utc_ns: 0,
                        starts_with_sync: true,
                    },
                    PartSlice {
                        offset: 180,
                        length: 120,
                        duration_ns: 2_000_000_000,
                        start_utc_ns: 1_500_000_000,
                        starts_with_sync: false,
                    },
                ],
            }],
        };

        assert_eq!(
            build_manifest(&descriptor, 100).unwrap(),
            "#EXTM3U\n\
#EXT-X-VERSION:7\n\
#EXT-X-TARGETDURATION:2\n\
#EXT-X-MEDIA-SEQUENCE:0\n\
#EXT-X-PLAYLIST-TYPE:VOD\n\
#EXT-X-MAP:URI=\"clip.mp4\",BYTERANGE=\"100@0\"\n\
#EXT-X-PROGRAM-DATE-TIME:1970-01-01T00:00:00.000000000Z\n\
#EXTINF:1.5,\n\
#EXT-X-BYTERANGE:80@100\n\
clip.mp4\n\
#EXT-X-PROGRAM-DATE-TIME:1970-01-01T00:00:01.500000000Z\n\
#EXTINF:2,\n\
#EXT-X-BYTERANGE:120@180\n\
clip.mp4\n\
#EXT-X-ENDLIST\n"
        );
    }
}
