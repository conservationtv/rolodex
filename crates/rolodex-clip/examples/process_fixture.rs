use std::{collections::HashMap, env, error::Error as StdError, fs, path::PathBuf};

use async_trait::async_trait;
use rolodex_clip::{
    CandidateFile, ClipRequest, Result, SourceOpener, SourceStamp, default_thumbnail_options,
    test_util::shift_mediamtx_start_ntp,
};
use rolodex_core::{
    ByteSource, ProcessorOptions, SourceIdentity, process_recording, source::MemorySource,
};

struct FixtureOpener(HashMap<String, Vec<u8>>);

#[async_trait(?Send)]
impl SourceOpener for FixtureOpener {
    async fn stamp(&self, id: &str) -> Result<SourceStamp> {
        let bytes = self
            .0
            .get(id)
            .ok_or(rolodex_clip::Error::InvalidRequest("unknown fixture ID"))?;
        Ok(SourceStamp {
            byte_length: bytes.len() as u64,
            modified_utc_ns: 0,
        })
    }

    async fn open(&self, id: &str) -> Result<Box<dyn ByteSource>> {
        let bytes = self
            .0
            .get(id)
            .cloned()
            .ok_or(rolodex_clip::Error::InvalidRequest("unknown fixture ID"))?;
        Ok(Box::new(MemorySource::new(bytes)))
    }
}

fn describe(
    id: &str,
    bytes: &[u8],
) -> std::result::Result<(CandidateFile, u64, u64), Box<dyn StdError>> {
    let output = futures::executor::block_on(process_recording(
        &MemorySource::new(bytes.to_vec()),
        SourceIdentity {
            object_key: id.into(),
            etag: "fixture".into(),
        },
        ProcessorOptions {
            thumbnail: None,
            ..ProcessorOptions::default()
        },
    ))?;
    let start = output
        .metadata
        .parts
        .first()
        .and_then(|part| part.start_utc_ns)
        .and_then(|value| u64::try_from(value).ok())
        .ok_or("fixture has no UTC media parts")?;
    let last = output
        .metadata
        .parts
        .last()
        .ok_or("fixture has no media parts")?;
    let end = u64::try_from(last.start_utc_ns.ok_or("fixture part has no UTC time")?)?
        .checked_add(last.duration_ns)
        .ok_or("fixture end overflow")?;
    Ok((
        CandidateFile {
            id: id.into(),
            start_utc_ns: start,
            stamp: SourceStamp {
                byte_length: bytes.len() as u64,
                modified_utc_ns: 0,
            },
        },
        start,
        end,
    ))
}

fn main() -> std::result::Result<(), Box<dyn StdError>> {
    let mut args = env::args_os().skip(1);
    let input = PathBuf::from(args.next().ok_or("missing input path")?);
    let output = PathBuf::from(args.next().ok_or("missing output directory")?);
    let repeat = args.next().is_some_and(|value| value == "--repeat");
    if args.next().is_some() {
        return Err("usage: process_fixture INPUT.mp4 OUTPUT_DIRECTORY [--repeat]".into());
    }

    let bytes = fs::read(&input)?;
    let first_id = format!("{}#1", input.display());
    let (first_candidate, first_start, first_end) = describe(&first_id, &bytes)?;
    let mut files = HashMap::from([(first_id, bytes.clone())]);
    let mut candidates = vec![first_candidate];
    let mut clip_end = first_end;
    if repeat {
        let repeated = shift_mediamtx_start_ntp(&bytes, i64::try_from(first_end - first_start)?)?;
        let second_id = format!("{}#2", input.display());
        let (second_candidate, _, second_end) = describe(&second_id, &repeated)?;
        files.insert(second_id, repeated);
        candidates.push(second_candidate);
        clip_end = second_end;
    }
    let opener = FixtureOpener(files);
    let request = ClipRequest {
        stream: "fixture".into(),
        from_utc_ns: first_start,
        to_utc_ns: clip_end.checked_sub(1).ok_or("empty fixture range")?,
        thumbnail: default_thumbnail_options(),
    };
    let descriptor =
        futures::executor::block_on(rolodex_clip::find(&opener, &candidates, &request))?;
    let artifacts = futures::executor::block_on(rolodex_clip::process(&opener, &descriptor))?;

    fs::create_dir_all(&output)?;
    fs::write(output.join("clip.mp4"), artifacts.media.bytes)?;
    fs::write(output.join("index.m3u8"), artifacts.manifest.bytes)?;
    fs::write(output.join("descriptor.json"), artifacts.descriptor.bytes)?;
    if let Some(thumbnail) = artifacts.thumbnail {
        fs::write(output.join("thumbnail.jpg"), thumbnail.bytes)?;
    }
    Ok(())
}
