mod archive;
pub mod boxes;
pub mod fragment;
pub mod init;

pub use archive::{
    ArchiveMovie, ArchiveTrack, MediaMtxInfo, TrackKind, build_canonical_init, fourcc_string,
    parse_archive_moov,
};
pub use boxes::{BoxHeader, read_top_level_header};
pub use fragment::{
    IndexedFragment, IndexedSample, IndexedTrackFragment, SampleDefaults, SampleLocation,
    parse_indexed_fragment, rewrite_fragment_timing,
};
pub use init::{VideoTrack, parse_moov};
