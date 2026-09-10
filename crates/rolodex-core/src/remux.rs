//! Bounded, lossless repairs of oversized independently decodable ranges.
pub use crate::processor::PlaybackPart;
use crate::processor::RepairSkipReason;
use crate::{
    ByteSource, Error, RecordingIndex, Result,
    mp4::{IndexedTrackFragment, parse_indexed_fragment},
};
use async_trait::async_trait;
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

pub async fn repair_recording<S: ByteSource + ?Sized>(
    source: &S,
    index: &mut RecordingIndex,
    sink: &dyn RepairSink,
) -> Result<Vec<PlaybackPart>> {
    if index.metadata.tracks.len() != 1 || index.metadata.tracks[0].codec != "avc1" {
        index.metrics.repair_skip_reason = Some(RepairSkipReason::UnsupportedTracks);
        return Ok(Vec::new());
    }
    let parts = &index.metadata.parts;
    let mut boundaries = Vec::new();
    let mut part_index = 0;
    let mut sample_index = 0;
    // Reuse the sample table from the existing scan; no second pass over R2 headers.
    for sample in &index.video_samples {
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
        > index.options.limits.max_total_bytes_read
    {
        index.metrics.repair_skip_reason = Some(RepairSkipReason::ByteBudgetExhausted);
        return Ok(Vec::new());
    }
    let defaults = index
        .movie
        .tracks
        .iter()
        .map(|t| (t.track_id, t.defaults))
        .collect::<Vec<_>>();
    let scale = index.metadata.tracks[0].timescale as i128;
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
        let track = &parsed.tracks[0];
        let mut begin = 0;
        let mut dts = track.base_decode_time;
        // One source buffer and one bounded output buffer; no recording-sized allocation.
        let estimated = track
            .samples
            .iter()
            .map(|s| s.location.size as usize + 16)
            .sum::<usize>()
            + (points.len() + 1) * 128;
        if estimated > MAX_REPAIR + REPAIR_HEADER_SLACK {
            return Err(Error::Limit("repair output exceeds cap"));
        }
        let mut output = Vec::with_capacity(estimated);
        let mut rewritten = Vec::new();
        for finish in points
            .iter()
            .copied()
            .chain(std::iter::once(track.samples.len()))
        {
            let start = output.len();
            let presentation = track.samples[begin..finish]
                .iter()
                .scan(dts as i128, |time, s| {
                    let pts = *time + s.composition_time_offset as i128;
                    *time += s.duration as i128;
                    Some(pts)
                })
                .min()
                .ok_or(Error::Invalid("empty repair fragment"))?;
            output.extend(build_moof(
                track,
                begin,
                finish,
                dts,
                parsed.sequence_number,
            )?);
            let payload = track.samples[begin..finish]
                .iter()
                .map(|s| s.location.size as u64)
                .sum::<u64>();
            output.extend(
                u32::try_from(payload + 8)
                    .map_err(|_| Error::Limit("repair mdat too large"))?
                    .to_be_bytes(),
            );
            output.extend(b"mdat");
            for sample in &track.samples[begin..finish] {
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
                dts += sample.duration as u64;
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
                starts_with_sync: track.samples[begin].location.is_sync,
            });
            begin = finish;
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
fn build_moof(
    track: &IndexedTrackFragment,
    begin: usize,
    end: usize,
    dts: u64,
    sequence: u32,
) -> Result<Vec<u8>> {
    let samples = &track.samples[begin..end];
    let build = |offset: i32| -> Result<Vec<u8>> {
        let mut trun = (samples.len() as u32).to_be_bytes().to_vec();
        trun.extend(offset.to_be_bytes());
        for sample in samples {
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
        let mut traf = full(b"tfhd", 0x020000, track.track_id.to_be_bytes().to_vec());
        traf.extend(full(b"tfdt", 0x01000000, dts.to_be_bytes().to_vec()));
        traf.extend(full(b"trun", 0x01000f01, trun));
        let mut body = full(b"mfhd", 0, sequence.to_be_bytes().to_vec());
        body.extend(atom(b"traf", traf));
        Ok(atom(b"moof", body))
    };
    let size = build(0)?.len();
    build(i32::try_from(size + 8).map_err(|_| Error::Limit("fragment header too large"))?)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::mp4::{IndexedSample, SampleDefaults, SampleLocation};

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
        let bytes = build_moof(&track, 1, 3, 903_000, 12).unwrap();
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
