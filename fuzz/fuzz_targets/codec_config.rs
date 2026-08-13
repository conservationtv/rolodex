#![no_main]

use libfuzzer_sys::fuzz_target;

fuzz_target!(|data: &[u8]| {
    let _ = rolodex_core::mp4::init::parse_avcc(data);
    let _ = rolodex_core::mp4::init::parse_hvcc(data);
});
