#![no_main]

use libfuzzer_sys::fuzz_target;
use thumbnail_core::codec::{VideoCodec, mp4_sample_to_annex_b, sample_has_random_access_nal};

fuzz_target!(|input: (&[u8], u8)| {
    let (data, encoded_length_size) = input;
    let length_size = usize::from(encoded_length_size % 6);
    let mut output = Vec::new();
    let _ = mp4_sample_to_annex_b(data, length_size, &mut output);
    let _ = sample_has_random_access_nal(data, length_size, VideoCodec::H264);
    let _ = sample_has_random_access_nal(data, length_size, VideoCodec::H265);
});
