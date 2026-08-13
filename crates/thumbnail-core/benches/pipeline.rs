use std::hint::black_box;

use criterion::{Criterion, criterion_group, criterion_main};
use thumbnail_core::{
    BUILD_CODEC, ProcessorOptions, SourceIdentity,
    codec::{SelectedDecoder, VideoDecoder, mp4_sample_to_annex_b},
    jpeg::encode_jpeg,
    mp4::{parse_archive_moov, parse_indexed_fragment, read_top_level_header},
    process_recording,
    scale::scale_yuv420,
    source::MemorySource,
};

struct FixtureParts {
    data: Vec<u8>,
    moov: Vec<u8>,
    moof: Vec<u8>,
    moof_offset: u64,
    sample: Vec<u8>,
    nal_length_size: usize,
    config: thumbnail_core::CodecConfig,
}

fn fixture() -> FixtureParts {
    #[cfg(feature = "codec-h264")]
    let relative = "../../fixtures/h264/synthetic-320x180.mp4";
    #[cfg(feature = "codec-h265")]
    let relative = "../../fixtures/h265/synthetic-320x180.mp4";
    let data = std::fs::read(format!("{}/{}", env!("CARGO_MANIFEST_DIR"), relative)).unwrap();

    let mut offset = 0u64;
    let mut moov = None;
    let mut moof = None;
    while offset < data.len() as u64 {
        let start = offset as usize;
        let header_end = (start + 16).min(data.len());
        let header =
            read_top_level_header(&data[start..header_end], offset, data.len() as u64).unwrap();
        let end = header.end().unwrap() as usize;
        match &header.kind {
            b"moov" => moov = Some(data[start..end].to_vec()),
            b"moof" if moof.is_none() => moof = Some((offset, data[start..end].to_vec())),
            _ => {}
        }
        offset = end as u64;
    }

    let moov = moov.unwrap();
    let (moof_offset, moof) = moof.unwrap();
    let movie = parse_archive_moov(&moov).unwrap();
    let defaults = movie
        .tracks
        .iter()
        .map(|track| (track.track_id, track.defaults))
        .collect::<Vec<_>>();
    let fragment = parse_indexed_fragment(&moof, moof_offset, &defaults).unwrap();
    let first = fragment
        .tracks
        .into_iter()
        .find(|track| track.track_id == movie.video.track_id)
        .unwrap()
        .samples
        .into_iter()
        .find(|sample| sample.location.is_sync)
        .unwrap()
        .location;
    let start = first.offset as usize;
    let sample = data[start..start + first.size as usize].to_vec();

    FixtureParts {
        data,
        moov,
        moof,
        moof_offset,
        sample,
        nal_length_size: movie.video.config.nal_length_size,
        config: movie.video.config,
    }
}

fn decoded_frame(parts: &FixtureParts) -> thumbnail_core::frame::DecodedFrame {
    let mut annex_b = Vec::new();
    mp4_sample_to_annex_b(&parts.sample, parts.nal_length_size, &mut annex_b).unwrap();
    let mut decoder = SelectedDecoder::default();
    decoder.initialise(&parts.config).unwrap();
    decoder.push_access_unit(&annex_b).unwrap().unwrap()
}

fn benchmark_stages(criterion: &mut Criterion) {
    let parts = fixture();
    let name = |stage: &str| format!("{stage}_{BUILD_CODEC}");

    criterion.bench_function(&name("parse_archive_moov"), |bencher| {
        bencher.iter(|| parse_archive_moov(black_box(&parts.moov)).unwrap())
    });
    criterion.bench_function(&name("parse_moof"), |bencher| {
        bencher.iter(|| {
            let movie = parse_archive_moov(&parts.moov).unwrap();
            let defaults = movie
                .tracks
                .iter()
                .map(|track| (track.track_id, track.defaults))
                .collect::<Vec<_>>();
            parse_indexed_fragment(black_box(&parts.moof), parts.moof_offset, &defaults).unwrap()
        })
    });
    criterion.bench_function(&name("nal_conversion"), |bencher| {
        bencher.iter(|| {
            let mut output = Vec::new();
            mp4_sample_to_annex_b(black_box(&parts.sample), parts.nal_length_size, &mut output)
                .unwrap();
            output
        })
    });
    criterion.bench_function(&name("decode_first_frame"), |bencher| {
        let mut annex_b = Vec::new();
        mp4_sample_to_annex_b(&parts.sample, parts.nal_length_size, &mut annex_b).unwrap();
        bencher.iter(|| {
            let mut decoder = SelectedDecoder::default();
            decoder.initialise(&parts.config).unwrap();
            decoder.push_access_unit(black_box(&annex_b)).unwrap()
        })
    });

    let frame = decoded_frame(&parts);
    criterion.bench_function(&name("scale_yuv"), |bencher| {
        bencher.iter(|| scale_yuv420(black_box(&frame), 480, 270).unwrap())
    });
    let scaled = scale_yuv420(&frame, 480, 270).unwrap();
    criterion.bench_function(&name("encode_jpeg"), |bencher| {
        bencher.iter(|| encode_jpeg(black_box(&scaled), 78).unwrap())
    });

    let source = MemorySource::new(parts.data);
    criterion.bench_function(&name("complete_processor_synthetic"), |bencher| {
        bencher.iter(|| {
            futures::executor::block_on(process_recording(
                black_box(&source),
                SourceIdentity {
                    object_key: "fixture.mp4".into(),
                    etag: "fixture".into(),
                },
                ProcessorOptions {
                    require_media_mtx: false,
                    ..ProcessorOptions::default()
                },
            ))
            .unwrap()
        })
    });

    #[cfg(feature = "codec-h264")]
    let mediamtx_relative = "../../fixtures/h264/mediamtx-v1.18.2-640x360.mp4";
    #[cfg(feature = "codec-h265")]
    let mediamtx_relative = "../../fixtures/h265/mediamtx-v1.18.2-640x360.mp4";
    let mediamtx = MemorySource::new(
        std::fs::read(format!(
            "{}/{}",
            env!("CARGO_MANIFEST_DIR"),
            mediamtx_relative
        ))
        .unwrap(),
    );
    criterion.bench_function(&name("complete_processor"), |bencher| {
        bencher.iter(|| {
            futures::executor::block_on(process_recording(
                black_box(&mediamtx),
                SourceIdentity {
                    object_key: "fixture.mp4".into(),
                    etag: "fixture".into(),
                },
                ProcessorOptions::default(),
            ))
            .unwrap()
        })
    });
}

criterion_group!(benches, benchmark_stages);
criterion_main!(benches);
