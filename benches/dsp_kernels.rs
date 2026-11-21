use std::hint::black_box;

use criterion::{BenchmarkId, Criterion, Throughput, criterion_group, criterion_main};
use pulp::Arch;

use objrender::io::interleave::{
    interleave_f32_le_scalar, interleave_f32_le_simd, interleave_f64_le_scalar,
    interleave_f64_le_simd,
};
use objrender::io::{NullWriter, PcmBus};
use objrender::layout::SpeakerLayout;
use objrender::math::{SphericalCoord, Vec3};
use objrender::mixer::simd::{
    accumulate_linear_ramp_scalar, accumulate_linear_ramp_simd, scale_and_meter_bus_scalar,
    scale_and_meter_bus_simd,
};
use objrender::mixer::{ChannelStats, RenderConfig, render_scene};
use objrender::renderer::VbapRenderer;
use objrender::scene::{
    AudioObject, InterpMode, Keyframe, ObjectId, ObjectRole, SceneBuilder, SineSource,
    SpatialParams,
};

const BENCH_CHANNELS: usize = 12;
const BLOCK_SIZES: [usize; 2] = [64, 256];

fn make_stats(n_ch: usize) -> Vec<ChannelStats> {
    (0..n_ch)
        .map(|i| ChannelStats {
            speaker_label: format!("SPK{i}"),
            peak_linear: 0.0,
            sum_squares: 0.0,
            clipped_samples: 0,
        })
        .collect()
}

fn filled_bus(n_ch: usize, n_frames: usize) -> PcmBus {
    let mut bus = PcmBus::new(n_ch, n_frames);
    for ch in 0..n_ch {
        for s in 0..n_frames {
            bus.channel_mut(ch)[s] = ((s as f64) * 0.031 + (ch as f64) * 0.17).sin() * 0.7;
        }
    }
    bus
}

fn bench_accumulate(c: &mut Criterion) {
    let arch = Arch::new();

    let mut const_group = c.benchmark_group("accumulate_const_gain");
    let const_gains0 = [0.5, 0.5, 0.3, 0.0, 0.2, 0.2, 0.4, 0.4, 0.1, 0.1, 0.25, 0.25];
    let const_gains1 = const_gains0;
    for n in BLOCK_SIZES {
        const_group.throughput(Throughput::Elements((BENCH_CHANNELS * n) as u64));
        let mono: Vec<f64> = (0..n).map(|i| (i as f64 * 0.11).sin()).collect();

        let mut bus_scalar = PcmBus::new(BENCH_CHANNELS, n);
        const_group.bench_with_input(BenchmarkId::new("scalar", n), &n, |b, &n| {
            b.iter(|| {
                accumulate_linear_ramp_scalar(
                    black_box(&mono),
                    black_box(&const_gains0),
                    black_box(&const_gains1),
                    &mut bus_scalar,
                    n,
                );
            });
        });

        let mut bus_simd = PcmBus::new(BENCH_CHANNELS, n);
        const_group.bench_with_input(BenchmarkId::new("pulp_simd", n), &n, |b, &n| {
            b.iter(|| {
                accumulate_linear_ramp_simd(
                    arch,
                    black_box(&mono),
                    black_box(&const_gains0),
                    black_box(&const_gains1),
                    &mut bus_simd,
                    n,
                );
            });
        });
    }
    const_group.finish();

    let mut ramp_group = c.benchmark_group("accumulate_linear_ramp");
    let ramp_gains0 = [0.2, 0.8, 0.4, 0.0, 0.1, 0.9, 0.3, 0.7, 0.5, 0.2, 0.6, 0.1];
    let ramp_gains1 = [0.8, 0.2, 0.9, 0.0, 0.7, 0.1, 0.8, 0.2, 0.1, 0.8, 0.2, 0.9];
    for n in BLOCK_SIZES {
        ramp_group.throughput(Throughput::Elements((BENCH_CHANNELS * n) as u64));
        let mono: Vec<f64> = (0..n).map(|i| (i as f64 * 0.11).sin()).collect();

        let mut bus_scalar = PcmBus::new(BENCH_CHANNELS, n);
        ramp_group.bench_with_input(BenchmarkId::new("scalar", n), &n, |b, &n| {
            b.iter(|| {
                accumulate_linear_ramp_scalar(
                    black_box(&mono),
                    black_box(&ramp_gains0),
                    black_box(&ramp_gains1),
                    &mut bus_scalar,
                    n,
                );
            });
        });

        let mut bus_simd = PcmBus::new(BENCH_CHANNELS, n);
        ramp_group.bench_with_input(BenchmarkId::new("pulp_simd", n), &n, |b, &n| {
            b.iter(|| {
                accumulate_linear_ramp_simd(
                    arch,
                    black_box(&mono),
                    black_box(&ramp_gains0),
                    black_box(&ramp_gains1),
                    &mut bus_simd,
                    n,
                );
            });
        });
    }
    ramp_group.finish();
}

fn bench_metering(c: &mut Criterion) {
    let arch = Arch::new();
    let mut group = c.benchmark_group("scale_and_meter_bus");

    for n in BLOCK_SIZES {
        group.throughput(Throughput::Elements((BENCH_CHANNELS * n) as u64));

        let mut bus_scalar = filled_bus(BENCH_CHANNELS, n);
        let mut stats_scalar = make_stats(BENCH_CHANNELS);
        group.bench_with_input(BenchmarkId::new("scalar", n), &n, |b, &n| {
            b.iter(|| {
                scale_and_meter_bus_scalar(&mut bus_scalar, n, black_box(-1.0), &mut stats_scalar);
            });
        });

        let mut bus_simd = filled_bus(BENCH_CHANNELS, n);
        let mut stats_simd = make_stats(BENCH_CHANNELS);
        group.bench_with_input(BenchmarkId::new("pulp_simd", n), &n, |b, &n| {
            b.iter(|| {
                scale_and_meter_bus_simd(arch, &mut bus_simd, n, black_box(-1.0), &mut stats_simd);
            });
        });
    }

    group.finish();
}

fn bench_interleave(c: &mut Criterion) {
    let arch = Arch::new();

    let mut f32_group = c.benchmark_group("interleave_f32_le");
    for n in BLOCK_SIZES {
        f32_group.throughput(Throughput::Bytes((BENCH_CHANNELS * n * 4) as u64));
        let bus = filled_bus(BENCH_CHANNELS, n);
        let mut out_scalar = Vec::with_capacity(BENCH_CHANNELS * n * 4);
        f32_group.bench_with_input(BenchmarkId::new("scalar", n), &n, |b, &n| {
            b.iter(|| {
                interleave_f32_le_scalar(black_box(&bus), n, &mut out_scalar);
                black_box(&out_scalar);
            });
        });
        let mut out_simd = Vec::with_capacity(BENCH_CHANNELS * n * 4);
        f32_group.bench_with_input(BenchmarkId::new("pulp_simd", n), &n, |b, &n| {
            b.iter(|| {
                interleave_f32_le_simd(arch, black_box(&bus), n, &mut out_simd);
                black_box(&out_simd);
            });
        });
    }
    f32_group.finish();

    let mut f64_group = c.benchmark_group("interleave_f64_le");
    for n in BLOCK_SIZES {
        f64_group.throughput(Throughput::Bytes((BENCH_CHANNELS * n * 8) as u64));
        let bus = filled_bus(BENCH_CHANNELS, n);
        let mut out_scalar = Vec::with_capacity(BENCH_CHANNELS * n * 8);
        f64_group.bench_with_input(BenchmarkId::new("scalar", n), &n, |b, &n| {
            b.iter(|| {
                interleave_f64_le_scalar(black_box(&bus), n, &mut out_scalar);
                black_box(&out_scalar);
            });
        });
        let mut out_simd = Vec::with_capacity(BENCH_CHANNELS * n * 8);
        f64_group.bench_with_input(BenchmarkId::new("pulp_simd", n), &n, |b, &n| {
            b.iter(|| {
                interleave_f64_le_simd(arch, black_box(&bus), n, &mut out_simd);
                black_box(&out_simd);
            });
        });
    }
    f64_group.finish();
}

fn bench_render_scene(c: &mut Criterion) {
    let layout = SpeakerLayout::layout_7_1_4();
    let vbap = VbapRenderer::build(&layout).unwrap();
    let sr = 48_000u32;
    let total_frames = 48_000u64;

    let mut builder = SceneBuilder::new(sr, total_frames);
    for idx in 0..16u32 {
        let role = if idx == 0 {
            ObjectRole::Lfe
        } else {
            ObjectRole::Dynamic
        };
        let az0 = ((idx as f64) * 24.0) - 180.0;
        let az1 = az0 + 90.0;
        let el = if idx % 3 == 0 { 35.0 } else { 0.0 };
        let pos0 = if role == ObjectRole::Lfe {
            Vec3::ZERO
        } else {
            Vec3::from_spherical(SphericalCoord::new(az0, el, 1.0))
        };
        let pos1 = if role == ObjectRole::Lfe {
            Vec3::ZERO
        } else {
            Vec3::from_spherical(SphericalCoord::new(az1, el, 1.0))
        };
        let mut obj = AudioObject::static_mono(
            ObjectId(idx + 1),
            format!("obj_{idx}"),
            role,
            pos0,
            0.5,
            Box::new(SineSource::new(
                sr,
                220.0 + (idx as f64) * 55.0,
                0.3,
                total_frames,
            )),
        );
        if role == ObjectRole::Dynamic {
            obj.keyframes = vec![
                Keyframe {
                    frame: 0,
                    position: pos0,
                    gain: 0.5,
                    spatial: SpatialParams::default(),
                    interp: InterpMode::Spherical,
                    discontinuous: false,
                },
                Keyframe {
                    frame: total_frames - 1,
                    position: pos1,
                    gain: 0.5,
                    spatial: SpatialParams::default(),
                    interp: InterpMode::Step,
                    discontinuous: false,
                },
            ];
        }
        builder = builder.add_object(obj);
    }
    let mut scene = builder.build().unwrap();

    let mut group = c.benchmark_group("render_scene_7_1_4_16obj");
    group.throughput(Throughput::Elements(total_frames));
    group.bench_function("1s_48khz_null_sink", |b| {
        b.iter(|| {
            let report = render_scene(
                &mut scene,
                &layout,
                &vbap,
                RenderConfig::default(),
                Box::new(NullWriter::default()),
            )
            .unwrap();
            black_box(report.frames_rendered);
        });
    });
    group.finish();
}

criterion_group!(
    benches,
    bench_accumulate,
    bench_metering,
    bench_interleave,
    bench_render_scene
);
criterion_main!(benches);
