use std::{
    collections::HashMap,
    env, fs,
    path::{Path, PathBuf},
};

use async_trait::async_trait;
use chrono::{DateTime, NaiveDateTime};
use rolodex_clip::{
    CandidateFile, ClipRequest, Error as ClipError, Result as ClipResult, SourceOpener,
    SourceStamp, ThumbnailOptions, default_thumbnail_options,
};
use rolodex_core::{
    ByteSource, Error as CoreError, parse_duration_ns,
    source::{FileReadCounter, FileSource, file_stamp},
};
use serde::Serialize;

use crate::{next_value, parse_number, r2};

const USAGE: &str = "usage: rolodex clip --recordings DIRECTORY --stream NAME --from RFC3339 --to RFC3339 \
    (--out DIRECTORY | --bucket NAME --endpoint URL) [--base-url URL] \
    [--thumbnail-interval DURATION] [--max-width N] [--max-height N] [--quality N]";

#[derive(Debug)]
struct Args {
    recordings: PathBuf,
    stream: String,
    from_utc_ns: u64,
    to_utc_ns: u64,
    out: Option<PathBuf>,
    bucket: Option<String>,
    endpoint: Option<String>,
    base_url: Option<String>,
    thumbnail: ThumbnailOptions,
}

#[derive(Serialize)]
struct Metrics {
    files_scanned: usize,
    parts_selected: usize,
    bytes_read: u64,
    bytes_written: u64,
    bytes_uploaded: u64,
    start_snap_ns: i64,
    end_snap_ns: i64,
}

struct FileOpener {
    paths: HashMap<String, PathBuf>,
    bytes_read: FileReadCounter,
}

impl FileOpener {
    fn path(&self, id: &str) -> ClipResult<&Path> {
        self.paths
            .get(id)
            .map(PathBuf::as_path)
            .ok_or_else(|| source_error(id, "source is absent from the scanned listing"))
    }
}

#[async_trait(?Send)]
impl SourceOpener for FileOpener {
    async fn stamp(&self, id: &str) -> ClipResult<SourceStamp> {
        let stamp = file_stamp(self.path(id)?).map_err(|message| source_error(id, message))?;
        Ok(SourceStamp {
            byte_length: stamp.byte_length,
            modified_utc_ns: stamp.modified_utc_ns,
        })
    }

    async fn open(&self, id: &str) -> ClipResult<Box<dyn ByteSource>> {
        let path = self.path(id)?;
        FileSource::open_with_counter(path, self.bytes_read.clone())
            .map(|source| Box::new(source) as Box<dyn ByteSource>)
            .map_err(|message| source_error(id, message))
    }
}

fn source_error(id: &str, message: impl Into<String>) -> ClipError {
    ClipError::Source {
        id: id.into(),
        source: CoreError::Source(message.into()),
    }
}

fn parse_args(values: impl IntoIterator<Item = String>) -> Result<Args, String> {
    let mut values = values.into_iter();
    let mut recordings = None;
    let mut stream = None;
    let mut from = None;
    let mut to = None;
    let mut out = None;
    let mut bucket = None;
    let mut endpoint = None;
    let mut base_url = None;
    let mut thumbnail = default_thumbnail_options();
    while let Some(flag) = values.next() {
        match flag.as_str() {
            "--recordings" => recordings = Some(PathBuf::from(next_value(&mut values, &flag)?)),
            "--stream" => stream = Some(next_value(&mut values, &flag)?),
            "--from" => from = Some(parse_timestamp(&next_value(&mut values, &flag)?)?),
            "--to" => to = Some(parse_timestamp(&next_value(&mut values, &flag)?)?),
            "--out" => out = Some(PathBuf::from(next_value(&mut values, &flag)?)),
            "--bucket" => bucket = Some(next_value(&mut values, &flag)?),
            "--endpoint" => endpoint = Some(next_value(&mut values, &flag)?),
            "--base-url" => base_url = Some(next_value(&mut values, &flag)?),
            "--thumbnail-interval" => {
                thumbnail.interval_ns = Some(
                    parse_duration_ns(&next_value(&mut values, &flag)?)
                        .map_err(|error| format!("invalid {flag}: {error}"))?,
                );
            }
            "--max-width" => thumbnail.max_width = parse_number(&mut values, &flag)?,
            "--max-height" => thumbnail.max_height = parse_number(&mut values, &flag)?,
            "--quality" => thumbnail.quality = parse_number(&mut values, &flag)?,
            "-h" | "--help" => return Err(USAGE.into()),
            _ => return Err(format!("unknown option {flag}\n{USAGE}")),
        }
    }
    let local = out.is_some();
    let remote = bucket.is_some() || endpoint.is_some();
    if local == remote || bucket.is_some() != endpoint.is_some() {
        return Err(format!(
            "select either --out or both --bucket and --endpoint\n{USAGE}"
        ));
    }
    if local && base_url.is_some() {
        return Err("--base-url is only valid with R2 upload mode".into());
    }
    Ok(Args {
        recordings: recordings.ok_or_else(|| format!("--recordings is required\n{USAGE}"))?,
        stream: stream.ok_or_else(|| format!("--stream is required\n{USAGE}"))?,
        from_utc_ns: from.ok_or_else(|| format!("--from is required\n{USAGE}"))?,
        to_utc_ns: to.ok_or_else(|| format!("--to is required\n{USAGE}"))?,
        out,
        bucket,
        endpoint,
        base_url,
        thumbnail,
    })
}

fn parse_timestamp(value: &str) -> Result<u64, String> {
    let parsed = DateTime::parse_from_rfc3339(value)
        .map_err(|error| format!("invalid RFC3339 timestamp {value}: {error}"))?;
    let nanos = parsed
        .timestamp_nanos_opt()
        .ok_or_else(|| format!("timestamp is outside nanosecond range: {value}"))?;
    u64::try_from(nanos).map_err(|_| format!("timestamp is before Unix epoch: {value}"))
}

fn scan_recordings(args: &Args) -> Result<(Vec<CandidateFile>, FileOpener), String> {
    let directory = args.recordings.join(&args.stream);
    let entries = fs::read_dir(&directory)
        .map_err(|error| format!("read {}: {error}", directory.display()))?;
    let mut candidates = Vec::new();
    let mut paths = HashMap::new();
    for entry in entries {
        let entry =
            entry.map_err(|error| format!("read {} entry: {error}", directory.display()))?;
        let path = entry.path();
        let file_type = entry
            .file_type()
            .map_err(|error| format!("stat {}: {error}", path.display()))?;
        if !file_type.is_file() || path.extension().and_then(|value| value.to_str()) != Some("mp4")
        {
            continue;
        }
        let Some(stem) = path.file_stem().and_then(|value| value.to_str()) else {
            continue;
        };
        let Ok(start) = NaiveDateTime::parse_from_str(stem, "%Y-%m-%d_%H-%M-%S-%f") else {
            continue;
        };
        let start_utc_ns =
            u64::try_from(start.and_utc().timestamp_nanos_opt().ok_or_else(|| {
                format!("recording timestamp is outside nanosecond range: {stem}")
            })?)
            .map_err(|_| format!("recording timestamp is before Unix epoch: {stem}"))?;
        let id = format!("{}/{}", args.stream, entry.file_name().to_string_lossy());
        let stamp = file_stamp(&path)?;
        paths.insert(id.clone(), path);
        candidates.push(CandidateFile {
            id,
            start_utc_ns,
            stamp: SourceStamp {
                byte_length: stamp.byte_length,
                modified_utc_ns: stamp.modified_utc_ns,
            },
        });
    }
    candidates.sort_by_key(|candidate| candidate.start_utc_ns);
    Ok((
        candidates,
        FileOpener {
            paths,
            bytes_read: FileSource::read_counter(),
        },
    ))
}

fn write_artifacts(out: &Path, artifacts: &rolodex_clip::ClipArtifacts) -> Result<u64, String> {
    fs::create_dir_all(out).map_err(|error| format!("create {}: {error}", out.display()))?;
    let mut bytes_written = 0u64;
    let outputs = [
        ("clip.mp4", Some(&artifacts.media)),
        ("thumbnail.jpg", artifacts.thumbnail.as_ref()),
        ("descriptor.json", Some(&artifacts.descriptor)),
    ];
    for (name, artifact) in outputs {
        if let Some(artifact) = artifact {
            fs::write(out.join(name), &artifact.bytes)
                .map_err(|error| format!("write {}: {error}", out.join(name).display()))?;
            bytes_written = bytes_written
                .checked_add(artifact.bytes.len() as u64)
                .ok_or_else(|| "written byte count overflow".to_string())?;
        }
    }
    fs::write(out.join("index.m3u8"), &artifacts.manifest.bytes)
        .map_err(|error| format!("write {}: {error}", out.join("index.m3u8").display()))?;
    bytes_written = bytes_written
        .checked_add(artifacts.manifest.bytes.len() as u64)
        .ok_or_else(|| "written byte count overflow".to_string())?;
    Ok(bytes_written)
}

fn snap_delta(actual: u64, requested: u64) -> Result<i64, String> {
    i64::try_from(i128::from(actual) - i128::from(requested))
        .map_err(|_| "snap delta exceeds i64 nanoseconds".to_string())
}

fn run(args: Args) -> Result<(), String> {
    let request = ClipRequest {
        stream: args.stream.clone(),
        from_utc_ns: args.from_utc_ns,
        to_utc_ns: args.to_utc_ns,
        thumbnail: args.thumbnail,
    };
    rolodex_clip::validate_request(&request).map_err(|error| error.to_string())?;
    let (listing, opener) = scan_recordings(&args)?;
    let descriptor = futures::executor::block_on(rolodex_clip::find(&opener, &listing, &request))
        .map_err(|error| error.to_string())?;
    let artifacts = futures::executor::block_on(rolodex_clip::process(&opener, &descriptor))
        .map_err(|error| error.to_string())?;
    let (location, bytes_written, bytes_uploaded) = if let Some(out) = &args.out {
        let bytes_written = write_artifacts(out, &artifacts)?;
        (
            out.join("index.m3u8").display().to_string(),
            bytes_written,
            0,
        )
    } else {
        let access_key = env::var("AWS_ACCESS_KEY_ID")
            .map_err(|_| "AWS_ACCESS_KEY_ID is required for R2 upload".to_string())?;
        let secret_key = env::var("AWS_SECRET_ACCESS_KEY")
            .map_err(|_| "AWS_SECRET_ACCESS_KEY is required for R2 upload".to_string())?;
        let store = r2::R2Store::new(
            args.endpoint.as_deref().expect("validated endpoint"),
            args.bucket.clone().expect("validated bucket"),
            access_key,
            secret_key,
        )
        .map_err(|error| error.to_string())?;
        let uploaded =
            futures::executor::block_on(rolodex_clip::upload(&store, &descriptor, &artifacts))
                .map_err(|error| error.to_string())?;
        let manifest_key = uploaded
            .last()
            .map(|artifact| artifact.key.as_str())
            .ok_or_else(|| "upload produced no artifacts".to_string())?;
        let bytes_uploaded = uploaded
            .iter()
            .filter(|artifact| artifact.outcome == rolodex_clip::UploadOutcome::Created)
            .map(|artifact| artifact.byte_length)
            .sum();
        let base = args
            .base_url
            .as_deref()
            .unwrap_or_else(|| args.endpoint.as_deref().expect("validated endpoint"))
            .trim_end_matches('/');
        let location = if args.base_url.is_some() {
            format!("{base}/{manifest_key}")
        } else {
            format!(
                "{base}/{}/{manifest_key}",
                args.bucket.as_deref().expect("validated bucket")
            )
        };
        (location, 0, bytes_uploaded)
    };
    let metrics = Metrics {
        files_scanned: listing.len(),
        parts_selected: descriptor
            .sources
            .iter()
            .map(|source| source.parts.len())
            .sum(),
        bytes_read: FileSource::counter_value(&opener.bytes_read),
        bytes_written,
        bytes_uploaded,
        start_snap_ns: snap_delta(
            descriptor.actual.from_utc_ns,
            descriptor.requested.from_utc_ns,
        )?,
        end_snap_ns: snap_delta(descriptor.actual.to_utc_ns, descriptor.requested.to_utc_ns)?,
    };
    println!("{location}");
    println!(
        "{}",
        serde_json::to_string(&metrics).map_err(|error| format!("serialize metrics: {error}"))?
    );
    Ok(())
}

pub(crate) fn run_cli(values: impl IntoIterator<Item = String>) -> Result<(), String> {
    parse_args(values).and_then(run)
}

#[cfg(test)]
mod tests {
    use std::time::{SystemTime, UNIX_EPOCH};

    use super::*;
    use chrono::Utc;
    use rolodex_clip::ProcessedDescriptor;
    use rolodex_core::{ProcessorOptions, SourceIdentity, process_recording, source::MemorySource};

    fn fixture() -> Vec<u8> {
        #[cfg(feature = "codec-h264")]
        let relative = "../../fixtures/h264/mediamtx-v1.18.2-640x360.mp4";
        #[cfg(feature = "codec-h265")]
        let relative = "../../fixtures/h265/mediamtx-v1.18.2-640x360.mp4";
        fs::read(format!("{}/{}", env!("CARGO_MANIFEST_DIR"), relative)).unwrap()
    }

    fn temp_directory() -> PathBuf {
        let unique = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        env::temp_dir().join(format!("rolodex-clip-{}-{unique}", std::process::id()))
    }

    #[test]
    fn writes_a_complete_local_artifact_set() {
        let bytes = fixture();
        let indexed = futures::executor::block_on(process_recording(
            &MemorySource::new(bytes.clone()),
            SourceIdentity {
                object_key: "fixture".into(),
                etag: "fixture".into(),
            },
            ProcessorOptions {
                thumbnail: None,
                ..ProcessorOptions::default()
            },
        ))
        .unwrap();
        let start = u64::try_from(indexed.metadata.parts[0].start_utc_ns.unwrap()).unwrap();
        let last = indexed.metadata.parts.last().unwrap();
        let end = u64::try_from(last.start_utc_ns.unwrap()).unwrap() + last.duration_ns;
        let root = temp_directory();
        let stream_dir = root.join("recordings/camera-12");
        let out = root.join("output");
        fs::create_dir_all(&stream_dir).unwrap();
        let timestamp = DateTime::<Utc>::from_timestamp(
            i64::try_from(start / 1_000_000_000).unwrap(),
            (start % 1_000_000_000) as u32,
        )
        .unwrap();
        let filename = timestamp.format("%Y-%m-%d_%H-%M-%S-%6f.mp4").to_string();
        fs::write(stream_dir.join(filename), bytes).unwrap();

        run(Args {
            recordings: root.join("recordings"),
            stream: "camera-12".into(),
            from_utc_ns: start,
            to_utc_ns: end - 1,
            out: Some(out.clone()),
            bucket: None,
            endpoint: None,
            base_url: None,
            thumbnail: ThumbnailOptions {
                interval_ns: Some(200_000_000),
                ..ThumbnailOptions::default()
            },
        })
        .unwrap();

        let media = fs::read(out.join("clip.mp4")).unwrap();
        let manifest = fs::read_to_string(out.join("index.m3u8")).unwrap();
        let sidecar: ProcessedDescriptor =
            serde_json::from_slice(&fs::read(out.join("descriptor.json")).unwrap()).unwrap();
        assert!(!media.is_empty());
        assert!(manifest.ends_with("#EXT-X-ENDLIST\n"));
        assert_eq!(
            manifest.matches("#EXT-X-BYTERANGE:").count(),
            sidecar.clip.sources[0].parts.len()
        );
        let mut expected_offset = sidecar.clip.sources[0].init_len;
        for line in manifest
            .lines()
            .filter_map(|line| line.strip_prefix("#EXT-X-BYTERANGE:"))
        {
            let (length, offset) = line.split_once('@').unwrap();
            let length = length.parse::<u64>().unwrap();
            let offset = offset.parse::<u64>().unwrap();
            assert_eq!(offset, expected_offset);
            expected_offset += length;
            assert!(expected_offset <= media.len() as u64);
        }
        assert_eq!(expected_offset, media.len() as u64);
        assert_eq!(
            sidecar.thumbnail_result.metadata.frame_count as usize,
            sidecar.thumbnail_result.metadata.frames.len()
        );
        assert_eq!(
            &fs::read(out.join("thumbnail.jpg")).unwrap()[..2],
            &[0xff, 0xd8]
        );

        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn parses_rfc3339_offsets_as_utc() {
        assert_eq!(parse_timestamp("1970-01-01T01:00:00+01:00").unwrap(), 0);
    }

    #[test]
    fn rejects_an_unsafe_stream_before_reading_the_recordings_root() {
        let error = run(Args {
            recordings: PathBuf::from("/path/that/must/not/be/read"),
            stream: "../outside".into(),
            from_utc_ns: 1,
            to_utc_ns: 2,
            out: Some(PathBuf::from("unused")),
            bucket: None,
            endpoint: None,
            base_url: None,
            thumbnail: default_thumbnail_options(),
        })
        .unwrap_err();
        assert!(error.contains("safe object-key path"));
    }
}
