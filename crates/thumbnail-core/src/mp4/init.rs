use crate::{
    Error, Result,
    codec::{CodecConfig, VideoCodec},
};

use super::{
    boxes::{BoxView, box_at, child, children, children_between, read_u16, read_u32},
    fragment::SampleDefaults,
};

const MAX_PARAMETER_SETS: usize = 64;
const MAX_PARAMETER_SET_BYTES: usize = 1024 * 1024;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VideoTrack {
    pub track_id: u32,
    pub config: CodecConfig,
    pub defaults: SampleDefaults,
}

pub fn parse_moov(data: &[u8]) -> Result<VideoTrack> {
    let moov = box_at(data, 0, data.len())?;
    if moov.kind != *b"moov" || moov.end() != data.len() {
        return Err(Error::Invalid("buffer is not exactly one moov box"));
    }

    let mut trex_defaults = Vec::new();
    let mut tracks = Vec::new();
    for view in children(data, moov)? {
        let view = view?;
        match &view.kind {
            b"mvex" => {
                for trex in children(data, view)? {
                    let trex = trex?;
                    if trex.kind == *b"trex" {
                        trex_defaults.push(parse_trex(trex.payload(data)?)?);
                    }
                }
            }
            b"trak" => tracks.push(view),
            _ => {}
        }
    }

    let mut selected = None;
    for trak in tracks {
        let Some((track_id, config)) = parse_trak(data, trak)? else {
            continue;
        };
        if selected.is_some() {
            return Err(Error::Unsupported("more than one video track"));
        }
        let defaults = trex_defaults
            .iter()
            .find(|(id, _)| *id == track_id)
            .map(|(_, defaults)| *defaults)
            .unwrap_or_default();
        selected = Some(VideoTrack {
            track_id,
            config,
            defaults,
        });
    }
    selected.ok_or(Error::Unsupported("moov contains no supported video track"))
}

fn parse_trak(data: &[u8], trak: BoxView) -> Result<Option<(u32, CodecConfig)>> {
    let mut tkhd = None;
    let mut mdia = None;
    for view in children(data, trak)? {
        let view = view?;
        match &view.kind {
            b"tkhd" if tkhd.is_none() => tkhd = Some(view),
            b"mdia" if mdia.is_none() => mdia = Some(view),
            _ => {}
        }
    }
    let tkhd = tkhd.ok_or(Error::Invalid("trak missing tkhd"))?;
    let mdia = mdia.ok_or(Error::Invalid("trak missing mdia"))?;

    let mut hdlr = None;
    let mut minf = None;
    for view in children(data, mdia)? {
        let view = view?;
        match &view.kind {
            b"hdlr" if hdlr.is_none() => hdlr = Some(view),
            b"minf" if minf.is_none() => minf = Some(view),
            _ => {}
        }
    }
    let hdlr = hdlr.ok_or(Error::Invalid("mdia missing hdlr"))?;
    let hdlr_payload = hdlr.payload(data)?;
    if hdlr_payload.get(8..12) != Some(b"vide") {
        return Ok(None);
    }

    let track_id = parse_track_id(tkhd.payload(data)?)?;
    let minf = minf.ok_or(Error::Invalid("video mdia missing minf"))?;
    let stbl = child(data, minf, b"stbl")?.ok_or(Error::Invalid("video minf missing stbl"))?;
    let stsd = child(data, stbl, b"stsd")?.ok_or(Error::Invalid("video stbl missing stsd"))?;
    let config = parse_stsd(data, stsd)?;
    Ok(Some((track_id, config)))
}

pub(crate) fn parse_track_id(payload: &[u8]) -> Result<u32> {
    let version = *payload.first().ok_or(Error::Invalid("empty tkhd"))?;
    let offset = match version {
        0 => 12,
        1 => 20,
        _ => return Err(Error::Unsupported("unknown tkhd version")),
    };
    let track_id = read_u32(payload, offset)?;
    if track_id == 0 {
        return Err(Error::Invalid("tkhd track ID is zero"));
    }
    Ok(track_id)
}

pub(crate) fn parse_trex(payload: &[u8]) -> Result<(u32, SampleDefaults)> {
    if payload.len() < 24 {
        return Err(Error::Invalid("truncated trex"));
    }
    let track_id = read_u32(payload, 4)?;
    Ok((
        track_id,
        SampleDefaults {
            duration: read_u32(payload, 12)?,
            size: read_u32(payload, 16)?,
            flags: read_u32(payload, 20)?,
        },
    ))
}

fn parse_stsd(data: &[u8], stsd: BoxView) -> Result<CodecConfig> {
    let payload = stsd.payload(data)?;
    if payload.len() < 8 {
        return Err(Error::Invalid("truncated stsd"));
    }
    let entry_count = read_u32(payload, 4)? as usize;
    if entry_count == 0 || entry_count > 64 {
        return Err(Error::Invalid("invalid stsd entry count"));
    }
    let mut entries = children_between(data, stsd.payload_start() + 8, stsd.end())?;
    for _ in 0..entry_count {
        let entry = entries
            .next()
            .transpose()?
            .ok_or(Error::Invalid("stsd entry count exceeds entries"))?;
        let codec = match &entry.kind {
            b"avc1" | b"avc3" => VideoCodec::H264,
            b"hvc1" | b"hev1" => VideoCodec::H265,
            _ => continue,
        };
        let child_start = entry
            .payload_start()
            .checked_add(78)
            .ok_or(Error::Invalid("sample entry child offset overflow"))?;
        if child_start > entry.end() {
            return Err(Error::Invalid("truncated visual sample entry"));
        }
        let config_kind = match codec {
            VideoCodec::H264 => b"avcC",
            VideoCodec::H265 => b"hvcC",
        };
        let mut config_box = None;
        for view in children_between(data, child_start, entry.end())? {
            let view = view?;
            if &view.kind == config_kind {
                config_box = Some(view);
                break;
            }
        }
        let config_box = config_box.ok_or(Error::Invalid(
            "video sample entry missing codec configuration",
        ))?;
        return match codec {
            VideoCodec::H264 => parse_avcc(config_box.payload(data)?),
            VideoCodec::H265 => parse_hvcc(config_box.payload(data)?),
        };
    }
    Err(Error::Unsupported(
        "video stsd has no AVC or HEVC sample entry",
    ))
}

pub fn parse_avcc(data: &[u8]) -> Result<CodecConfig> {
    if data.len() < 7 || data[0] != 1 {
        return Err(Error::Invalid("invalid avcC record"));
    }
    let nal_length_size = usize::from((data[4] & 0x03) + 1);
    let sps_count = usize::from(data[5] & 0x1f);
    let mut cursor = 6usize;
    let mut parameter_sets = Vec::new();
    read_parameter_sets(data, &mut cursor, sps_count, &mut parameter_sets)?;
    let pps_count = usize::from(
        *data
            .get(cursor)
            .ok_or(Error::Invalid("avcC missing PPS count"))?,
    );
    cursor += 1;
    read_parameter_sets(data, &mut cursor, pps_count, &mut parameter_sets)?;
    if sps_count == 0 || pps_count == 0 {
        return Err(Error::Unsupported(
            "avcC requires SPS and PPS for random access",
        ));
    }
    Ok(CodecConfig {
        codec: VideoCodec::H264,
        nal_length_size,
        parameter_sets,
    })
}

pub fn parse_hvcc(data: &[u8]) -> Result<CodecConfig> {
    if data.len() < 23 || data[0] != 1 {
        return Err(Error::Invalid("invalid hvcC record"));
    }
    let nal_length_size = usize::from((data[21] & 0x03) + 1);
    let array_count = usize::from(data[22]);
    let mut cursor = 23usize;
    let mut parameter_sets = Vec::new();
    let mut found = [false; 3];
    for _ in 0..array_count {
        let header = *data
            .get(cursor)
            .ok_or(Error::Invalid("truncated hvcC array"))?;
        cursor += 1;
        let nal_type = header & 0x3f;
        let count = usize::from(read_u16(data, cursor)?);
        cursor += 2;
        for _ in 0..count {
            let length = usize::from(read_u16(data, cursor)?);
            cursor += 2;
            let end = cursor
                .checked_add(length)
                .ok_or(Error::Invalid("hvcC parameter set offset overflow"))?;
            let nal = data
                .get(cursor..end)
                .ok_or(Error::Invalid("truncated hvcC parameter set"))?;
            if matches!(nal_type, 32..=34) {
                push_parameter_set(&mut parameter_sets, nal)?;
                found[(nal_type - 32) as usize] = true;
            }
            cursor = end;
        }
    }
    if !found.into_iter().all(|value| value) {
        return Err(Error::Unsupported(
            "hvcC requires VPS, SPS, and PPS for random access",
        ));
    }
    // Configuration arrays are commonly ordered VPS/SPS/PPS, but the record
    // does not require callers to depend on array order. Feed the decoder in
    // parameter-set dependency order.
    parameter_sets.sort_by_key(|nal| nal.first().map_or(255, |header| (header >> 1) & 0x3f));
    Ok(CodecConfig {
        codec: VideoCodec::H265,
        nal_length_size,
        parameter_sets,
    })
}

fn read_parameter_sets(
    data: &[u8],
    cursor: &mut usize,
    count: usize,
    output: &mut Vec<Vec<u8>>,
) -> Result<()> {
    for _ in 0..count {
        let length = usize::from(read_u16(data, *cursor)?);
        *cursor += 2;
        let end = cursor
            .checked_add(length)
            .ok_or(Error::Invalid("parameter set offset overflow"))?;
        let nal = data
            .get(*cursor..end)
            .ok_or(Error::Invalid("truncated parameter set"))?;
        push_parameter_set(output, nal)?;
        *cursor = end;
    }
    Ok(())
}

fn push_parameter_set(output: &mut Vec<Vec<u8>>, nal: &[u8]) -> Result<()> {
    if nal.is_empty() {
        return Err(Error::Invalid("empty codec parameter set"));
    }
    if nal.len() > MAX_PARAMETER_SET_BYTES {
        return Err(Error::Limit("codec parameter set is too large"));
    }
    if output.len() >= MAX_PARAMETER_SETS {
        return Err(Error::Limit("too many codec parameter sets"));
    }
    output.push(nal.to_vec());
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_avcc_parameter_sets() {
        let avcc = [
            1, 100, 0, 40, 0xff, 0xe1, 0, 2, 0x67, 0x64, 1, 0, 2, 0x68, 0xee,
        ];
        let config = parse_avcc(&avcc).unwrap();
        assert_eq!(config.codec, VideoCodec::H264);
        assert_eq!(config.nal_length_size, 4);
        assert_eq!(
            config.parameter_sets,
            vec![vec![0x67, 0x64], vec![0x68, 0xee]]
        );
    }

    #[test]
    fn rejects_avcc_without_pps() {
        let avcc = [1, 66, 0, 30, 0xff, 0xe1, 0, 1, 0x67, 0];
        assert!(parse_avcc(&avcc).is_err());
    }
}
