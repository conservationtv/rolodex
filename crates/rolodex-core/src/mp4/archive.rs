use crate::{Error, Result};

use super::{
    boxes::{
        BoxView, box_at, child, children, children_between, read_i64, read_u16, read_u32, read_u64,
    },
    fragment::SampleDefaults,
    init::{VideoTrack, parse_moov, parse_track_id, parse_trex},
};

const CANONICAL_FTYP: &[u8] = &[
    0, 0, 0, 36, b'f', b't', b'y', b'p', b'i', b's', b'o', b'6', 0, 0, 0, 1, b'i', b's', b'o',
    b'6', b'm', b'p', b'4', b'1', b'm', b'p', b'4', b'2', b'i', b's', b'o', b'm', b'h', b'l', b's',
    b'f',
];

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TrackKind {
    Video,
    Audio,
    Other([u8; 4]),
}

impl TrackKind {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Video => "video",
            Self::Audio => "audio",
            Self::Other(_) => "other",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ArchiveTrack {
    pub track_id: u32,
    pub timescale: u32,
    pub kind: TrackKind,
    pub codec: [u8; 4],
    pub width: Option<u16>,
    pub height: Option<u16>,
    pub defaults: SampleDefaults,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MediaMtxInfo {
    pub stream_id: [u8; 16],
    pub segment_number: u64,
    pub start_dts_ns: i64,
    pub start_ntp_ns: i64,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ArchiveMovie {
    pub video: VideoTrack,
    pub tracks: Vec<ArchiveTrack>,
    pub media_mtx: Option<MediaMtxInfo>,
}

pub fn parse_archive_moov(data: &[u8]) -> Result<ArchiveMovie> {
    let video = parse_moov(data)?;
    let moov = box_at(data, 0, data.len())?;
    if moov.kind != *b"moov" || moov.end() != data.len() {
        return Err(Error::Invalid("buffer is not exactly one moov box"));
    }

    let mut defaults = Vec::new();
    let mut track_boxes = Vec::new();
    let mut media_mtx = None;
    let mut saw_mvex = false;
    for view in children(data, moov)? {
        let view = view?;
        match &view.kind {
            b"trak" => {
                if saw_mvex {
                    return Err(Error::Invalid("mvex appears before the last trak"));
                }
                track_boxes.push(view);
            }
            b"mvex" => {
                if saw_mvex {
                    return Err(Error::Invalid("moov contains multiple mvex boxes"));
                }
                saw_mvex = true;
                for trex in children(data, view)? {
                    let trex = trex?;
                    if trex.kind == *b"trex" {
                        let (track_id, track_defaults) = parse_trex(trex.payload(data)?)?;
                        if defaults.iter().any(|(existing, _)| *existing == track_id) {
                            return Err(Error::Invalid("duplicate trex track ID"));
                        }
                        defaults.push((track_id, track_defaults));
                    }
                }
            }
            b"udta" => {
                for entry in children(data, view)? {
                    let entry = entry?;
                    if entry.kind == *b"mtxi" {
                        if media_mtx.is_some() {
                            return Err(Error::Invalid("moov contains multiple mtxi boxes"));
                        }
                        media_mtx = Some(parse_mtxi(entry.payload(data)?)?);
                    }
                }
            }
            _ => {}
        }
    }
    if !saw_mvex {
        return Err(Error::Invalid("fragmented moov is missing mvex"));
    }

    let mut tracks = Vec::new();
    for trak in track_boxes {
        let mut track = parse_track(data, trak)?;
        track.defaults = defaults
            .iter()
            .find(|(track_id, _)| *track_id == track.track_id)
            .map(|(_, value)| *value)
            .ok_or(Error::Invalid("track is missing trex defaults"))?;
        if tracks
            .iter()
            .any(|existing: &ArchiveTrack| existing.track_id == track.track_id)
        {
            return Err(Error::Invalid("duplicate track ID in moov"));
        }
        tracks.push(track);
    }
    if tracks.is_empty() {
        return Err(Error::Invalid("moov contains no tracks"));
    }
    if !tracks.iter().any(|track| track.track_id == video.track_id) {
        return Err(Error::Invalid(
            "selected video track is absent from archive tracks",
        ));
    }
    if defaults
        .iter()
        .any(|(track_id, _)| !tracks.iter().any(|track| track.track_id == *track_id))
    {
        return Err(Error::Invalid("trex references an unknown track"));
    }

    Ok(ArchiveMovie {
        video,
        tracks,
        media_mtx,
    })
}

fn parse_mtxi(payload: &[u8]) -> Result<MediaMtxInfo> {
    if payload.len() != 44 {
        return Err(Error::Invalid("invalid mtxi payload size"));
    }
    if payload[0] != 0 {
        return Err(Error::Unsupported("unknown mtxi version"));
    }
    let mut stream_id = [0; 16];
    stream_id.copy_from_slice(&payload[4..20]);
    Ok(MediaMtxInfo {
        stream_id,
        segment_number: read_u64(payload, 20)?,
        start_dts_ns: read_i64(payload, 28)?,
        start_ntp_ns: read_i64(payload, 36)?,
    })
}

fn parse_track(data: &[u8], trak: BoxView) -> Result<ArchiveTrack> {
    let tkhd = child(data, trak, b"tkhd")?.ok_or(Error::Invalid("trak missing tkhd"))?;
    let mdia = child(data, trak, b"mdia")?.ok_or(Error::Invalid("trak missing mdia"))?;
    let track_id = parse_track_id(tkhd.payload(data)?)?;
    let mdhd = child(data, mdia, b"mdhd")?.ok_or(Error::Invalid("mdia missing mdhd"))?;
    let timescale = parse_timescale(mdhd.payload(data)?)?;
    let hdlr = child(data, mdia, b"hdlr")?.ok_or(Error::Invalid("mdia missing hdlr"))?;
    let hdlr_payload = hdlr.payload(data)?;
    let handler: [u8; 4] = hdlr_payload
        .get(8..12)
        .ok_or(Error::Invalid("truncated hdlr"))?
        .try_into()
        .expect("fixed handler slice");
    let kind = match &handler {
        b"vide" => TrackKind::Video,
        b"soun" => TrackKind::Audio,
        _ => TrackKind::Other(handler),
    };

    let minf = child(data, mdia, b"minf")?.ok_or(Error::Invalid("mdia missing minf"))?;
    let stbl = child(data, minf, b"stbl")?.ok_or(Error::Invalid("minf missing stbl"))?;
    validate_empty_sample_table(data, stbl)?;
    let stsd = child(data, stbl, b"stsd")?.ok_or(Error::Invalid("stbl missing stsd"))?;
    let payload = stsd.payload(data)?;
    if payload.len() < 8 || read_u32(payload, 4)? == 0 {
        return Err(Error::Invalid("stsd contains no sample entry"));
    }
    let entry = children_between(data, stsd.payload_start() + 8, stsd.end())?
        .next()
        .transpose()?
        .ok_or(Error::Invalid("stsd entry is missing"))?;
    let (width, height) = if kind == TrackKind::Video {
        let sample = entry.payload(data)?;
        (Some(read_u16(sample, 24)?), Some(read_u16(sample, 26)?))
    } else {
        (None, None)
    };

    Ok(ArchiveTrack {
        track_id,
        timescale,
        kind,
        codec: entry.kind,
        width,
        height,
        defaults: SampleDefaults::default(),
    })
}

fn validate_empty_sample_table(data: &[u8], stbl: BoxView) -> Result<()> {
    let mut found_sample_size = false;
    for view in children(data, stbl)? {
        let view = view?;
        let payload = view.payload(data)?;
        let count = match &view.kind {
            b"stsz" | b"stz2" => {
                found_sample_size = true;
                Some(read_u32(payload, 8)?)
            }
            b"stts" | b"ctts" | b"stsc" | b"stco" | b"co64" | b"stss" => {
                Some(read_u32(payload, 4)?)
            }
            _ => None,
        };
        if count.is_some_and(|value| value != 0) {
            return Err(Error::Unsupported(
                "initialization sample table contains media samples",
            ));
        }
    }
    if !found_sample_size {
        return Err(Error::Invalid("sample table is missing stsz/stz2"));
    }
    Ok(())
}

fn parse_timescale(payload: &[u8]) -> Result<u32> {
    let version = *payload.first().ok_or(Error::Invalid("empty mdhd"))?;
    let offset = match version {
        0 => 12,
        1 => 20,
        _ => return Err(Error::Unsupported("unknown mdhd version")),
    };
    let value = read_u32(payload, offset)?;
    if value == 0 {
        return Err(Error::Invalid("mdhd timescale is zero"));
    }
    Ok(value)
}

pub fn build_canonical_init(data: &[u8], requested_track_ids: &[u32]) -> Result<Vec<u8>> {
    let moov = box_at(data, 0, data.len())?;
    if moov.kind != *b"moov" || moov.end() != data.len() {
        return Err(Error::Invalid("buffer is not exactly one moov box"));
    }
    let mut active_track_ids = Vec::new();
    active_track_ids
        .try_reserve_exact(requested_track_ids.len())
        .map_err(|_| Error::Limit("active track allocation failed"))?;
    active_track_ids.extend_from_slice(requested_track_ids);
    active_track_ids.sort_unstable();
    active_track_ids.dedup();
    if active_track_ids.is_empty() {
        return Err(Error::Invalid("canonical init has no active tracks"));
    }

    let mut body = Vec::new();
    body.try_reserve_exact(data.len())
        .map_err(|_| Error::Limit("canonical moov allocation failed"))?;
    let mut retained_tracks = Vec::new();
    let mut retained_defaults = Vec::new();
    retained_tracks
        .try_reserve_exact(active_track_ids.len())
        .map_err(|_| Error::Limit("canonical track allocation failed"))?;
    retained_defaults
        .try_reserve_exact(active_track_ids.len())
        .map_err(|_| Error::Limit("canonical defaults allocation failed"))?;
    for view in children(data, moov)? {
        let view = view?;
        if view.kind == *b"udta" {
            continue;
        }
        let bytes = match &view.kind {
            b"trak" => {
                let track_id = trak_track_id(data, view)?;
                if active_track_ids.binary_search(&track_id).is_err() {
                    continue;
                }
                retained_tracks.push(track_id);
                let mut bytes = data[view.start..view.end()].to_vec();
                zero_trak_duration(&mut bytes)?;
                bytes
            }
            b"mvex" => canonical_mvex(data, view, &active_track_ids, &mut retained_defaults)?,
            _ => {
                let mut bytes = data[view.start..view.end()].to_vec();
                if view.kind == *b"mvhd" {
                    zero_duration(&mut bytes, *b"mvhd")?;
                }
                bytes
            }
        };
        body.extend_from_slice(&bytes);
    }
    retained_tracks.sort_unstable();
    retained_tracks.dedup();
    retained_defaults.sort_unstable();
    retained_defaults.dedup();
    if retained_tracks != active_track_ids {
        return Err(Error::Invalid(
            "active track is missing from canonical init",
        ));
    }
    if retained_defaults != active_track_ids {
        return Err(Error::Invalid(
            "active track is missing defaults from canonical init",
        ));
    }

    let size = body
        .len()
        .checked_add(8)
        .ok_or(Error::Limit("canonical moov size overflow"))?;
    let size = u32::try_from(size).map_err(|_| Error::Limit("canonical moov exceeds 32 bits"))?;
    let mut output = Vec::new();
    output
        .try_reserve_exact(CANONICAL_FTYP.len() + size as usize)
        .map_err(|_| Error::Limit("canonical init allocation failed"))?;
    output.extend_from_slice(CANONICAL_FTYP);
    output.extend_from_slice(&size.to_be_bytes());
    output.extend_from_slice(b"moov");
    output.extend_from_slice(&body);
    Ok(output)
}

fn trak_track_id(data: &[u8], trak: BoxView) -> Result<u32> {
    let tkhd = child(data, trak, b"tkhd")?.ok_or(Error::Invalid("trak missing tkhd"))?;
    parse_track_id(tkhd.payload(data)?)
}

fn canonical_mvex(
    data: &[u8],
    mvex: BoxView,
    active_track_ids: &[u32],
    retained_defaults: &mut Vec<u32>,
) -> Result<Vec<u8>> {
    let mut body = Vec::new();
    body.try_reserve_exact(mvex.size.saturating_sub(mvex.header_size))
        .map_err(|_| Error::Limit("canonical mvex allocation failed"))?;
    for view in children(data, mvex)? {
        let view = view?;
        if view.kind == *b"trex" {
            let (track_id, _) = parse_trex(view.payload(data)?)?;
            if active_track_ids.binary_search(&track_id).is_err() {
                continue;
            }
            retained_defaults.push(track_id);
        }
        body.extend_from_slice(&data[view.start..view.end()]);
    }
    make_box(*b"mvex", &body)
}

fn make_box(kind: [u8; 4], body: &[u8]) -> Result<Vec<u8>> {
    let size = body
        .len()
        .checked_add(8)
        .ok_or(Error::Limit("canonical box size overflow"))?;
    let size = u32::try_from(size).map_err(|_| Error::Limit("canonical box exceeds 32 bits"))?;
    let mut output = Vec::new();
    output
        .try_reserve_exact(size as usize)
        .map_err(|_| Error::Limit("canonical box allocation failed"))?;
    output.extend_from_slice(&size.to_be_bytes());
    output.extend_from_slice(&kind);
    output.extend_from_slice(body);
    Ok(output)
}

fn zero_trak_duration(data: &mut [u8]) -> Result<()> {
    let trak = box_at(data, 0, data.len())?;
    let tkhd = child(data, trak, b"tkhd")?.ok_or(Error::Invalid("trak missing tkhd"))?;
    let bytes = data
        .get_mut(tkhd.start..tkhd.end())
        .ok_or(Error::Invalid("tkhd outside trak"))?;
    zero_duration(bytes, *b"tkhd")
}

fn zero_duration(data: &mut [u8], expected: [u8; 4]) -> Result<()> {
    let view = box_at(data, 0, data.len())?;
    if view.kind != expected {
        return Err(Error::Invalid("unexpected duration box"));
    }
    let payload = data
        .get_mut(view.payload_start()..view.end())
        .ok_or(Error::Invalid("duration box payload outside buffer"))?;
    let version = *payload
        .first()
        .ok_or(Error::Invalid("empty duration box"))?;
    let (offset, length) = match (&expected, version) {
        (b"mvhd", 0) => (16, 4),
        (b"mvhd", 1) => (24, 8),
        (b"tkhd", 0) => (20, 4),
        (b"tkhd", 1) => (28, 8),
        (_, 0 | 1) => return Err(Error::Invalid("unsupported duration box kind")),
        _ => return Err(Error::Unsupported("unknown duration box version")),
    };
    payload
        .get_mut(offset..offset + length)
        .ok_or(Error::Invalid("truncated duration field"))?
        .fill(0);
    Ok(())
}

pub fn fourcc_string(value: [u8; 4]) -> String {
    value.iter().map(|byte| char::from(*byte)).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn mediamtx_moov() -> Vec<u8> {
        let path = format!(
            "{}/../../fixtures/h264/mediamtx-v1.18.2-640x360.mp4",
            env!("CARGO_MANIFEST_DIR")
        );
        let bytes = std::fs::read(path).unwrap();
        let ftyp = box_at(&bytes, 0, bytes.len()).unwrap();
        let moov = box_at(&bytes, ftyp.end(), bytes.len()).unwrap();
        bytes[moov.start..moov.end()].to_vec()
    }

    fn moov_with_unused_second_track() -> Vec<u8> {
        let source = mediamtx_moov();
        let moov = box_at(&source, 0, source.len()).unwrap();
        let trak = child(&source, moov, b"trak").unwrap().unwrap();
        let mvex = child(&source, moov, b"mvex").unwrap().unwrap();
        let trex = child(&source, mvex, b"trex").unwrap().unwrap();

        let mut second_trak = source[trak.start..trak.end()].to_vec();
        let second_trak_view = box_at(&second_trak, 0, second_trak.len()).unwrap();
        let tkhd = child(&second_trak, second_trak_view, b"tkhd")
            .unwrap()
            .unwrap();
        let track_id_offset = tkhd.payload_start()
            + match second_trak[tkhd.payload_start()] {
                0 => 12,
                1 => 20,
                _ => unreachable!(),
            };
        second_trak[track_id_offset..track_id_offset + 4].copy_from_slice(&2u32.to_be_bytes());

        let mut second_trex = source[trex.start..trex.end()].to_vec();
        let second_trex_view = box_at(&second_trex, 0, second_trex.len()).unwrap();
        let track_id_offset = second_trex_view.payload_start() + 4;
        second_trex[track_id_offset..track_id_offset + 4].copy_from_slice(&2u32.to_be_bytes());

        let mut mvex_body = source[mvex.payload_start()..mvex.end()].to_vec();
        mvex_body.extend_from_slice(&second_trex);
        let replacement_mvex = make_box(*b"mvex", &mvex_body).unwrap();

        let mut moov_body = Vec::new();
        for view in children(&source, moov).unwrap() {
            let view = view.unwrap();
            if view.kind == *b"mvex" {
                moov_body.extend_from_slice(&second_trak);
                moov_body.extend_from_slice(&replacement_mvex);
            } else {
                moov_body.extend_from_slice(&source[view.start..view.end()]);
            }
        }
        make_box(*b"moov", &moov_body).unwrap()
    }

    #[test]
    fn parses_mediamtx_identity_and_tracks() {
        let movie = parse_archive_moov(&mediamtx_moov()).unwrap();
        assert_eq!(movie.tracks.len(), 1);
        assert_eq!(movie.tracks[0].timescale, 90_000);
        assert_eq!(movie.tracks[0].codec, *b"avc1");
        assert_eq!(
            (movie.tracks[0].width, movie.tracks[0].height),
            (Some(640), Some(360))
        );
        assert!(movie.media_mtx.is_some());
    }

    #[test]
    fn canonical_init_has_iso6_and_no_recording_identity() {
        let source = mediamtx_moov();
        let source_moov = box_at(&source, 0, source.len()).unwrap();
        let source_mvhd = child(&source, source_moov, b"mvhd").unwrap().unwrap();
        assert_ne!(
            read_u32(source_mvhd.payload(&source).unwrap(), 16).unwrap(),
            0
        );

        let init = build_canonical_init(&source, &[1]).unwrap();
        assert_eq!(&init[8..12], b"iso6");
        assert!(!init.windows(4).any(|window| window == b"mtxi"));
        assert!(init.windows(4).any(|window| window == b"mvex"));

        let ftyp = box_at(&init, 0, init.len()).unwrap();
        let moov = box_at(&init, ftyp.end(), init.len()).unwrap();
        let mvhd = child(&init, moov, b"mvhd").unwrap().unwrap();
        assert_eq!(read_u32(mvhd.payload(&init).unwrap(), 16).unwrap(), 0);
        let trak = child(&init, moov, b"trak").unwrap().unwrap();
        let tkhd = child(&init, trak, b"tkhd").unwrap().unwrap();
        assert_eq!(read_u32(tkhd.payload(&init).unwrap(), 20).unwrap(), 0);
    }

    #[test]
    fn canonical_init_ignores_closed_duration_and_mtxi_identity() {
        let source = mediamtx_moov();
        let mut changed = source.clone();
        let moov = box_at(&changed, 0, changed.len()).unwrap();
        let mvhd = child(&changed, moov, b"mvhd").unwrap().unwrap();
        let duration_offset = mvhd.payload_start() + 16;
        changed[duration_offset..duration_offset + 4].copy_from_slice(&1234u32.to_be_bytes());
        let moov = box_at(&changed, 0, changed.len()).unwrap();
        let udta = child(&changed, moov, b"udta").unwrap().unwrap();
        let mtxi = child(&changed, udta, b"mtxi").unwrap().unwrap();
        changed[mtxi.payload_start() + 4..mtxi.payload_start() + 20].fill(0x5a);

        assert_eq!(
            build_canonical_init(&source, &[1]).unwrap(),
            build_canonical_init(&changed, &[1]).unwrap()
        );
    }

    #[test]
    fn canonical_init_omits_tracks_without_fragments() {
        let init = build_canonical_init(&moov_with_unused_second_track(), &[1]).unwrap();
        let ftyp = box_at(&init, 0, init.len()).unwrap();
        let moov = box_at(&init, ftyp.end(), init.len()).unwrap();
        let track_ids = children(&init, moov)
            .unwrap()
            .filter_map(|view| {
                let view = view.unwrap();
                (view.kind == *b"trak").then(|| trak_track_id(&init, view).unwrap())
            })
            .collect::<Vec<_>>();
        let mvex = child(&init, moov, b"mvex").unwrap().unwrap();
        let default_track_ids = children(&init, mvex)
            .unwrap()
            .filter_map(|view| {
                let view = view.unwrap();
                (view.kind == *b"trex").then(|| parse_trex(view.payload(&init).unwrap()).unwrap().0)
            })
            .collect::<Vec<_>>();

        assert_eq!(track_ids, vec![1]);
        assert_eq!(default_track_ids, vec![1]);
    }
}
