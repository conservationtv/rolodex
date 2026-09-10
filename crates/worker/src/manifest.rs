#![cfg_attr(not(target_arch = "wasm32"), allow(dead_code))]

use std::collections::BTreeMap;

use rolodex_core::RecordingMetadata;
use serde::{Deserialize, Serialize};

pub(crate) const ARCHIVE_SCHEMA_VERSION: u32 = 3;

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub(crate) struct StreamArchive {
    pub(crate) version: u32,
    pub(crate) stream: String,
    pub(crate) days: BTreeMap<String, ArchiveDay>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) head_prefix: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) retention_seconds: Option<u64>,
}

impl StreamArchive {
    pub(crate) fn new(stream: String) -> Self {
        Self {
            version: 2,
            stream,
            days: BTreeMap::new(),
            head_prefix: None,
            retention_seconds: None,
        }
    }

    pub(crate) fn validate(&self, stream: &str) -> Result<(), String> {
        if self.version != 2 {
            return Err(format!(
                "archive manifest schema {} is unsupported",
                self.version
            ));
        }
        if self.stream != stream {
            return Err(format!(
                "archive manifest belongs to stream {}, not {stream}",
                self.stream
            ));
        }
        Ok(())
    }

    pub(crate) fn merge_day(&mut self, date: String, day: ArchiveDay) {
        self.days.insert(date, day);
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub(crate) struct ArchiveDay {
    pub(crate) key: String,
    pub(crate) started_at: String,
    pub(crate) ended_at: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub(crate) struct DailyArchive {
    pub(crate) version: u32,
    pub(crate) stream: String,
    pub(crate) date: String,
    pub(crate) recordings: Vec<ArchiveRecording>,
}

impl DailyArchive {
    pub(crate) fn new(stream: String, date: String) -> Self {
        Self {
            version: ARCHIVE_SCHEMA_VERSION,
            stream,
            date,
            recordings: Vec::new(),
        }
    }

    pub(crate) fn validate(&self, stream: &str, date: &str) -> Result<(), String> {
        if self.version != 1 && self.version != ARCHIVE_SCHEMA_VERSION {
            return Err(format!(
                "daily archive schema {} is unsupported",
                self.version
            ));
        }
        if self.stream != stream || self.date != date {
            return Err(format!(
                "daily archive belongs to {}/{}, not {stream}/{date}",
                self.stream, self.date
            ));
        }
        Ok(())
    }

    pub(crate) fn merge(&mut self, recording: ArchiveRecording) -> Result<(), String> {
        let recording_date = recording
            .started_at
            .get(..10)
            .ok_or_else(|| "recording UTC start is not an ISO date".to_string())?;
        if recording_date != self.date {
            return Err(format!(
                "recording date {recording_date} does not match daily archive {}",
                self.date
            ));
        }
        self.version = ARCHIVE_SCHEMA_VERSION;
        self.recordings
            .retain(|existing| existing.id != recording.id);
        self.recordings.push(recording);
        self.recordings.sort_by(|left, right| {
            left.started_at
                .cmp(&right.started_at)
                .then_with(|| left.id.cmp(&right.id))
        });
        Ok(())
    }

    pub(crate) fn coverage(&self, key: String) -> Result<ArchiveDay, String> {
        let start_utc = self
            .recordings
            .iter()
            .map(|recording| &recording.started_at)
            .min()
            .cloned()
            .ok_or_else(|| "daily archive has no recordings".to_string())?;
        let end_utc = self
            .recordings
            .iter()
            .map(|recording| &recording.ended_at)
            .max()
            .cloned()
            .ok_or_else(|| "daily archive has no recordings".to_string())?;
        Ok(ArchiveDay {
            key,
            started_at: start_utc,
            ended_at: end_utc,
        })
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub(crate) struct ArchiveRecording {
    pub(crate) id: String,
    pub(crate) started_at: String,
    pub(crate) ended_at: String,
    #[serde(with = "rolodex_core::decimal_string::scalar")]
    pub(crate) duration_ns: u64,
    pub(crate) source: String,
    pub(crate) init: String,
    pub(crate) segments: Vec<ArchiveSegment>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) thumbnail: Option<ArchiveThumbnail>,
}

impl ArchiveRecording {
    pub(crate) fn from_metadata(
        metadata: &RecordingMetadata,
        init_key: String,
        thumbnail_key: Option<String>,
    ) -> Result<Self, String> {
        let recording = &metadata.recording;
        let start_utc_ns = recording
            .start_utc_ns
            .as_ref()
            .ok_or_else(|| "recording metadata has no MediaMTX UTC start".to_string())?;
        let start_utc_ns = i128::from(*start_utc_ns);
        let duration_ns = i128::from(recording.video_duration_ns);
        let end_utc_ns = start_utc_ns
            .checked_add(duration_ns)
            .ok_or_else(|| "recording UTC end overflows i128 nanoseconds".to_string())?;

        let mut segments: Vec<ArchiveSegment> = Vec::new();
        for part in &metadata.parts {
            if part.video_starts_with_sync {
                segments.push(ArchiveSegment {
                    pieces: Vec::new(),
                    presentation_start_ns: part.presentation_start_ns,
                    byte_offset: part.offset,
                    byte_length: part.length,
                });
                continue;
            }

            let current = segments
                .last_mut()
                .ok_or_else(|| "recording does not start with a random-access part".to_string())?;
            let expected_offset = current
                .byte_offset
                .checked_add(current.byte_length)
                .ok_or_else(|| "archive segment byte range overflows u64".to_string())?;
            if part.offset != expected_offset {
                return Err(format!(
                    "non-contiguous part {} while building archive segment",
                    part.sequence_number
                ));
            }
            current.byte_length = current
                .byte_length
                .checked_add(part.length)
                .ok_or_else(|| "archive segment length overflows u64".to_string())?;
        }
        if !metadata.playback_parts.is_empty() {
            segments = playback_segments(metadata)?;
        }
        if segments.is_empty() {
            return Err("recording has no random-access segments".into());
        }

        let thumbnail = thumbnail_key.and_then(|image_key| {
            let value = &metadata.thumbnail;
            (!value.frames.is_empty()
                && value.frame_width > 0
                && value.frame_height > 0
                && value.columns > 0)
                .then(|| ArchiveThumbnail {
                    key: image_key,
                    interval_ns: value.interval_ns,
                    cell: ThumbnailCell {
                        width: value.frame_width,
                        height: value.frame_height,
                    },
                    columns: value.columns,
                    presentation_ns: value
                        .frames
                        .iter()
                        .map(|frame| frame.presentation_time_ns)
                        .collect(),
                })
        });

        Ok(Self {
            id: metadata.source.object_key.clone(),
            started_at: utc_timestamp(start_utc_ns)?,
            ended_at: utc_timestamp_ceil(end_utc_ns)?,
            duration_ns: recording.video_duration_ns,
            source: metadata.source.object_key.clone(),
            init: init_key,
            segments,
            thumbnail,
        })
    }
}

fn playback_segments(metadata: &RecordingMetadata) -> Result<Vec<ArchiveSegment>, String> {
    let mut segments: Vec<ArchiveSegment> = Vec::new();
    for part in &metadata.playback_parts {
        if part.starts_with_sync {
            segments.push(ArchiveSegment {
                presentation_start_ns: part.presentation_start_ns,
                byte_offset: 0,
                byte_length: 0,
                pieces: Vec::new(),
            });
        }
        let segment = segments
            .last_mut()
            .ok_or("playback does not begin with a keyframe")?;
        segment.byte_length = segment
            .byte_length
            .checked_add(part.length)
            .ok_or("playback size overflow")?;
        if let Some(previous) = segment.pieces.last_mut() {
            if previous.source == part.source
                && previous.byte_offset + previous.byte_length == part.offset
            {
                previous.byte_length += part.length;
                continue;
            }
        }
        segment.pieces.push(ArchivePiece {
            source: part.source.clone(),
            byte_offset: part.offset,
            byte_length: part.length,
        });
    }
    Ok(segments)
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
/// Location discriminant: nonempty `pieces` is authoritative and must be read in
/// order; `byte_offset` is then zero and `byte_length` is the sum of piece lengths.
/// Empty/omitted `pieces` means the segment is one range in ArchiveRecording.source,
/// addressed by `byte_offset` and `byte_length`. Never read that range when pieces exist.
pub(crate) struct ArchiveSegment {
    #[serde(with = "rolodex_core::decimal_string::scalar")]
    pub(crate) presentation_start_ns: i64,
    pub(crate) byte_offset: u64,
    pub(crate) byte_length: u64,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub(crate) pieces: Vec<ArchivePiece>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub(crate) struct ArchivePiece {
    pub(crate) source: String,
    pub(crate) byte_offset: u64,
    pub(crate) byte_length: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub(crate) struct ArchiveThumbnail {
    pub(crate) key: String,
    #[serde(
        default,
        with = "rolodex_core::decimal_string::optional",
        skip_serializing_if = "Option::is_none"
    )]
    pub(crate) interval_ns: Option<u64>,
    pub(crate) cell: ThumbnailCell,
    pub(crate) columns: u32,
    #[serde(with = "rolodex_core::decimal_string::sequence")]
    pub(crate) presentation_ns: Vec<i64>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub(crate) struct ThumbnailCell {
    pub(crate) width: u32,
    pub(crate) height: u32,
}

/// Stable five-minute UTC partitions bound the tail independently of recording duration.
/// Late arrivals replace one addressed page, never the whole day's recording index.
pub(crate) fn recording_bucket(recording: &ArchiveRecording) -> Result<String, String> {
    let start = &recording.started_at;
    let minute: u32 = start
        .get(14..16)
        .ok_or("invalid recording minute")?
        .parse()
        .map_err(|_| "invalid recording minute")?;
    if minute > 59 {
        return Err("invalid recording minute".into());
    }
    Ok(format!(
        "{}{:02}",
        start.get(..14).ok_or("invalid recording timestamp")?,
        minute / 5 * 5
    ))
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub(crate) struct ArchivePageEntry {
    pub(crate) bucket: String,
    #[serde(flatten)]
    pub(crate) coverage: ArchiveDay,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub(crate) struct ArchivePageDirectory {
    pub(crate) version: u32,
    pub(crate) stream: String,
    pub(crate) date: String,
    pub(crate) pages: Vec<ArchivePageEntry>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub(crate) struct ArchiveHead {
    pub(crate) version: u32,
    pub(crate) stream: String,
    pub(crate) date: String,
    pub(crate) started_at: String,
    pub(crate) ended_at: String,
    pub(crate) pages: String,
    pub(crate) tail: ArchivePageEntry,
}

impl ArchiveHead {
    pub(crate) fn validate(&self, stream: &str, date: &str) -> Result<(), String> {
        if self.version != 2 || self.stream != stream || self.date != date {
            return Err("archive head identity or version mismatch".into());
        }
        Ok(())
    }
    pub(crate) fn from_pages(
        directory: &ArchivePageDirectory,
        pages: String,
        tail: ArchivePageEntry,
    ) -> Self {
        let mut started_at = tail.coverage.started_at.clone();
        let mut ended_at = tail.coverage.ended_at.clone();
        for page in &directory.pages {
            started_at = started_at.min(page.coverage.started_at.clone());
            ended_at = ended_at.max(page.coverage.ended_at.clone());
        }
        Self {
            version: 2,
            stream: directory.stream.clone(),
            date: directory.date.clone(),
            started_at,
            ended_at,
            pages,
            tail,
        }
    }
    pub(crate) fn coverage(&self, key: String) -> ArchiveDay {
        ArchiveDay {
            key,
            started_at: self.started_at.clone(),
            ended_at: self.ended_at.clone(),
        }
    }
}

#[cfg(test)]
fn utc_date(unix_ns: &str) -> Result<String, String> {
    const NS_PER_DAY: i128 = 86_400_000_000_000;
    let days = decimal_i128(unix_ns)?.div_euclid(NS_PER_DAY);
    let days = i64::try_from(days).map_err(|_| "UTC date exceeds i64 days".to_string())?;
    let (year, month, day) = civil_from_days(days);
    Ok(format!("{year:04}-{month:02}-{day:02}"))
}

pub(crate) fn utc_timestamp(unix_ns: i128) -> Result<String, String> {
    const MS_PER_DAY: i128 = 86_400_000;
    let unix_ms = unix_ns.div_euclid(1_000_000);
    let days = unix_ms.div_euclid(MS_PER_DAY);
    let day_ms = unix_ms.rem_euclid(MS_PER_DAY);
    let days = i64::try_from(days).map_err(|_| "UTC timestamp exceeds i64 days".to_string())?;
    let (year, month, day) = civil_from_days(days);
    let hour = day_ms / 3_600_000;
    let minute = day_ms % 3_600_000 / 60_000;
    let second = day_ms % 60_000 / 1_000;
    let millisecond = day_ms % 1_000;
    Ok(format!(
        "{year:04}-{month:02}-{day:02}T{hour:02}:{minute:02}:{second:02}.{millisecond:03}Z"
    ))
}

fn utc_timestamp_ceil(unix_ns: i128) -> Result<String, String> {
    const NS_PER_MS: i128 = 1_000_000;
    let remainder = unix_ns.rem_euclid(NS_PER_MS);
    let rounded = if remainder == 0 {
        unix_ns
    } else {
        unix_ns
            .checked_add(NS_PER_MS - remainder)
            .ok_or_else(|| "UTC timestamp millisecond rounding overflow".to_string())?
    };
    utc_timestamp(rounded)
}

#[cfg(test)]
fn decimal_i128(value: &str) -> Result<i128, String> {
    value
        .parse()
        .map_err(|_| format!("{value:?} is not a decimal i128 nanosecond value"))
}

// Howard Hinnant's civil-from-days algorithm, with day zero at 1970-01-01.
fn civil_from_days(days_since_epoch: i64) -> (i64, i64, i64) {
    let z = days_since_epoch + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let day_of_era = z - era * 146_097;
    let year_of_era =
        (day_of_era - day_of_era / 1_460 + day_of_era / 36_524 - day_of_era / 146_096) / 365;
    let mut year = year_of_era + era * 400;
    let day_of_year = day_of_era - (365 * year_of_era + year_of_era / 4 - year_of_era / 100);
    let month_prime = (5 * day_of_year + 2) / 153;
    let day = day_of_year - (153 * month_prime + 2) / 5 + 1;
    let month = month_prime + if month_prime < 10 { 3 } else { -9 };
    year += i64::from(month <= 2);
    (year, month, day)
}

#[cfg(test)]
mod tests {
    use rolodex_core::processor::{
        InitMetadata, MetadataSource, PartMetadata, RecordingDetails, ThumbnailFrameMetadata,
        ThumbnailMetadata,
    };

    use super::*;

    fn metadata(object_key: &str, start_utc_ns: &str) -> RecordingMetadata {
        let start_utc_ns = start_utc_ns.parse::<i64>().unwrap();
        RecordingMetadata {
            playback_parts: Vec::new(),
            schema_version: 3,
            processor_version: "5".into(),
            source: MetadataSource {
                object_key: object_key.into(),
                etag: "etag".into(),
                byte_length: 1_000_000,
            },
            recording: RecordingDetails {
                video_duration_ns: 600_000_000_000,
                stream_id: Some("stream-id".into()),
                segment_number: Some(1),
                start_dts_ns: Some(0),
                start_utc_ns: Some(start_utc_ns),
            },
            init: InitMetadata {
                sha256: "abc123".into(),
                byte_length: 512,
            },
            tracks: Vec::new(),
            parts: vec![
                PartMetadata {
                    offset: 1_000,
                    length: 100,
                    sequence_number: 1,
                    start_ns: 0,
                    start_utc_ns: Some(start_utc_ns),
                    duration_ns: 1_000_000_000,
                    presentation_start_ns: 0,
                    presentation_duration_ns: 1_000_000_000,
                    video_starts_with_sync: true,
                    first_video_sync_ns: Some(0),
                    first_video_sync_presentation_ns: Some(0),
                    track_fragments: Vec::new(),
                },
                PartMetadata {
                    offset: 1_100,
                    length: 150,
                    sequence_number: 2,
                    start_ns: 1_000_000_000,
                    start_utc_ns: None,
                    duration_ns: 1_000_000_000,
                    presentation_start_ns: 1_000_000_000,
                    presentation_duration_ns: 1_000_000_000,
                    video_starts_with_sync: false,
                    first_video_sync_ns: None,
                    first_video_sync_presentation_ns: None,
                    track_fragments: Vec::new(),
                },
                PartMetadata {
                    offset: 1_250,
                    length: 200,
                    sequence_number: 3,
                    start_ns: 2_000_000_000,
                    start_utc_ns: None,
                    duration_ns: 1_000_000_000,
                    presentation_start_ns: 2_000_000_000,
                    presentation_duration_ns: 1_000_000_000,
                    video_starts_with_sync: true,
                    first_video_sync_ns: Some(2_000_000_000),
                    first_video_sync_presentation_ns: Some(2_000_000_000),
                    track_fragments: Vec::new(),
                },
            ],
            thumbnail: ThumbnailMetadata {
                interval_ns: Some(10_000_000_000),
                error: None,
                frame_count: 2,
                columns: 2,
                rows: 1,
                frame_width: 240,
                frame_height: 136,
                sprite_width: 480,
                sprite_height: 136,
                frames: vec![
                    ThumbnailFrameMetadata {
                        index: 0,
                        target_ns: 0,
                        presentation_time_ns: 0,
                        sample_offset: 1_010,
                        sample_length: 50,
                    },
                    ThumbnailFrameMetadata {
                        index: 1,
                        target_ns: 10_000_000_000,
                        presentation_time_ns: 10_033_333_333,
                        sample_offset: 1_260,
                        sample_length: 50,
                    },
                ],
            },
        }
    }

    #[test]
    fn converts_unix_nanoseconds_to_iso_utc_dates() {
        assert_eq!(utc_date("0").unwrap(), "1970-01-01");
        assert_eq!(utc_date("-1").unwrap(), "1969-12-31");
        assert_eq!(utc_date("1786622400000000000").unwrap(), "2026-08-13");
        assert_eq!(utc_date("1805587200000000000").unwrap(), "2027-03-21");
        assert_eq!(
            utc_timestamp(1_786_622_400_123_456_789).unwrap(),
            "2026-08-13T12:00:00.123Z"
        );
        assert_eq!(
            utc_timestamp_ceil(1_786_622_400_123_456_789).unwrap(),
            "2026-08-13T12:00:00.124Z"
        );
    }

    #[test]
    fn projects_only_playback_and_thumbnail_fields() {
        let value = ArchiveRecording::from_metadata(
            &metadata(
                "recordings/h265/cam/2026-08-13_12-00-00.mp4",
                "1786622400000000000",
            ),
            "init/abc123.mp4".into(),
            Some("artifacts/h265/cam/2026-08-13_12-00-00/thumbnail.jpg".into()),
        )
        .unwrap();

        assert_eq!(
            value.segments,
            vec![
                ArchiveSegment {
                    pieces: Vec::new(),
                    presentation_start_ns: 0,
                    byte_offset: 1_000,
                    byte_length: 250,
                },
                ArchiveSegment {
                    pieces: Vec::new(),
                    presentation_start_ns: 2_000_000_000,
                    byte_offset: 1_250,
                    byte_length: 200,
                },
            ]
        );
        let thumbnail = value.thumbnail.unwrap();
        assert_eq!(thumbnail.presentation_ns, vec![0, 10_033_333_333]);
        assert_eq!(
            thumbnail.cell,
            ThumbnailCell {
                width: 240,
                height: 136
            }
        );
        assert_eq!(
            serde_json::to_value(&thumbnail).unwrap(),
            serde_json::json!({
                "key": "artifacts/h265/cam/2026-08-13_12-00-00/thumbnail.jpg",
                "intervalNs": "10000000000",
                "cell": { "width": 240, "height": 136 },
                "columns": 2,
                "presentationNs": ["0", "10033333333"]
            })
        );
        let encoded = serde_json::to_vec(&thumbnail).unwrap();
        let decoded: ArchiveThumbnail = serde_json::from_slice(&encoded).unwrap();
        assert_eq!(decoded, thumbnail);
    }

    #[test]
    fn daily_merge_replaces_a_source_and_sorts_chronologically() {
        let later = ArchiveRecording::from_metadata(
            &metadata("recordings/cam/later.mp4", "1786626000000000000"),
            "init/abc.mp4".into(),
            None,
        )
        .unwrap();
        let earlier = ArchiveRecording::from_metadata(
            &metadata("recordings/cam/earlier.mp4", "1786622400000000000"),
            "init/abc.mp4".into(),
            None,
        )
        .unwrap();
        let replacement = ArchiveRecording {
            duration_ns: 42,
            ..later.clone()
        };
        let mut day = DailyArchive::new("cam".into(), "2026-08-13".into());

        day.merge(later).unwrap();
        day.merge(earlier).unwrap();
        day.merge(replacement).unwrap();

        assert_eq!(day.recordings.len(), 2);
        assert_eq!(day.recordings[0].source, "recordings/cam/earlier.mp4");
        assert_eq!(day.recordings[1].duration_ns, 42);
    }

    #[test]
    fn serializes_the_compact_browser_contract() {
        let mut archive = StreamArchive::new("h265/cam".into());
        archive.merge_day(
            "2026-08-13".into(),
            ArchiveDay {
                key: "artifacts/h265/cam/archive/2026-08-13.json".into(),
                started_at: "2026-08-13T12:00:00.000Z".into(),
                ended_at: "2026-08-13T12:10:00.000Z".into(),
            },
        );
        assert_eq!(
            serde_json::to_value(archive).unwrap(),
            serde_json::json!({
                "version": 2,
                "stream": "h265/cam",
                "days": {
                    "2026-08-13": {
                        "key": "artifacts/h265/cam/archive/2026-08-13.json",
                        "startedAt": "2026-08-13T12:00:00.000Z",
                        "endedAt": "2026-08-13T12:10:00.000Z"
                    }
                }
            })
        );

        let recording = ArchiveRecording::from_metadata(
            &metadata(
                "recordings/h265/cam/2026-08-13_12-00-00.mp4",
                "1786622400000000000",
            ),
            "init/abc123.mp4".into(),
            None,
        )
        .unwrap();
        let value = serde_json::to_value(&recording).unwrap();
        assert_eq!(value["id"], "recordings/h265/cam/2026-08-13_12-00-00.mp4");
        assert_eq!(value["startedAt"], "2026-08-13T12:00:00.000Z");
        assert_eq!(value["endedAt"], "2026-08-13T12:10:00.000Z");
        assert!(value.get("startUtcNs").is_none());
        assert_eq!(value["durationNs"], "600000000000");
        assert_eq!(
            value["source"],
            "recordings/h265/cam/2026-08-13_12-00-00.mp4"
        );
        assert_eq!(value["init"], "init/abc123.mp4");
        assert_eq!(
            value["segments"][0],
            serde_json::json!({
                "presentationStartNs": "0",
                "byteOffset": 1000,
                "byteLength": 250
            })
        );
        assert!(value.get("thumbnail").is_none());
        assert!(value.get("parts").is_none());

        let encoded = serde_json::to_vec(&recording).unwrap();
        let decoded: ArchiveRecording = serde_json::from_slice(&encoded).unwrap();
        assert_eq!(decoded, recording);
    }
    #[test]
    fn pages_use_five_minute_partitions_and_heads_cover_gaps_and_cross_midnight() {
        let mut recording = ArchiveRecording::from_metadata(
            &metadata("recordings/cam/one.mp4", "1786622400000000000"),
            "init/abc.mp4".into(),
            None,
        )
        .unwrap();
        recording.started_at = "2026-08-13T23:59:59.999Z".into();
        assert_eq!(recording_bucket(&recording).unwrap(), "2026-08-13T23:55");
        let entry = ArchivePageEntry {
            bucket: "2026-08-13T12:00".into(),
            coverage: ArchiveDay {
                key: "index.early.json".into(),
                started_at: "2026-08-13T12:00:00.000Z".into(),
                ended_at: "2026-08-13T12:10:00.000Z".into(),
            },
        };
        let directory = ArchivePageDirectory {
            version: 2,
            stream: "cam".into(),
            date: "2026-08-13".into(),
            pages: vec![entry],
        };
        let tail = ArchivePageEntry {
            bucket: "2026-08-13T23:55".into(),
            coverage: ArchiveDay {
                key: "index.tail.json".into(),
                started_at: "2026-08-13T23:59:59.999Z".into(),
                ended_at: "2026-08-14T00:00:30.000Z".into(),
            },
        };
        let head = ArchiveHead::from_pages(&directory, "pages.hash.json".into(), tail);
        assert_eq!(head.started_at, "2026-08-13T12:00:00.000Z");
        assert_eq!(head.ended_at, "2026-08-14T00:00:30.000Z");
        head.validate("cam", "2026-08-13").unwrap();
        assert!(head.validate("other", "2026-08-13").is_err());
        let wire = serde_json::to_value(&head).unwrap();
        assert_eq!(wire["tail"]["key"], "index.tail.json");
        assert!(wire["tail"].get("coverage").is_none());
        assert_eq!(serde_json::from_value::<ArchiveHead>(wire).unwrap(), head);
    }
}
