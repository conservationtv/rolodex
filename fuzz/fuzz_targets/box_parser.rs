#![no_main]

use libfuzzer_sys::fuzz_target;

fuzz_target!(|data: &[u8]| {
    if let Ok(root) = rolodex_core::mp4::boxes::box_at(data, 0, data.len()) {
        let _ = rolodex_core::mp4::boxes::children(data, root);
    }
    let _ = rolodex_core::mp4::parse_archive_moov(data);
    let _ = rolodex_core::mp4::build_canonical_init(data, &[1]);
});
