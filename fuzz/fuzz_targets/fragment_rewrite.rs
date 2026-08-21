#![no_main]

use libfuzzer_sys::fuzz_target;
use rolodex_core::mp4::rewrite_fragment_timing;

fuzz_target!(|data: &[u8]| {
    let Some(header) = data.get(..16) else {
        return;
    };
    let sequence_number = u32::from_le_bytes(header[0..4].try_into().unwrap());
    let track_id = u32::from_le_bytes(header[4..8].try_into().unwrap());
    let base_decode_time = u64::from_le_bytes(header[8..16].try_into().unwrap());
    let mut fragment = data[16..].to_vec();
    let _ = rewrite_fragment_timing(
        &mut fragment,
        sequence_number,
        &[(track_id, base_decode_time)],
    );
});
