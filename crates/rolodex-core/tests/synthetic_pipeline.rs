use rolodex_core::{
    ByteSource, Error, ProcessorOptions, ProcessorOutput, Result, SourceIdentity, VideoCodec,
    process_recording,
};

fn selected_fixture() -> Vec<u8> {
    #[cfg(feature = "codec-h264")]
    let path = "../../fixtures/h264/synthetic-320x180.mp4";
    #[cfg(feature = "codec-h265")]
    let path = "../../fixtures/h265/synthetic-320x180.mp4";
    std::fs::read(format!("{}/{}", env!("CARGO_MANIFEST_DIR"), path)).unwrap()
}

fn other_fixture() -> Vec<u8> {
    #[cfg(feature = "codec-h264")]
    let path = "../../fixtures/h265/synthetic-320x180.mp4";
    #[cfg(feature = "codec-h265")]
    let path = "../../fixtures/h264/synthetic-320x180.mp4";
    std::fs::read(format!("{}/{}", env!("CARGO_MANIFEST_DIR"), path)).unwrap()
}

fn process<S: ByteSource + ?Sized>(source: &S) -> Result<ProcessorOutput> {
    futures::executor::block_on(process_recording(
        source,
        SourceIdentity {
            object_key: "fixture.mp4".into(),
            etag: "fixture".into(),
        },
        ProcessorOptions::default(),
    ))
}

#[test]
fn creates_thumbnail_from_selected_codec_fixture() {
    let source = rolodex_core::source::MemorySource::new(selected_fixture());
    let output = process(&source).unwrap();
    assert_eq!(&output.thumbnail_jpeg.as_ref().unwrap()[..2], &[0xff, 0xd8]);
    assert_eq!(output.metrics.source_width, 320);
    assert_eq!(output.metrics.source_height, 180);
    assert_eq!(output.metrics.thumbnail_samples_decoded, 1);
    assert_eq!(output.metrics.parts_indexed, 1);
    assert_eq!(output.metadata.recording.stream_id, None);
    assert_eq!(output.metadata.recording.segment_number, None);
    assert_eq!(output.metadata.recording.start_dts_ns, None);
    assert_eq!(output.metadata.recording.start_utc_ns, None);
}

#[test]
fn decodes_checked_in_profile_corpus() {
    #[cfg(feature = "codec-h264")]
    let fixtures = [
        "mediamtx-v1.18.2-640x360.mp4",
        "synthetic-baseline-320x180.mp4",
        "synthetic-main-320x180.mp4",
        "synthetic-320x180.mp4",
    ];
    #[cfg(feature = "codec-h265")]
    let fixtures = [
        "mediamtx-v1.18.2-640x360.mp4",
        "synthetic-320x180.mp4",
        "synthetic-main10-320x180.mp4",
    ];
    #[cfg(feature = "codec-h264")]
    let codec_dir = "h264";
    #[cfg(feature = "codec-h265")]
    let codec_dir = "h265";

    for fixture in fixtures {
        let path = format!(
            "{}/../../fixtures/{codec_dir}/{fixture}",
            env!("CARGO_MANIFEST_DIR")
        );
        let source = rolodex_core::source::MemorySource::new(std::fs::read(path).unwrap());
        let output = process(&source).unwrap();
        assert_eq!(
            &output.thumbnail_jpeg.as_ref().unwrap()[..2],
            &[0xff, 0xd8],
            "{fixture}"
        );
        assert_eq!(output.metrics.thumbnail_frames, 1, "{fixture}");
    }
}

#[test]
fn rejects_opposite_codec_before_decoding() {
    let source = rolodex_core::source::MemorySource::new(other_fixture());
    let error = process(&source).unwrap_err();
    match error {
        Error::CodecMismatch { expected, actual } => {
            #[cfg(feature = "codec-h264")]
            assert_eq!((expected, actual), (VideoCodec::H264, VideoCodec::H265));
            #[cfg(feature = "codec-h265")]
            assert_eq!((expected, actual), (VideoCodec::H265, VideoCodec::H264));
        }
        other => panic!("expected codec mismatch, got {other:?}"),
    }
}

#[test]
fn skips_a_large_top_level_box_without_fetching_its_payload() {
    let fixture = selected_fixture();
    let first_box_size = u32::from_be_bytes(fixture[..4].try_into().unwrap()) as usize;
    let free_size = 2 * 1024 * 1024usize;
    let mut padded = Vec::with_capacity(fixture.len() + free_size);
    padded.extend_from_slice(&fixture[..first_box_size]);
    padded.extend_from_slice(&(free_size as u32).to_be_bytes());
    padded.extend_from_slice(b"free");
    padded.resize(first_box_size + free_size, 0);
    padded.extend_from_slice(&fixture[first_box_size..]);

    let source = rolodex_core::source::MemorySource::new(padded);
    let output = process(&source).unwrap();

    assert!(output.metrics.source_bytes > 2 * 1024 * 1024);
    assert!(output.metrics.range_requests >= 2);
    assert!(output.metrics.bytes_read < 512 * 1024);
    assert_eq!(output.metrics.thumbnail_samples_decoded, 1);
}

#[cfg(feature = "codec-h265")]
#[test]
fn decodes_main10_and_reduces_to_eight_bit_jpeg() {
    let path = format!(
        "{}/../../fixtures/h265/synthetic-main10-320x180.mp4",
        env!("CARGO_MANIFEST_DIR")
    );
    let source = rolodex_core::source::MemorySource::new(std::fs::read(path).unwrap());
    let output = process(&source).unwrap();
    assert_eq!(output.metrics.source_bit_depth, 10);
    assert_eq!(&output.thumbnail_jpeg.as_ref().unwrap()[..2], &[0xff, 0xd8]);
}
