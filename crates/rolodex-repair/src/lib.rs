//! Bounded, lossless repairs of oversized independently decodable ranges.
//!
//! A repair works on a recording `rolodex-core` has indexed: it reads the parts
//! that hold a keyframe mid-part from a [`ByteSource`], splits them there, and
//! commits each rewritten part to a [`RepairSink`]. Neither end is tied to a
//! platform, so the same repair runs wherever both can be supplied: against R2
//! in the worker, or against local files before a recording is uploaded.
//!
//! The recording's video track, AVC or HEVC, decides where the cuts go: only its
//! keyframes start a piece. Audio tracks are cut at the same instants, each
//! piece taking the audio that decodes from its keyframe until the next piece's.
#![forbid(unsafe_code)]

use async_trait::async_trait;
pub use rolodex_core::processor::{PlaybackPart, RepairSkipReason};
use rolodex_core::{
    ByteSource, Error, ProcessorOptions, ProcessorOutput, RecordingIndex, Result, SourceIdentity,
    index_recording,
    mp4::{IndexedSample, IndexedTrackFragment, TrackKind, parse_indexed_fragment},
    render_recording_thumbnail,
};
use std::collections::BTreeMap;
const MAX_SECONDS: f64 = 6.0;
/// Decimal-byte playback cut threshold; independent of the per-source-part allocation cap.
const MAX_BYTES: u64 = 4_000_000;
/// Maximum source part read for repair (4 MiB), not the target playback range size.
const MAX_REPAIR: usize = 4 * 1024 * 1024;
/// Rewriting explicit sample tables and adding fragment headers can grow a part.
/// Permit at most 64 KiB of overhead beyond MAX_REPAIR; both estimates and actual
/// output enforce this bound. This is a safety allowance, not guaranteed headroom.
const REPAIR_HEADER_SLACK: usize = 64 * 1024;
/// What a rewritten piece adds around its samples: `moof`, `mfhd` and `mdat` headers.
const PIECE_HEADER_BYTES: usize = 32;
/// What each track adds to a piece: `traf`, `tfhd`, `tfdt`, and `trun` before its samples.
const TRACK_HEADER_BYTES: usize = 64;
/// What each sample adds to its `trun`: an explicit duration, size, flags and composition offset.
const SAMPLE_ENTRY_BYTES: usize = 16;
#[derive(Clone, Copy)]
struct Boundary {
    part: usize,
    sample: usize,
    seconds: f64,
    offset: u64,
}
#[async_trait(?Send)]
pub trait RepairSink {
    /// Commit a bounded immutable object before returning its storage key.
    async fn put(&self, bytes: Vec<u8>) -> Result<String>;
}

/// Index a recording, render its thumbnail sprite, and repair its playback
/// layout: `rolodex_core::process_recording`, with the repair's playback parts
/// in the output's metadata.
pub async fn process_recording_with_repairs<S: ByteSource + ?Sized>(
    source: &S,
    identity: SourceIdentity,
    options: ProcessorOptions,
    sink: &dyn RepairSink,
) -> Result<ProcessorOutput> {
    let mut index = index_recording(source, identity, options).await?;
    let thumbnail_jpeg = render_recording_thumbnail(source, &mut index).await;
    // Repairs share the budget already consumed by indexing and thumbnail reads.
    index.metadata.playback_parts = repair_recording(source, &mut index, sink).await?;
    Ok(index.into_output(thumbnail_jpeg))
}

/// Repair an indexed recording's playback layout, returning its playback parts:
/// none when no repair was made, with the reason in the index's metrics.
pub async fn repair_recording<S: ByteSource + ?Sized>(
    source: &S,
    index: &mut RecordingIndex,
    sink: &dyn RepairSink,
) -> Result<Vec<PlaybackPart>> {
    // Audio samples decode independently of one another, so an audio track can be
    // cut wherever the video is. No other kind of track makes that promise.
    let video_id = index.movie().video.track_id;
    let audio = TrackKind::Audio.as_str();
    if index
        .metadata
        .tracks
        .iter()
        .any(|track| track.id != video_id && track.kind != audio)
    {
        index.metrics.repair_skip_reason = Some(RepairSkipReason::UnsupportedTracks);
        return Ok(Vec::new());
    }
    let parts = &index.metadata.parts;
    let mut boundaries = Vec::new();
    let mut part_index = 0;
    let mut sample_index = 0;
    // Reuse the sample table from the existing scan; no second pass over R2 headers.
    for sample in index.video_samples() {
        while part_index + 1 < parts.len() && sample.location.offset >= parts[part_index + 1].offset
        {
            part_index += 1;
            sample_index = 0;
        }
        if sample.location.is_sync {
            boundaries.push(Boundary {
                part: part_index,
                sample: sample_index,
                seconds: sample.decode_start_ns as f64 / 1e9,
                offset: sample.location.offset,
            });
        }
        sample_index += 1;
    }
    let end = Boundary {
        part: parts.len(),
        sample: 0,
        seconds: index.metadata.recording.video_duration_ns as f64 / 1e9,
        offset: source.len(),
    };
    let cuts = match plan(&boundaries, end) {
        Ok(cuts) => cuts,
        Err(Error::Unsupported(_)) => {
            index.metrics.repair_skip_reason = Some(RepairSkipReason::UnsupportedGop);
            return Ok(Vec::new());
        }
        Err(e) => return Err(e),
    };
    if cuts.is_empty() {
        index.metrics.repair_skip_reason = Some(RepairSkipReason::NotNeeded);
        return Ok(Vec::new());
    }
    if cuts.keys().any(|i| parts[*i].length > MAX_REPAIR as u64) {
        index.metrics.repair_skip_reason = Some(RepairSkipReason::OversizedPart);
        return Ok(Vec::new());
    }
    if index
        .metrics
        .bytes_read
        .saturating_add(cuts.keys().map(|i| parts[*i].length).sum::<u64>())
        > index.options().limits.max_total_bytes_read
    {
        index.metrics.repair_skip_reason = Some(RepairSkipReason::ByteBudgetExhausted);
        return Ok(Vec::new());
    }
    let movie = index.movie();
    let defaults = movie
        .tracks
        .iter()
        .map(|t| (t.track_id, t.defaults))
        .collect::<Vec<_>>();
    let timescales = movie
        .tracks
        .iter()
        .map(|t| (t.track_id, t.timescale))
        .collect::<Vec<_>>();
    let scale = i128::from(timescale_of(&timescales, video_id)?);
    let mut result = Vec::new();
    for (i, part) in parts.iter().enumerate() {
        let Some(points) = cuts.get(&i) else {
            result.push(PlaybackPart {
                source: index.metadata.source.object_key.clone(),
                offset: part.offset,
                length: part.length,
                presentation_start_ns: part.presentation_start_ns,
                starts_with_sync: part.video_starts_with_sync,
            });
            continue;
        };
        let bytes = source.range(part.offset, part.length as usize).await?;
        index.metrics.bytes_read += part.length;
        index.metrics.range_requests += 1;
        let header_size = u32::from_be_bytes(
            bytes
                .get(..4)
                .ok_or(Error::Invalid("short repair part"))?
                .try_into()
                .map_err(|_| Error::Invalid("short repair header"))?,
        ) as usize;
        let parsed = parse_indexed_fragment(
            bytes
                .get(..header_size)
                .ok_or(Error::Invalid("invalid repair moof"))?,
            part.offset,
            &defaults,
        )?;
        let pieces = split(&parsed.tracks, &timescales, video_id, points)?;
        // One source buffer and one bounded output buffer; no recording-sized allocation.
        let samples = parsed.tracks.iter().flat_map(|track| &track.samples);
        let estimated = samples
            .map(|s| s.location.size as usize + SAMPLE_ENTRY_BYTES)
            .sum::<usize>()
            + pieces.len() * (PIECE_HEADER_BYTES + parsed.tracks.len() * TRACK_HEADER_BYTES);
        if estimated > MAX_REPAIR + REPAIR_HEADER_SLACK {
            return Err(Error::Limit("repair output exceeds cap"));
        }
        let mut output = Vec::with_capacity(estimated);
        let mut rewritten = Vec::new();
        for runs in &pieces {
            let start = output.len();
            let video = runs
                .iter()
                .find(|run| run.track_id == video_id)
                .ok_or(Error::Invalid("repair fragment has no video samples"))?;
            let presentation = video
                .samples
                .iter()
                .scan(video.decode_time as i128, |time, s| {
                    let pts = *time + s.composition_time_offset as i128;
                    *time += s.duration as i128;
                    Some(pts)
                })
                .min()
                .ok_or(Error::Invalid("empty repair fragment"))?;
            output.extend(build_moof(runs, parsed.sequence_number)?);
            let payload = runs.iter().map(TrackRun::payload).sum::<u64>();
            output.extend(
                u32::try_from(payload + 8)
                    .map_err(|_| Error::Limit("repair mdat too large"))?
                    .to_be_bytes(),
            );
            output.extend(b"mdat");
            for sample in runs.iter().flat_map(|run| run.samples) {
                let offset = sample
                    .location
                    .offset
                    .checked_sub(part.offset)
                    .ok_or(Error::Invalid("repair sample before part"))?
                    as usize;
                output.extend(
                    bytes
                        .get(offset..offset + sample.location.size as usize)
                        .ok_or(Error::Invalid("repair sample outside part"))?,
                );
            }
            if output.len() > MAX_REPAIR + REPAIR_HEADER_SLACK {
                return Err(Error::Limit("repair output exceeds cap"));
            }
            rewritten.push(PlaybackPart {
                source: String::new(),
                offset: start as u64,
                length: (output.len() - start) as u64,
                presentation_start_ns: i64::try_from(presentation * 1_000_000_000 / scale)
                    .map_err(|_| Error::Invalid("repair timestamp overflow"))?,
                starts_with_sync: video.samples[0].location.is_sync,
            });
        }
        drop(bytes);
        let key = sink.put(output).await?;
        for mut item in rewritten {
            item.source = key.clone();
            result.push(item);
        }
    }
    Ok(result)
}

/// One track's share of a rewritten fragment: a run of its source samples, the
/// first of them decoding at `decode_time`.
struct TrackRun<'a> {
    track_id: u32,
    decode_time: u64,
    samples: &'a [IndexedSample],
}

impl TrackRun<'_> {
    fn payload(&self) -> u64 {
        self.samples
            .iter()
            .map(|sample| u64::from(sample.location.size))
            .sum()
    }
}

fn timescale_of(timescales: &[(u32, u32)], track_id: u32) -> Result<u32> {
    timescales
        .iter()
        .find(|(candidate, _)| *candidate == track_id)
        .map(|(_, timescale)| *timescale)
        .ok_or(Error::Invalid(
            "repair fragment references an unknown track",
        ))
}

/// Splits a fragment's tracks into pieces at the video samples `points`: each
/// piece after the first starts on one of them, and takes from every other
/// track the samples decoding from that instant until the next piece's. A piece
/// leaves out a track with no sample in its span.
fn split<'a>(
    tracks: &'a [IndexedTrackFragment],
    timescales: &[(u32, u32)],
    video_id: u32,
    points: &[usize],
) -> Result<Vec<Vec<TrackRun<'a>>>> {
    let video = tracks
        .iter()
        .find(|track| track.track_id == video_id)
        .ok_or(Error::Invalid("repair part has no video samples"))?;
    let video_scale = timescale_of(timescales, video_id)?;
    // When each piece after the first starts, on the video track's decode timeline.
    let mut cuts = Vec::with_capacity(points.len());
    let mut time = video.base_decode_time;
    let mut next = 0;
    for &point in points {
        let before = video
            .samples
            .get(next..point)
            .filter(|samples| !samples.is_empty() && point < video.samples.len())
            .ok_or(Error::Invalid("repair cut is outside its part"))?;
        for sample in before {
            time = advance(time, sample)?;
        }
        cuts.push(time);
        next = point;
    }
    // Where each piece starts in each track: its first sample there, and when that sample decodes.
    let mut starts = Vec::with_capacity(tracks.len());
    for track in tracks {
        let track_scale = timescale_of(timescales, track.track_id)?;
        let mut track_starts = Vec::with_capacity(cuts.len() + 1);
        let mut sample = 0;
        let mut time = track.base_decode_time;
        track_starts.push((sample, time));
        for &cut in &cuts {
            // The first sample decoding at or after the cut, comparing across the two timescales.
            while sample < track.samples.len()
                && u128::from(time) * u128::from(video_scale)
                    < u128::from(cut) * u128::from(track_scale)
            {
                time = advance(time, &track.samples[sample])?;
                sample += 1;
            }
            track_starts.push((sample, time));
        }
        starts.push(track_starts);
    }
    Ok((0..=cuts.len())
        .map(|piece| {
            tracks
                .iter()
                .zip(&starts)
                .filter_map(|(track, starts)| {
                    let (begin, decode_time) = starts[piece];
                    let end = starts
                        .get(piece + 1)
                        .map_or(track.samples.len(), |(sample, _)| *sample);
                    (begin < end).then(|| TrackRun {
                        track_id: track.track_id,
                        decode_time,
                        samples: &track.samples[begin..end],
                    })
                })
                .collect()
        })
        .collect())
}

/// The decode time after `sample`, which decodes at `time`.
fn advance(time: u64, sample: &IndexedSample) -> Result<u64> {
    time.checked_add(u64::from(sample.duration))
        .ok_or(Error::Invalid("repair decode time overflow"))
}

fn plan(boundaries: &[Boundary], end: Boundary) -> Result<BTreeMap<usize, Vec<usize>>> {
    let mut anchor = *boundaries
        .first()
        .ok_or(Error::Invalid("no random access samples"))?;
    let mut previous = anchor;
    let mut cuts = BTreeMap::<usize, Vec<usize>>::new();
    for next in boundaries
        .iter()
        .skip(1)
        .copied()
        .chain(std::iter::once(end))
    {
        if next.seconds - anchor.seconds > MAX_SECONDS || next.offset - anchor.offset > MAX_BYTES {
            if previous.offset == anchor.offset {
                return Err(Error::Unsupported("GOP itself exceeds repair threshold"));
            }
            if previous.sample != 0 {
                cuts.entry(previous.part).or_default().push(previous.sample);
            }
            anchor = previous;
            if next.seconds - anchor.seconds > MAX_SECONDS
                || next.offset - anchor.offset > MAX_BYTES
            {
                return Err(Error::Unsupported("GOP itself exceeds repair threshold"));
            }
        }
        // Existing independently decodable fragments remain independent in the resulting index.
        if next.sample == 0 {
            anchor = next;
        }
        previous = next;
    }
    Ok(cuts)
}

fn atom(kind: &[u8; 4], payload: Vec<u8>) -> Vec<u8> {
    let mut out = Vec::with_capacity(payload.len() + 8);
    out.extend_from_slice(&((payload.len() + 8) as u32).to_be_bytes());
    out.extend_from_slice(kind);
    out.extend_from_slice(&payload);
    out
}
fn full(kind: &[u8; 4], flags: u32, payload: Vec<u8>) -> Vec<u8> {
    let mut body = flags.to_be_bytes().to_vec();
    body.extend(payload);
    atom(kind, body)
}
/// A `moof` for one piece: a `traf` for each run, its samples addressed in the
/// `mdat` that follows the `moof`, where the runs' payloads lie one after another.
fn build_moof(runs: &[TrackRun<'_>], sequence: u32) -> Result<Vec<u8>> {
    let build = |first_data_offset: u64| -> Result<Vec<u8>> {
        let mut body = full(b"mfhd", 0, sequence.to_be_bytes().to_vec());
        let mut data_offset = first_data_offset;
        for run in runs {
            let mut trun = (run.samples.len() as u32).to_be_bytes().to_vec();
            trun.extend(
                i32::try_from(data_offset)
                    .map_err(|_| Error::Limit("fragment header too large"))?
                    .to_be_bytes(),
            );
            for sample in run.samples {
                trun.extend(sample.duration.to_be_bytes());
                trun.extend(sample.location.size.to_be_bytes());
                let flags: u32 = if sample.location.is_sync {
                    0x02000000
                } else {
                    0x01010000
                };
                trun.extend(flags.to_be_bytes());
                trun.extend(
                    i32::try_from(sample.composition_time_offset)
                        .map_err(|_| Error::Unsupported("composition offset exceeds i32"))?
                        .to_be_bytes(),
                );
            }
            let mut traf = full(b"tfhd", 0x020000, run.track_id.to_be_bytes().to_vec());
            traf.extend(full(
                b"tfdt",
                0x01000000,
                run.decode_time.to_be_bytes().to_vec(),
            ));
            traf.extend(full(b"trun", 0x01000f01, trun));
            body.extend(atom(b"traf", traf));
            data_offset += run.payload();
        }
        Ok(atom(b"moof", body))
    };
    let size = build(0)?.len();
    build(size as u64 + 8)
}
#[cfg(test)]
mod tests {
    use super::*;
    use rolodex_core::mp4::{SampleDefaults, SampleLocation};

    fn boundary(part: usize, sample: usize, seconds: f64, offset: u64) -> Boundary {
        Boundary {
            part,
            sample,
            seconds,
            offset,
        }
    }
    #[test]
    fn byte_threshold_can_cut_before_duration_threshold() {
        let candidates = [
            boundary(0, 0, 0.0, 0),
            boundary(1, 2, 2.0, 2_000_000),
            boundary(2, 2, 4.0, 3_000_000),
        ];
        let cuts = plan(&candidates, boundary(3, 0, 5.0, 5_000_000)).unwrap();
        assert_eq!(cuts, BTreeMap::from([(2, vec![2])]));
    }
    #[test]
    fn duration_threshold_uses_last_keyframe_before_limit() {
        let candidates = [
            boundary(0, 0, 0.0, 0),
            boundary(1, 2, 4.0, 100),
            boundary(2, 2, 6.0, 200),
        ];
        let cuts = plan(&candidates, boundary(3, 0, 8.0, 300)).unwrap();
        assert_eq!(cuts, BTreeMap::from([(2, vec![2])]));
    }
    #[test]
    fn oversized_gop_is_rejected_even_at_existing_part_boundary() {
        let candidates = [boundary(0, 0, 0.0, 0), boundary(1, 2, 2.0, 100)];
        assert!(plan(&candidates, boundary(2, 0, 10.0, 200)).is_err());
    }

    fn sample(offset: u64, size: u32, duration: u32, is_sync: bool) -> IndexedSample {
        IndexedSample {
            location: SampleLocation {
                offset,
                size,
                is_sync,
            },
            duration,
            composition_time_offset: 0,
        }
    }

    /// Four frames at 90 kHz from 10 s, the third a keyframe.
    fn video_fragment() -> IndexedTrackFragment {
        IndexedTrackFragment {
            track_id: 1,
            base_decode_time: 900_000,
            samples: vec![
                sample(1000, 40, 3000, false),
                sample(1040, 30, 3000, false),
                sample(1070, 90, 3000, true),
                sample(1160, 20, 3000, false),
            ],
        }
    }

    /// `frames` AAC frames at 48 kHz from 10 s.
    fn audio_fragment(frames: u64) -> IndexedTrackFragment {
        IndexedTrackFragment {
            track_id: 2,
            base_decode_time: 480_000,
            samples: (0..frames)
                .map(|frame| sample(2000 + frame * 10, 10, 1024, true))
                .collect(),
        }
    }

    const TIMESCALES: [(u32, u32); 2] = [(1, 90_000), (2, 48_000)];

    #[test]
    fn audio_is_cut_where_the_video_keyframe_decodes() {
        let tracks = [video_fragment(), audio_fragment(6)];
        let pieces = split(&tracks, &TIMESCALES, 1, &[2]).unwrap();
        // The keyframe decodes at 906_000 / 90_000 = 10.0667 s. The fourth audio frame decodes before it, at
        // 483_072 / 48_000 = 10.064 s, and the fifth after it, at 10.0853 s.
        let runs = pieces
            .iter()
            .map(|piece| {
                piece
                    .iter()
                    .map(|run| (run.track_id, run.decode_time, run.samples.len()))
                    .collect::<Vec<_>>()
            })
            .collect::<Vec<_>>();
        assert_eq!(
            runs,
            vec![
                vec![(1, 900_000, 2), (2, 480_000, 4)],
                vec![(1, 906_000, 2), (2, 484_096, 2)],
            ]
        );
        assert!(pieces[1][0].samples[0].location.is_sync);
    }

    #[test]
    fn a_piece_leaves_out_a_track_with_no_sample_in_its_span() {
        // Three audio frames end at 10.064 s, before the keyframe decodes.
        let tracks = [video_fragment(), audio_fragment(3)];
        let pieces = split(&tracks, &TIMESCALES, 1, &[2]).unwrap();
        let tracks_in =
            |piece: &[TrackRun<'_>]| piece.iter().map(|run| run.track_id).collect::<Vec<_>>();
        assert_eq!(tracks_in(&pieces[0]), vec![1, 2]);
        assert_eq!(tracks_in(&pieces[1]), vec![1]);
    }

    #[test]
    fn a_cut_outside_the_part_is_refused() {
        let tracks = [video_fragment()];
        assert!(split(&tracks, &TIMESCALES, 1, &[4]).is_err());
        assert!(split(&tracks, &TIMESCALES, 1, &[0]).is_err());
        assert!(split(&tracks, &TIMESCALES, 7, &[2]).is_err());
    }

    #[test]
    fn fragment_with_two_tracks_addresses_each_run_in_the_shared_mdat() {
        let tracks = [video_fragment(), audio_fragment(6)];
        let pieces = split(&tracks, &TIMESCALES, 1, &[2]).unwrap();
        let bytes = build_moof(&pieces[1], 12).unwrap();
        let defaults = [
            (1, SampleDefaults::default()),
            (2, SampleDefaults::default()),
        ];
        let parsed = parse_indexed_fragment(&bytes, 0, &defaults).unwrap();
        let [video, audio] = parsed.tracks.as_slice() else {
            panic!("expected a traf for each track");
        };
        assert_eq!((video.track_id, video.base_decode_time), (1, 906_000));
        assert_eq!((audio.track_id, audio.base_decode_time), (2, 484_096));
        // The mdat follows the moof: the video's 90 and 20 bytes, then the audio's two frames of 10.
        let payload = bytes.len() as u64 + 8;
        let offsets = |track: &IndexedTrackFragment| {
            track
                .samples
                .iter()
                .map(|sample| sample.location.offset - payload)
                .collect::<Vec<_>>()
        };
        assert_eq!(offsets(video), vec![0, 90]);
        assert_eq!(offsets(audio), vec![110, 120]);
        assert!(video.samples[0].location.is_sync && !video.samples[1].location.is_sync);
        assert!(audio.samples.iter().all(|sample| sample.location.is_sync));
        assert!(audio.samples.iter().all(|sample| sample.duration == 1024));
    }

    #[test]
    fn rewritten_fragment_preserves_sample_timing_and_random_access() {
        let track = IndexedTrackFragment {
            track_id: 7,
            base_decode_time: 900_000,
            samples: vec![
                IndexedSample {
                    location: SampleLocation {
                        offset: 1000,
                        size: 20,
                        is_sync: false,
                    },
                    duration: 3000,
                    composition_time_offset: 0,
                },
                IndexedSample {
                    location: SampleLocation {
                        offset: 1020,
                        size: 40,
                        is_sync: true,
                    },
                    duration: 3000,
                    composition_time_offset: 6000,
                },
                IndexedSample {
                    location: SampleLocation {
                        offset: 1060,
                        size: 30,
                        is_sync: false,
                    },
                    duration: 3000,
                    composition_time_offset: -3000,
                },
            ],
        };
        let run = TrackRun {
            track_id: 7,
            decode_time: 903_000,
            samples: &track.samples[1..],
        };
        let bytes = build_moof(&[run], 12).unwrap();
        let parsed = parse_indexed_fragment(&bytes, 0, &[(7, SampleDefaults::default())]).unwrap();
        let output = &parsed.tracks[0];
        assert_eq!(output.track_id, 7);
        assert_eq!(output.base_decode_time, 903_000);
        assert_eq!(output.samples.len(), 2);
        assert_eq!(output.samples[0].location.offset, bytes.len() as u64 + 8);
        assert_eq!(
            output.samples[1].location.offset,
            bytes.len() as u64 + 8 + 40
        );
        for (expected, actual) in track.samples[1..].iter().zip(&output.samples) {
            assert_eq!(expected.duration, actual.duration);
            assert_eq!(
                expected.composition_time_offset,
                actual.composition_time_offset
            );
            assert_eq!(expected.location.size, actual.location.size);
            assert_eq!(expected.location.is_sync, actual.location.is_sync);
        }
    }
}
