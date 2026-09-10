//! CAS-published paged archive storage. Write immutable pages and directories before
//! publishing their head; a losing writer rereads and merges before retrying.
//! The newest partition lives in the head as a mutable tail reference. Ordinary
//! appends replace that reference without rewriting the directory of older pages.
//! Directory writes occur only when its entries change (rollover, late writes, or pruning).
//! Superseded objects remain unreachable until an external sweep; see README.md.
use crate::{
    key::{ArchiveKeys, derive_archive_keys},
    manifest::{
        ArchiveHead, ArchivePageDirectory, ArchivePageEntry, ArchiveRecording, DailyArchive,
        StreamArchive, recording_bucket,
    },
};
use serde::{Serialize, de::DeserializeOwned};
use sha2::{Digest, Sha256};
use worker::{Bucket, Conditional, HttpMetadata};

#[derive(Debug)]
pub(crate) enum ArchiveError {
    Permanent(String),
    Retry(String),
}
const IMMUTABLE_CACHE_CONTROL: &str = "public, max-age=31536000, immutable";
const MANIFEST_CACHE_CONTROL: &str = "public, max-age=5, must-revalidate";
const MAX_MANIFEST_WRITE_ATTEMPTS: usize = 5;
pub(crate) const MAX_DAILY_ARCHIVE_BYTES: u64 = 32 * 1024 * 1024;
const MAX_STREAM_ARCHIVE_BYTES: u64 = 2 * 1024 * 1024;

pub(crate) struct VersionedJson<T> {
    pub(crate) value: T,
    etag: String,
}

pub(crate) async fn load_json<T: DeserializeOwned>(
    bucket: &Bucket,
    key: &str,
    maximum_bytes: u64,
) -> std::result::Result<Option<VersionedJson<T>>, ArchiveError> {
    let Some(object) =
        bucket.get(key).execute().await.map_err(|error| {
            ArchiveError::Retry(format!("R2 manifest GET {key} failed: {error}"))
        })?
    else {
        return Ok(None);
    };
    if object.size() > maximum_bytes {
        return Err(ArchiveError::Permanent(format!(
            "manifest {key} is {} bytes, above the {maximum_bytes}-byte limit",
            object.size()
        )));
    }
    let etag = object.etag();
    let body = object
        .body()
        .ok_or_else(|| ArchiveError::Retry(format!("R2 manifest GET {key} returned no body")))?;
    let bytes = body
        .bytes()
        .await
        .map_err(|error| ArchiveError::Retry(format!("R2 manifest body {key} failed: {error}")))?;
    let value = serde_json::from_slice(&bytes).map_err(|error| {
        ArchiveError::Permanent(format!("manifest {key} contains invalid JSON: {error}"))
    })?;
    Ok(Some(VersionedJson { value, etag }))
}

// Current and previous UTC days stay short-lived; older heads remain revalidatable.
fn manifest_cache_control(date: Option<&str>) -> &'static str {
    if let Some(date) = date {
        let start =
            worker::Date::new(worker::DateInit::String(format!("{date}T00:00:00Z"))).as_millis();
        if worker::Date::now().as_millis().saturating_sub(start) < 2 * 86_400_000 {
            return MANIFEST_CACHE_CONTROL;
        }
    }
    "public, max-age=300, must-revalidate"
}

/// All bounds use utc_timestamp's fixed-width UTC format, so string order is time order.
/// Compute once per publication so pruning and replay suppression share the same instant.
fn retention_cutoff(retention_seconds: u64) -> Result<Option<String>, ArchiveError> {
    if retention_seconds == 0 {
        return Ok(None);
    }
    let millis = worker::Date::now()
        .as_millis()
        .saturating_sub(retention_seconds.saturating_mul(1000));
    crate::manifest::utc_timestamp(i128::from(millis) * 1_000_000)
        .map(Some)
        .map_err(ArchiveError::Permanent)
}

async fn put_json_if_current<T: Serialize>(
    bucket: &Bucket,
    key: &str,
    value: &T,
    etag: Option<String>,
    date: Option<&str>,
) -> std::result::Result<bool, ArchiveError> {
    let bytes = serde_json::to_vec(value).map_err(|error| {
        ArchiveError::Permanent(format!("manifest serialization failed: {error}"))
    })?;
    let condition = match etag {
        Some(etag) => Conditional {
            etag_matches: Some(etag),
            ..Conditional::default()
        },
        None => Conditional {
            etag_does_not_match: Some("*".into()),
            ..Conditional::default()
        },
    };
    let result = bucket
        .put(key, bytes)
        .only_if(condition)
        .http_metadata(HttpMetadata {
            content_type: Some("application/json".into()),
            cache_control: Some(manifest_cache_control(date).into()),
            ..HttpMetadata::default()
        })
        .execute()
        .await
        .map_err(|error| ArchiveError::Retry(format!("R2 manifest PUT {key} failed: {error}")))?;
    Ok(result.is_some())
}

pub(crate) async fn update_archive_manifests(
    bucket: &Bucket,
    source_prefix: &str,
    artifact_prefix: &str,
    retention_seconds: u64,
    metadata: &rolodex_core::RecordingMetadata,
    init_key: String,
    thumbnail_key: Option<String>,
) -> std::result::Result<ArchiveKeys, ArchiveError> {
    let recording = ArchiveRecording::from_metadata(metadata, init_key, thumbnail_key)
        .map_err(ArchiveError::Permanent)?;
    let date = recording
        .started_at
        .get(..10)
        .ok_or_else(|| ArchiveError::Permanent("recording UTC start is not an ISO date".into()))?
        .to_string();
    let archive_keys = derive_archive_keys(
        &metadata.source.object_key,
        source_prefix,
        artifact_prefix,
        &date,
    )
    .ok_or_else(|| ArchiveError::Permanent("could not derive archive manifest keys".into()))?;

    // Retried notifications must not resurrect footage outside the rolling archive window.
    let cutoff = retention_cutoff(retention_seconds)?;
    if cutoff
        .as_ref()
        .is_some_and(|cutoff| recording.ended_at <= *cutoff)
    {
        return Ok(archive_keys);
    }
    update_daily_archive(bucket, &archive_keys, recording, &date, cutoff.as_deref()).await?;
    update_stream_archive(
        bucket,
        &archive_keys,
        &date,
        retention_seconds,
        cutoff.as_deref(),
    )
    .await?;
    Ok(archive_keys)
}

/// Content-addressed writes may outlive a failed CAS; external sweeping reclaims them.
async fn put_immutable_json<T: Serialize>(
    bucket: &Bucket,
    prefix: &str,
    kind: &str,
    value: &T,
) -> Result<String, ArchiveError> {
    let bytes =
        serde_json::to_vec(value).map_err(|error| ArchiveError::Permanent(error.to_string()))?;
    if bytes.len() as u64 > MAX_DAILY_ARCHIVE_BYTES {
        return Err(ArchiveError::Permanent(
            "archive page exceeds size limit".into(),
        ));
    }
    let hash = rolodex_core::hex(&Sha256::digest(&bytes));
    let key = format!("{prefix}/{kind}.{hash}.json");
    bucket
        .put(&key, bytes)
        .only_if(Conditional {
            etag_does_not_match: Some("*".into()),
            ..Conditional::default()
        })
        .http_metadata(HttpMetadata {
            content_type: Some("application/json".into()),
            cache_control: Some(IMMUTABLE_CACHE_CONTROL.into()),
            ..HttpMetadata::default()
        })
        .execute()
        .await
        .map_err(|error| ArchiveError::Retry(format!("immutable PUT {key}: {error}")))?;
    Ok(key)
}

async fn required_json<T: DeserializeOwned>(bucket: &Bucket, key: &str) -> Result<T, ArchiveError> {
    load_json(bucket, key, MAX_DAILY_ARCHIVE_BYTES)
        .await?
        .map(|entry| entry.value)
        .ok_or_else(|| ArchiveError::Retry(format!("archive reference {key} is missing")))
}

async fn write_page(
    bucket: &Bucket,
    prefix: &str,
    partition: String,
    page: &DailyArchive,
) -> Result<ArchivePageEntry, ArchiveError> {
    let key = put_immutable_json(bucket, prefix, "index", page).await?;
    Ok(ArchivePageEntry {
        bucket: partition,
        coverage: page.coverage(key).map_err(ArchiveError::Permanent)?,
    })
}

async fn read_paged_day(
    bucket: &Bucket,
    keys: &ArchiveKeys,
    date: &str,
    head: Option<ArchiveHead>,
) -> Result<(ArchivePageDirectory, Option<ArchivePageEntry>), ArchiveError> {
    match head {
        Some(head) => {
            head.validate(&keys.stream, date)
                .map_err(ArchiveError::Permanent)?;
            let directory: ArchivePageDirectory = required_json(bucket, &head.pages).await?;
            if directory.version != 2 || directory.stream != keys.stream || directory.date != date {
                return Err(ArchiveError::Permanent(
                    "page directory identity mismatch".into(),
                ));
            }
            Ok((directory, Some(head.tail)))
        }
        None => Ok((
            ArchivePageDirectory {
                version: 2,
                stream: keys.stream.clone(),
                date: date.into(),
                pages: Vec::new(),
            },
            None,
        )),
    }
}

async fn update_partition(
    bucket: &Bucket,
    prefix: &str,
    directory: &mut ArchivePageDirectory,
    tail: Option<ArchivePageEntry>,
    recording: ArchiveRecording,
) -> Result<ArchivePageEntry, ArchiveError> {
    let partition = recording_bucket(&recording).map_err(ArchiveError::Permanent)?;
    let mut entries = directory.pages.clone();
    if let Some(tail) = tail {
        entries.push(tail);
    }
    let existing = entries.iter().find(|page| page.bucket == partition);
    let mut page = match existing {
        Some(entry) => required_json::<DailyArchive>(bucket, &entry.coverage.key).await?,
        None => DailyArchive::new(directory.stream.clone(), directory.date.clone()),
    };
    page.validate(&directory.stream, &directory.date)
        .map_err(ArchiveError::Permanent)?;
    page.merge(recording).map_err(ArchiveError::Permanent)?;
    entries.retain(|page| page.bucket != partition);
    entries.push(write_page(bucket, prefix, partition, &page).await?);
    // recording_bucket emits fixed-width UTC buckets; lexical order selects the newest tail.
    entries.sort_by(|left, right| left.bucket.cmp(&right.bucket));
    let tail = entries.pop().expect("the updated partition exists");
    directory.pages = entries;
    Ok(tail)
}

/// Prune only at publication, using page bounds to avoid reading wholly expired pages.
async fn prune_directory(
    bucket: &Bucket,
    prefix: &str,
    directory: &mut ArchivePageDirectory,
    tail: Option<ArchivePageEntry>,
    cutoff: Option<&str>,
) -> Result<Option<ArchivePageEntry>, ArchiveError> {
    let Some(cutoff) = cutoff else {
        return Ok(tail);
    };
    let mut retained = Vec::new();
    let entries = std::mem::take(&mut directory.pages).into_iter().chain(tail);
    for entry in entries {
        if entry.coverage.ended_at.as_str() <= cutoff {
            continue;
        }
        if entry.coverage.started_at.as_str() >= cutoff {
            retained.push(entry);
            continue;
        }
        let mut page: DailyArchive = required_json(bucket, &entry.coverage.key).await?;
        let count = page.recordings.len();
        page.recordings
            .retain(|recording| recording.ended_at.as_str() > cutoff);
        if page.recordings.is_empty() {
            continue;
        }
        if page.recordings.len() == count {
            retained.push(entry);
        } else {
            retained.push(write_page(bucket, prefix, entry.bucket, &page).await?);
        }
    }
    retained.sort_by(|a, b| a.bucket.cmp(&b.bucket));
    let tail = retained.pop();
    directory.pages = retained;
    Ok(tail)
}

async fn update_daily_archive(
    bucket: &Bucket,
    keys: &ArchiveKeys,
    recording: ArchiveRecording,
    date: &str,
    cutoff: Option<&str>,
) -> Result<(), ArchiveError> {
    let prefix = keys.day_prefix();
    for _ in 0..MAX_MANIFEST_WRITE_ATTEMPTS {
        let existing = load_json::<ArchiveHead>(bucket, &keys.day, MAX_DAILY_ARCHIVE_BYTES).await?;
        let previous_pages = existing.as_ref().map(|entry| entry.value.pages.clone());
        let (document, etag) = match existing {
            Some(entry) => (Some(entry.value), Some(entry.etag)),
            None => (None, None),
        };
        let (mut directory, tail) = read_paged_day(bucket, keys, date, document).await?;
        let previous_directory = directory.clone();
        let tail = prune_directory(bucket, prefix, &mut directory, tail, cutoff).await?;
        let tail =
            update_partition(bucket, prefix, &mut directory, tail, recording.clone()).await?;
        let pages = match previous_pages.filter(|_| directory == previous_directory) {
            Some(key) => key,
            None => put_immutable_json(bucket, prefix, "pages", &directory).await?,
        };
        let head = ArchiveHead::from_pages(&directory, pages, tail);
        // Commit only after every referenced object is durable. Losing writers reread and merge.
        if put_json_if_current(bucket, &keys.day, &head, etag, Some(date)).await? {
            return Ok(());
        }
    }
    Err(ArchiveError::Retry(format!(
        "daily archive {} changed during every conditional write attempt",
        keys.day
    )))
}

async fn update_stream_archive(
    bucket: &Bucket,
    keys: &ArchiveKeys,
    date: &str,
    retention_seconds: u64,
    cutoff: Option<&str>,
) -> std::result::Result<(), ArchiveError> {
    for _ in 0..MAX_MANIFEST_WRITE_ATTEMPTS {
        let existing =
            load_json::<StreamArchive>(bucket, &keys.root, MAX_STREAM_ARCHIVE_BYTES).await?;
        let (mut archive, etag) = match existing {
            Some(existing) => {
                // Hard cutover: v1 discovery is replaced, without migrating old recording indexes.
                if existing.value.version == 1 {
                    (StreamArchive::new(keys.stream.clone()), Some(existing.etag))
                } else {
                    existing
                        .value
                        .validate(&keys.stream)
                        .map_err(ArchiveError::Permanent)?;
                    (existing.value, Some(existing.etag))
                }
            }
            None => (StreamArchive::new(keys.stream.clone()), None),
        };

        let previous_days = archive.days.len();
        if let Some(cutoff) = cutoff {
            archive.days.retain(|day, _| day.as_str() >= &cutoff[..10]);
        }
        let prefix = keys.archive_prefix();
        let registered = archive
            .days
            .get(date)
            .is_some_and(|day| day.key == keys.day);
        let retention = (retention_seconds > 0).then_some(retention_seconds);
        if previous_days == archive.days.len()
            && registered
            && archive.head_prefix.as_ref() == Some(&prefix)
            && archive.retention_seconds == retention
        {
            return Ok(());
        }
        // Root bounds are discovery-time hints. Clients read current coverage from daily heads.
        if !registered {
            let head: ArchiveHead = required_json(bucket, &keys.day).await?;
            head.validate(&keys.stream, date)
                .map_err(ArchiveError::Permanent)?;
            archive.merge_day(date.into(), head.coverage(keys.day.clone()));
        }
        archive.head_prefix = Some(prefix);
        archive.retention_seconds = retention;
        // Root day membership is pruned without sweeping historical objects.
        if put_json_if_current(bucket, &keys.root, &archive, etag, None).await? {
            return Ok(());
        }
    }
    Err(ArchiveError::Retry(format!(
        "stream archive {} changed during every conditional write attempt",
        keys.root
    )))
}
