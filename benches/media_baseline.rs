//! Reproducible local media baselines for production-hardening work.
//!
//! These benchmarks intentionally exercise the same private VPx/H264 encoders,
//! decoders, and RTP packetizers as the SDK without making benchmark hooks part
//! of the public API.

use std::hint::black_box;
use std::time::Duration;

use criterion::{BenchmarkId, Criterion, Throughput, criterion_group, criterion_main};
use getstream::rtc::{PcmFrame, StreamResampler};

// The Criterion target is a separate crate. Including the exact implementation
// files here lets it measure crate-private codec paths without publishing test
// hooks or copying implementations that could drift.
#[path = "support/rtc_sources.rs"]
mod rtc;

use rtc::rtp_vpx::VpxRtpPacketizer;
use rtc::vpx::{VpxCodec, VpxEncoder};
use rtc::vpx_decode::VpxDecoder;
use rtc::{
    h264::{H264Decoder, H264Encoder},
    rtp_h264::H264RtpPacketizer,
};

const FRAME_DURATION_MS: i64 = 33;
const RTP_PAYLOAD_MTU: usize = 1_200;

fn tone(sample_rate: u32, channels: u16, milliseconds: usize) -> PcmFrame {
    let frames = sample_rate as usize * milliseconds / 1_000;
    let mut samples = Vec::with_capacity(frames * channels as usize);
    for index in 0..frames {
        let sample = ((index as f64 * 440.0 * std::f64::consts::TAU / f64::from(sample_rate)).sin()
            * 12_000.0) as i16;
        samples.extend(std::iter::repeat_n(sample, channels as usize));
    }
    PcmFrame::new(samples, sample_rate, channels)
}

fn i420_fixture(width: u32, height: u32) -> Vec<u8> {
    let (width, height) = (width as usize, height as usize);
    let mut frame = Vec::with_capacity(width * height * 3 / 2);
    for y in 0..height {
        for x in 0..width {
            frame.push(((x + y) % 220 + 16) as u8);
        }
    }
    frame.extend(std::iter::repeat_n(128, width * height / 4));
    frame.extend(std::iter::repeat_n(128, width * height / 4));
    frame
}

fn encoded_keyframe(codec: VpxCodec, width: u32, height: u32) -> Vec<u8> {
    let source = i420_fixture(width, height);
    let mut encoder =
        VpxEncoder::new(codec, width, height, 1_000).expect("create fixture VPx encoder");
    encoder
        .encode(&source, 0, FRAME_DURATION_MS, true)
        .expect("encode fixture VPx keyframe")
        .into_iter()
        .find(|frame| !frame.data.is_empty())
        .expect("fixture encoder must emit a frame")
        .data
}

fn encoded_h264_keyframe(width: u32, height: u32) -> Vec<u8> {
    let source = i420_fixture(width, height);
    let mut encoder = H264Encoder::new(1_000_000).expect("create fixture H264 encoder");
    let mut encoded = Vec::new();
    let keyframe = encoder
        .encode_into(&source, width, height, true, &mut encoded)
        .expect("encode fixture H264 keyframe");
    assert!(keyframe, "fixture H264 frame must be a keyframe");
    assert!(!encoded.is_empty(), "fixture H264 frame must not be empty");
    encoded
}

fn bench_resampling(criterion: &mut Criterion) {
    let mut group = criterion.benchmark_group("resample");
    for (name, input) in [
        ("44k1_stereo_to_48k_mono_20ms", tone(44_100, 2, 20)),
        ("24k_mono_to_48k_mono_20ms", tone(24_000, 1, 20)),
    ] {
        group.throughput(Throughput::Elements(input.frames() as u64));
        let mut resampler = StreamResampler::to_opus_mono();
        group.bench_function(name, |bencher| {
            bencher.iter(|| black_box(resampler.push(black_box(&input))));
        });
    }
    group.finish();
}

fn bench_opus(criterion: &mut Criterion) {
    let pcm = tone(48_000, 1, 20).samples;
    let mut encoder = rtc::opus::Encoder::new_voip_mono().expect("create Opus encoder");
    let mut encoded = vec![0u8; 1_500];

    let mut group = criterion.benchmark_group("opus");
    group.throughput(Throughput::Elements(pcm.len() as u64));
    group.bench_function("encode_48k_mono_20ms", |bencher| {
        bencher.iter(|| {
            let length = encoder
                .encode(black_box(&pcm), black_box(&mut encoded))
                .expect("encode Opus benchmark frame");
            black_box(length)
        });
    });

    let encoded_length = encoder
        .encode(&pcm, &mut encoded)
        .expect("encode Opus decode fixture");
    encoded.truncate(encoded_length);
    let mut decoder = rtc::opus::Decoder::new_mono().expect("create Opus decoder");
    let mut decoded = vec![0i16; 5_760];
    group.throughput(Throughput::Bytes(encoded.len() as u64));
    group.bench_function("decode_48k_mono_20ms", |bencher| {
        bencher.iter(|| {
            let length = decoder
                .decode(
                    black_box(encoded.as_slice()),
                    black_box(&mut decoded),
                    false,
                )
                .expect("decode Opus benchmark frame");
            black_box(length)
        });
    });
    group.finish();
}

fn bench_rtp_packetization(criterion: &mut Criterion) {
    let frame = vec![0x5a; 64 * 1_024];
    let mut group = criterion.benchmark_group("rtp_packetize");
    group.throughput(Throughput::Bytes(frame.len() as u64));
    for codec in [VpxCodec::Vp8, VpxCodec::Vp9] {
        let mut packetizer = VpxRtpPacketizer::new(codec);
        group.bench_with_input(
            BenchmarkId::from_parameter(match codec {
                VpxCodec::Vp8 => "vp8_64k_keyframe",
                VpxCodec::Vp9 => "vp9_64k_keyframe",
            }),
            &codec,
            |bencher, _| {
                bencher.iter(|| {
                    black_box(packetizer.packetize(
                        black_box(&frame),
                        true,
                        1_280,
                        720,
                        RTP_PAYLOAD_MTU,
                    ))
                });
            },
        );
    }
    let frame = encoded_h264_keyframe(1_280, 720);
    let mut packetizer = H264RtpPacketizer::default();
    group.throughput(Throughput::Bytes(frame.len() as u64));
    group.bench_function("h264_720p_keyframe", |bencher| {
        bencher.iter(|| {
            black_box(
                packetizer
                    .packetize(black_box(&frame), RTP_PAYLOAD_MTU)
                    .expect("packetize H264 benchmark frame"),
            )
        });
    });
    group.finish();
}

fn bench_vpx_encode(criterion: &mut Criterion) {
    let mut group = criterion.benchmark_group("vpx_encode");
    for (codec, codec_name) in [(VpxCodec::Vp8, "vp8"), (VpxCodec::Vp9, "vp9")] {
        for (width, height) in [(640, 360), (1_280, 720)] {
            let source = i420_fixture(width, height);
            let mut encoder =
                VpxEncoder::new(codec, width, height, 1_000).expect("create VPx encoder");
            let _ = encoder
                .encode(&source, 0, FRAME_DURATION_MS, true)
                .expect("warm VPx encoder");
            let mut pts = FRAME_DURATION_MS;

            group.throughput(Throughput::Bytes(source.len() as u64));
            group.bench_function(
                format!("{codec_name}_{width}x{height}_realtime_sequence"),
                |bencher| {
                    bencher.iter(|| {
                        let force_keyframe = pts % 990 == 0;
                        let frames = encoder
                            .encode(black_box(&source), pts, FRAME_DURATION_MS, force_keyframe)
                            .expect("encode VPx benchmark frame");
                        pts += FRAME_DURATION_MS;
                        black_box(frames)
                    });
                },
            );
        }
    }
    group.finish();
}

fn bench_vpx_decode(criterion: &mut Criterion) {
    let mut group = criterion.benchmark_group("vpx_decode");
    for (codec, codec_name) in [(VpxCodec::Vp8, "vp8"), (VpxCodec::Vp9, "vp9")] {
        for (width, height) in [(640, 360), (1_280, 720)] {
            let encoded = encoded_keyframe(codec, width, height);
            let mut decoder = VpxDecoder::new(codec).expect("create VPx decoder");

            group.throughput(Throughput::Bytes(encoded.len() as u64));
            group.bench_function(
                format!("{codec_name}_{width}x{height}_keyframe"),
                |bencher| {
                    bencher.iter(|| {
                        black_box(
                            decoder
                                .decode(black_box(&encoded), 90_000)
                                .expect("decode VPx benchmark frame"),
                        )
                    });
                },
            );
        }
    }
    group.finish();
}

fn bench_h264_encode(criterion: &mut Criterion) {
    let mut group = criterion.benchmark_group("h264_encode");
    for (width, height) in [(640, 360), (1_280, 720)] {
        let mut source = i420_fixture(width, height);
        let mut encoder = H264Encoder::new(1_000_000).expect("create H264 encoder");
        let mut encoded = Vec::new();
        encoder
            .encode_into(&source, width, height, true, &mut encoded)
            .expect("warm H264 encoder");
        let mut frame_index = 0usize;

        group.throughput(Throughput::Bytes(source.len() as u64));
        group.bench_function(
            format!("h264_{width}x{height}_realtime_sequence"),
            |bencher| {
                bencher.iter(|| {
                    let offset = frame_index % source.len();
                    source[offset] = source[offset].wrapping_add(1);
                    let force_keyframe = frame_index.is_multiple_of(30);
                    let keyframe = encoder
                        .encode_into(
                            black_box(&source),
                            width,
                            height,
                            force_keyframe,
                            black_box(&mut encoded),
                        )
                        .expect("encode H264 benchmark frame");
                    frame_index = frame_index.wrapping_add(1);
                    black_box((keyframe, encoded.len()))
                });
            },
        );
    }
    group.finish();
}

/// Bounded multi-track publisher load: per iteration, encode a fixed fan-out of
/// concurrent audio and video tracks (3 Opus + 2 VP9 640x360) representing a
/// realtime multi-track publisher/forwarder cycle. The point is a *bounded*,
/// reproducible per-cycle cost — throughput is reported over the total encoded
/// bytes so a regression in any single codec path is visible in one number.
fn bench_multitrack_load(criterion: &mut Criterion) {
    const AUDIO_TRACKS: usize = 3;
    const VIDEO_TRACKS: usize = 2;
    const VIDEO_W: u32 = 640;
    const VIDEO_H: u32 = 360;

    let pcm = tone(48_000, 1, 20).samples;
    let mut audio_encoders: Vec<rtc::opus::Encoder> = (0..AUDIO_TRACKS)
        .map(|_| rtc::opus::Encoder::new_voip_mono().expect("create multitrack Opus encoder"))
        .collect();
    let mut audio_scratch = vec![0u8; 1_500];

    let source = i420_fixture(VIDEO_W, VIDEO_H);
    let mut video_encoders: Vec<VpxEncoder> = (0..VIDEO_TRACKS)
        .map(|_| {
            let mut encoder = VpxEncoder::new(VpxCodec::Vp9, VIDEO_W, VIDEO_H, 1_000)
                .expect("create multitrack VP9 encoder");
            let _ = encoder
                .encode(&source, 0, FRAME_DURATION_MS, true)
                .expect("warm multitrack VP9 encoder");
            encoder
        })
        .collect();

    let encoded_bytes =
        (AUDIO_TRACKS * pcm.len() * std::mem::size_of::<i16>()) + (VIDEO_TRACKS * source.len());
    let mut pts = FRAME_DURATION_MS;

    let mut group = criterion.benchmark_group("multitrack");
    group.throughput(Throughput::Bytes(encoded_bytes as u64));
    group.bench_function("3x_opus_2x_vp9_360p_cycle", |bencher| {
        bencher.iter(|| {
            for encoder in &mut audio_encoders {
                let length = encoder
                    .encode(black_box(&pcm), black_box(&mut audio_scratch))
                    .expect("encode multitrack Opus frame");
                black_box(length);
            }
            let force_keyframe = pts % 990 == 0;
            for encoder in &mut video_encoders {
                let frames = encoder
                    .encode(black_box(&source), pts, FRAME_DURATION_MS, force_keyframe)
                    .expect("encode multitrack VP9 frame");
                black_box(frames);
            }
            pts += FRAME_DURATION_MS;
        });
    });
    group.finish();
}

fn bench_h264_decode(criterion: &mut Criterion) {
    let mut group = criterion.benchmark_group("h264_decode");
    for (width, height) in [(640, 360), (1_280, 720)] {
        let encoded = encoded_h264_keyframe(width, height);
        let mut decoder = H264Decoder::new().expect("create H264 decoder");

        group.throughput(Throughput::Bytes(encoded.len() as u64));
        group.bench_function(format!("h264_{width}x{height}_keyframe"), |bencher| {
            bencher.iter(|| {
                black_box(
                    decoder
                        .decode(black_box(&encoded), 90_000)
                        .expect("decode H264 benchmark frame"),
                )
            });
        });
    }
    group.finish();
}

criterion_group! {
    name = media_baselines;
    config = Criterion::default()
        .warm_up_time(Duration::from_secs(1))
        .measurement_time(Duration::from_secs(3))
        .sample_size(20);
    targets =
        bench_resampling,
        bench_opus,
        bench_rtp_packetization,
        bench_vpx_encode,
        bench_vpx_decode,
        bench_h264_encode,
        bench_h264_decode,
        bench_multitrack_load
}
criterion_main!(media_baselines);
