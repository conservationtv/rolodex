use crate::{Error, Result};

use super::boxes::{box_at, child, children, read_i32, read_u24, read_u32, read_u64};

const MAX_SAMPLES_PER_FRAGMENT: usize = 100_000;

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct SampleDefaults {
    pub duration: u32,
    pub size: u32,
    pub flags: u32,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SampleLocation {
    pub offset: u64,
    pub size: u32,
    pub is_sync: bool,
}

#[derive(Debug, Clone, Copy)]
struct Tfhd {
    base_data_offset: Option<u64>,
    default_base_is_moof: bool,
    defaults: SampleDefaults,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct IndexedSample {
    pub location: SampleLocation,
    pub duration: u32,
    pub composition_time_offset: i64,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct IndexedTrackFragment {
    pub track_id: u32,
    pub base_decode_time: u64,
    pub samples: Vec<IndexedSample>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct IndexedFragment {
    pub sequence_number: u32,
    pub tracks: Vec<IndexedTrackFragment>,
}

struct Fields<'a> {
    payload: &'a [u8],
    cursor: usize,
}

impl<'a> Fields<'a> {
    fn new(payload: &'a [u8], cursor: usize) -> Self {
        Self { payload, cursor }
    }

    fn u32_if(&mut self, present: bool) -> Result<Option<u32>> {
        if !present {
            return Ok(None);
        }
        let value = read_u32(self.payload, self.cursor)?;
        self.cursor += 4;
        Ok(Some(value))
    }

    fn i32_if(&mut self, present: bool) -> Result<Option<i32>> {
        if !present {
            return Ok(None);
        }
        let value = read_i32(self.payload, self.cursor)?;
        self.cursor += 4;
        Ok(Some(value))
    }

    fn u64_if(&mut self, present: bool) -> Result<Option<u64>> {
        if !present {
            return Ok(None);
        }
        let value = read_u64(self.payload, self.cursor)?;
        self.cursor += 8;
        Ok(Some(value))
    }
}

fn parse_tfhd(payload: &[u8], trex: SampleDefaults) -> Result<(u32, Tfhd)> {
    if payload.len() < 8 {
        return Err(Error::Invalid("truncated tfhd"));
    }
    if payload[0] != 0 {
        return Err(Error::Unsupported("unknown tfhd version"));
    }
    let flags = read_u24(payload, 1)?;
    let track_id = read_u32(payload, 4)?;
    if track_id == 0 {
        return Err(Error::Invalid("tfhd track ID is zero"));
    }
    let mut fields = Fields::new(payload, 8);
    let base_data_offset = fields.u64_if(flags & 0x000001 != 0)?;
    let _sample_description_index = fields.u32_if(flags & 0x000002 != 0)?;
    let duration = fields
        .u32_if(flags & 0x000008 != 0)?
        .unwrap_or(trex.duration);
    let size = fields.u32_if(flags & 0x000010 != 0)?.unwrap_or(trex.size);
    let sample_flags = fields.u32_if(flags & 0x000020 != 0)?.unwrap_or(trex.flags);
    Ok((
        track_id,
        Tfhd {
            base_data_offset,
            default_base_is_moof: flags & 0x020000 != 0,
            defaults: SampleDefaults {
                duration,
                size,
                flags: sample_flags,
            },
        },
    ))
}

pub fn parse_indexed_fragment(
    data: &[u8],
    moof_absolute_offset: u64,
    track_defaults: &[(u32, SampleDefaults)],
) -> Result<IndexedFragment> {
    let moof = box_at(data, 0, data.len())?;
    if moof.kind != *b"moof" || moof.end() != data.len() {
        return Err(Error::Invalid("buffer is not exactly one moof box"));
    }
    let mfhd = child(data, moof, b"mfhd")?.ok_or(Error::Invalid("moof missing mfhd"))?;
    let mfhd_payload = mfhd.payload(data)?;
    if mfhd_payload.len() < 8 || mfhd_payload[0] != 0 {
        return Err(Error::Invalid("invalid mfhd"));
    }
    let sequence_number = read_u32(mfhd_payload, 4)?;

    let mut tracks = Vec::new();
    for traf in children(data, moof)? {
        let traf = traf?;
        if traf.kind != *b"traf" {
            continue;
        }
        let tfhd_box = child(data, traf, b"tfhd")?.ok_or(Error::Invalid("traf missing tfhd"))?;
        let tfhd_payload = tfhd_box.payload(data)?;
        let track_id = read_u32(tfhd_payload, 4)?;
        let trex = track_defaults
            .iter()
            .find(|(candidate, _)| *candidate == track_id)
            .map(|(_, value)| *value)
            .ok_or(Error::Invalid("fragment references an unknown track"))?;
        let (parsed_track_id, tfhd) = parse_tfhd(tfhd_payload, trex)?;
        debug_assert_eq!(parsed_track_id, track_id);
        if tfhd.base_data_offset.is_some() || !tfhd.default_base_is_moof {
            return Err(Error::Unsupported(
                "fragment does not use default-base-is-moof relative addressing",
            ));
        }
        if tracks
            .iter()
            .any(|track: &IndexedTrackFragment| track.track_id == track_id)
        {
            return Err(Error::Unsupported("multiple traf boxes for one track"));
        }

        let tfdt = child(data, traf, b"tfdt")?.ok_or(Error::Invalid("traf missing tfdt"))?;
        let base_decode_time = parse_tfdt(tfdt.payload(data)?)?;
        let mut next_data_offset = None;
        let mut samples = Vec::new();
        for trun in children(data, traf)? {
            let trun = trun?;
            if trun.kind == *b"trun" {
                parse_trun(
                    trun.payload(data)?,
                    moof_absolute_offset,
                    &mut next_data_offset,
                    tfhd.defaults,
                    &mut samples,
                )?;
            }
        }
        if samples.is_empty() {
            return Err(Error::Invalid("traf contains no samples"));
        }
        if samples.iter().any(|sample| sample.duration == 0) {
            return Err(Error::Invalid("sample has no resolved duration"));
        }
        tracks.push(IndexedTrackFragment {
            track_id,
            base_decode_time,
            samples,
        });
    }
    if tracks.is_empty() {
        return Err(Error::Invalid("moof contains no track fragments"));
    }
    Ok(IndexedFragment {
        sequence_number,
        tracks,
    })
}

fn parse_tfdt(payload: &[u8]) -> Result<u64> {
    let version = *payload.first().ok_or(Error::Invalid("empty tfdt"))?;
    match version {
        0 => Ok(u64::from(read_u32(payload, 4)?)),
        1 => read_u64(payload, 4),
        _ => Err(Error::Unsupported("unknown tfdt version")),
    }
}

fn parse_trun(
    payload: &[u8],
    base_data_offset: u64,
    next_data_offset: &mut Option<u64>,
    defaults: SampleDefaults,
    output: &mut Vec<IndexedSample>,
) -> Result<()> {
    if payload.len() < 8 {
        return Err(Error::Invalid("truncated trun"));
    }
    let version = payload[0];
    if version > 1 {
        return Err(Error::Unsupported("unknown trun version"));
    }
    let flags = read_u24(payload, 1)?;
    let sample_count = read_u32(payload, 4)? as usize;
    let total_samples = output
        .len()
        .checked_add(sample_count)
        .ok_or(Error::Limit("sample count overflow"))?;
    if total_samples > MAX_SAMPLES_PER_FRAGMENT {
        return Err(Error::Limit("too many samples in fragment"));
    }
    let mut fields = Fields::new(payload, 8);
    let data_offset = fields.i32_if(flags & 0x000001 != 0)?;
    let first_sample_flags = fields.u32_if(flags & 0x000004 != 0)?;
    let mut sample_offset = if let Some(relative) = data_offset {
        checked_add_signed(base_data_offset, relative)?
    } else {
        next_data_offset.ok_or(Error::Unsupported(
            "first trun needs data_offset for relative addressing",
        ))?
    };

    for index in 0..sample_count {
        let duration = fields
            .u32_if(flags & 0x000100 != 0)?
            .unwrap_or(defaults.duration);
        let size = fields
            .u32_if(flags & 0x000200 != 0)?
            .unwrap_or(defaults.size);
        let sample_flags = if let Some(value) = fields.u32_if(flags & 0x000400 != 0)? {
            value
        } else if index == 0 {
            first_sample_flags.unwrap_or(defaults.flags)
        } else {
            defaults.flags
        };
        let composition_time_offset = fields.u32_if(flags & 0x000800 != 0)?.map_or(0, |raw| {
            if version == 0 {
                i64::from(raw)
            } else {
                i64::from(raw as i32)
            }
        });
        if size == 0 {
            return Err(Error::Invalid("sample has no resolved size"));
        }
        let location = SampleLocation {
            offset: sample_offset,
            size,
            is_sync: sample_is_sync(sample_flags),
        };
        output.push(IndexedSample {
            location,
            duration,
            composition_time_offset,
        });
        sample_offset = sample_offset
            .checked_add(u64::from(size))
            .ok_or(Error::Invalid("sample data offset overflow"))?;
    }
    *next_data_offset = Some(sample_offset);
    Ok(())
}

fn sample_is_sync(sample_flags: u32) -> bool {
    let sample_depends_on = (sample_flags >> 24) & 0x03;
    sample_flags & 0x0001_0000 == 0 && sample_depends_on != 1
}

fn checked_add_signed(base: u64, relative: i32) -> Result<u64> {
    if relative >= 0 {
        base.checked_add(relative as u64)
            .ok_or(Error::Invalid("trun data offset overflow"))
    } else {
        base.checked_sub(u64::from(relative.unsigned_abs()))
            .ok_or(Error::Invalid("negative trun data offset before object"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn signed_offsets_are_checked() {
        assert_eq!(checked_add_signed(100, -20).unwrap(), 80);
        assert!(checked_add_signed(10, -20).is_err());
    }

    #[test]
    fn explicit_tfhd_base_supports_trun_without_data_offset() {
        let payload = [
            0, 0, 2, 0, // version + sample-size-present flag
            0, 0, 0, 1, // sample count
            0, 0, 0, 25, // sample size
        ];
        let mut next = Some(1_000);
        let mut samples = Vec::new();
        parse_trun(
            &payload,
            1_000,
            &mut next,
            SampleDefaults::default(),
            &mut samples,
        )
        .unwrap();
        assert_eq!(samples[0].location.offset, 1_000);
        assert_eq!(samples[0].location.size, 25);
        assert_eq!(samples[0].duration, 0);
        assert_eq!(next, Some(1_025));
    }

    #[test]
    fn shared_trun_parser_preserves_signed_composition_offsets() {
        let payload = [
            1, 0, 11, 1, // version + data-offset/duration/size/cto flags
            0, 0, 0, 1, // sample count
            0, 0, 0, 32, // data offset
            0, 0, 0, 10, // duration
            0, 0, 0, 25, // size
            0xff, 0xff, 0xff, 0xfb, // composition offset -5
        ];
        let mut next = None;
        let mut samples = Vec::new();

        parse_trun(
            &payload,
            100,
            &mut next,
            SampleDefaults::default(),
            &mut samples,
        )
        .unwrap();

        assert_eq!(samples[0].location.offset, 132);
        assert_eq!(samples[0].duration, 10);
        assert_eq!(samples[0].composition_time_offset, -5);
    }

    #[test]
    fn rejects_huge_sample_count_before_allocation() {
        let payload = [0, 0, 0, 0, 0xff, 0xff, 0xff, 0xff];
        let mut next_data = Some(0);
        let mut samples = Vec::new();
        let error = parse_trun(
            &payload,
            0,
            &mut next_data,
            SampleDefaults::default(),
            &mut samples,
        )
        .unwrap_err();
        assert!(matches!(error, Error::Limit(_)));
        assert!(samples.is_empty());
    }
}
