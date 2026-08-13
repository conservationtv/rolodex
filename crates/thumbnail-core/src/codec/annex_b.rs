use crate::{Error, Result};

use super::VideoCodec;

const MAX_NALS_PER_SAMPLE: usize = 16_384;

struct NalUnits<'a> {
    input: &'a [u8],
    nal_length_size: usize,
    cursor: usize,
    count: usize,
    failed: bool,
}

impl<'a> Iterator for NalUnits<'a> {
    type Item = Result<&'a [u8]>;

    fn next(&mut self) -> Option<Self::Item> {
        if self.failed || self.cursor == self.input.len() {
            return None;
        }
        if self.count >= MAX_NALS_PER_SAMPLE {
            self.failed = true;
            return Some(Err(Error::Limit("too many NAL units in one sample")));
        }

        let length_end = match self.cursor.checked_add(self.nal_length_size) {
            Some(end) => end,
            None => {
                self.failed = true;
                return Some(Err(Error::Invalid("NAL length offset overflow")));
            }
        };
        let Some(prefix) = self.input.get(self.cursor..length_end) else {
            self.failed = true;
            return Some(Err(Error::Invalid("truncated NAL length")));
        };
        let mut nal_len = 0usize;
        for byte in prefix {
            let Some(length) = nal_len
                .checked_mul(256)
                .and_then(|value| value.checked_add(*byte as usize))
            else {
                self.failed = true;
                return Some(Err(Error::Limit("NAL length overflow")));
            };
            nal_len = length;
        }

        self.cursor = length_end;
        let nal_end = match self.cursor.checked_add(nal_len) {
            Some(end) => end,
            None => {
                self.failed = true;
                return Some(Err(Error::Invalid("NAL payload offset overflow")));
            }
        };
        let Some(nal) = self.input.get(self.cursor..nal_end) else {
            self.failed = true;
            return Some(Err(Error::Invalid("NAL length exceeds sample")));
        };
        self.cursor = nal_end;
        self.count += 1;
        Some(Ok(nal))
    }
}

fn nal_units(input: &[u8], nal_length_size: usize) -> Result<NalUnits<'_>> {
    if !(1..=4).contains(&nal_length_size) {
        return Err(Error::Invalid("NAL length size must be in 1..=4"));
    }
    Ok(NalUnits {
        input,
        nal_length_size,
        cursor: 0,
        count: 0,
        failed: false,
    })
}

pub fn mp4_sample_to_annex_b(
    input: &[u8],
    nal_length_size: usize,
    output: &mut Vec<u8>,
) -> Result<usize> {
    let mut count = 0usize;
    for nal in nal_units(input, nal_length_size)? {
        let nal = nal?;
        if nal.is_empty() {
            continue;
        }
        output
            .try_reserve(4 + nal.len())
            .map_err(|_| Error::Limit("Annex-B allocation failed"))?;
        output.extend_from_slice(&[0, 0, 0, 1]);
        output.extend_from_slice(nal);
        count += 1;
    }

    if count == 0 {
        return Err(Error::Invalid("video sample contains no NAL units"));
    }
    Ok(count)
}

pub(crate) fn annex_b_access_unit(input: &[u8], nal_length_size: usize) -> Result<Vec<u8>> {
    let mut output = Vec::new();
    output
        .try_reserve(input.len().saturating_add(64))
        .map_err(|_| Error::Limit("access-unit allocation failed"))?;
    mp4_sample_to_annex_b(input, nal_length_size, &mut output)?;
    Ok(output)
}

pub fn sample_has_random_access_nal(
    input: &[u8],
    nal_length_size: usize,
    codec: VideoCodec,
) -> Result<bool> {
    for nal in nal_units(input, nal_length_size)? {
        let nal = nal?;
        let random_access = match codec {
            VideoCodec::H264 => nal.first().is_some_and(|header| header & 0x1f == 5),
            VideoCodec::H265 => nal
                .first()
                .is_some_and(|header| matches!((header >> 1) & 0x3f, 16..=21)),
        };
        if random_access {
            return Ok(true);
        }
    }
    Ok(false)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn converts_four_byte_lengths() {
        let input = [0, 0, 0, 2, 0x65, 0xaa, 0, 0, 0, 1, 0x06];
        let mut output = vec![9];
        let count = mp4_sample_to_annex_b(&input, 4, &mut output).unwrap();
        assert_eq!(count, 2);
        assert_eq!(output, [9, 0, 0, 0, 1, 0x65, 0xaa, 0, 0, 0, 1, 0x06]);
        assert!(sample_has_random_access_nal(&input, 4, VideoCodec::H264).unwrap());
    }

    #[test]
    fn allocates_and_converts_an_access_unit() {
        let input = [0, 0, 0, 2, 0x65, 0xaa];
        assert_eq!(
            annex_b_access_unit(&input, 4).unwrap(),
            [0, 0, 0, 1, 0x65, 0xaa]
        );
    }

    #[test]
    fn detects_hevc_irap() {
        let input = [0, 3, 21 << 1, 1, 0xaa];
        assert!(sample_has_random_access_nal(&input, 2, VideoCodec::H265).unwrap());
    }

    #[test]
    fn rejects_truncated_nal() {
        let input = [0, 0, 0, 4, 0x65];
        let mut output = Vec::new();
        assert!(mp4_sample_to_annex_b(&input, 4, &mut output).is_err());
    }

    #[test]
    fn skips_zero_length_nals_consistently() {
        let input = [0, 0, 0, 0, 0, 0, 0, 2, 0x65, 0xaa];
        let mut output = Vec::new();

        let count = mp4_sample_to_annex_b(&input, 4, &mut output).unwrap();

        assert_eq!(count, 1);
        assert_eq!(output, [0, 0, 0, 1, 0x65, 0xaa]);
        assert!(sample_has_random_access_nal(&input, 4, VideoCodec::H264).unwrap());
    }

    #[test]
    fn empty_nal_entries_are_not_usable_video() {
        let input = [0, 0, 0, 0];
        let mut output = Vec::new();

        assert!(mp4_sample_to_annex_b(&input, 4, &mut output).is_err());
        assert!(!sample_has_random_access_nal(&input, 4, VideoCodec::H264).unwrap());
        assert!(output.is_empty());
    }
}
