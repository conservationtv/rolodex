#![no_main]

use libfuzzer_sys::fuzz_target;
use rolodex_core::mp4::{SampleDefaults, parse_indexed_fragment};

fuzz_target!(|data: &[u8]| {
    let defaults = SampleDefaults::default();
    let _ = parse_indexed_fragment(data, 0, &[(1, defaults)]);
});
