use approx::assert_relative_eq;
use proptest::prelude::*;
use pulp::Arch;

use objrender::io::PcmBus;
use objrender::io::interleave::{
    interleave_f32_le_scalar, interleave_f32_le_simd, interleave_f64_le_scalar,
    interleave_f64_le_simd,
};
use objrender::mixer::ChannelStats;
use objrender::mixer::simd::{
    accumulate_linear_ramp_scalar, accumulate_linear_ramp_simd, scale_and_meter_bus_scalar,
    scale_and_meter_bus_simd,
};

fn make_stats(n_ch: usize) -> Vec<ChannelStats> {
    (0..n_ch)
        .map(|i| ChannelStats {
            speaker_label: format!("CH{i}"),
            peak_linear: 0.0,
            sum_squares: 0.0,
            clipped_samples: 0,
        })
        .collect()
}

fn fill_patterned_bus(n_ch: usize, n_frames: usize) -> PcmBus {
    let mut bus = PcmBus::new(n_ch, n_frames.max(1));
    for ch in 0..n_ch {
        for s in 0..n_frames {
            bus.channel_mut(ch)[s] = ((s as f64) * 0.137 + (ch as f64) * 0.41).sin() * 1.35;
        }
    }
    bus
}

#[test]
fn simd_gain_accumulation_matches_scalar_across_all_edge_lengths() {
    let arch = Arch::new();
    let lengths = [0, 1, 2, 3, 4, 5, 7, 8, 9, 15, 16, 17, 63, 64, 65, 128, 257];
    let n_ch = 12usize;

    let gains0 = [
        0.0, 0.5, 0.25, 0.0, 0.7, 0.1, 0.0, 0.35, 0.85, 0.0, 0.4, 0.15,
    ];
    let gains1 = [0.0, 0.5, 0.8, 0.3, 0.2, 0.1, 0.0, 0.95, 0.15, 0.6, 0.4, 0.0];

    for &n_frames in &lengths {
        let mut bus_scalar = fill_patterned_bus(n_ch, n_frames);
        let mut bus_simd = bus_scalar.clone();
        let mono: Vec<f64> = (0..n_frames)
            .map(|i| (i as f64 * 0.23).sin() + 0.15 * (i as f64 * 0.71).cos())
            .collect();

        accumulate_linear_ramp_scalar(&mono, &gains0, &gains1, &mut bus_scalar, n_frames);
        accumulate_linear_ramp_simd(arch, &mono, &gains0, &gains1, &mut bus_simd, n_frames);

        for ch in 0..n_ch {
            for s in 0..n_frames {
                assert_relative_eq!(
                    bus_simd.channel(ch)[s],
                    bus_scalar.channel(ch)[s],
                    epsilon = 1e-13
                );
            }
        }
    }
}

#[test]
fn simd_scale_and_meter_matches_scalar_including_clipping_and_unity_scale() {
    let arch = Arch::new();
    let lengths = [0, 1, 3, 4, 7, 8, 15, 63, 64, 65, 130];
    let n_ch = 12usize;

    for output_scale in [1.0, 0.75, 2.0] {
        for &n_frames in &lengths {
            let mut bus_scalar = fill_patterned_bus(n_ch, n_frames);
            let mut bus_simd = bus_scalar.clone();
            let mut stats_scalar = make_stats(n_ch);
            let mut stats_simd = make_stats(n_ch);

            scale_and_meter_bus_scalar(&mut bus_scalar, n_frames, output_scale, &mut stats_scalar);
            scale_and_meter_bus_simd(arch, &mut bus_simd, n_frames, output_scale, &mut stats_simd);

            for ch in 0..n_ch {
                for s in 0..n_frames {
                    assert_relative_eq!(
                        bus_simd.channel(ch)[s],
                        bus_scalar.channel(ch)[s],
                        epsilon = 1e-14
                    );
                }
                assert_relative_eq!(
                    stats_simd[ch].peak_linear,
                    stats_scalar[ch].peak_linear,
                    epsilon = 1e-14
                );
                assert_relative_eq!(
                    stats_simd[ch].sum_squares,
                    stats_scalar[ch].sum_squares,
                    epsilon = 1e-12
                );
                assert_eq!(
                    stats_simd[ch].clipped_samples,
                    stats_scalar[ch].clipped_samples
                );
            }
        }
    }
}

#[test]
fn simd_interleave_f32_and_f64_are_bit_exact_against_scalar() {
    let arch = Arch::new();
    let ch_counts = [1, 2, 6, 12, 16, 20];
    let frame_counts = [0, 1, 3, 7, 8, 9, 15, 16, 17, 64, 65];

    for &n_ch in &ch_counts {
        for &n_frames in &frame_counts {
            let bus = fill_patterned_bus(n_ch, n_frames);

            let mut out_f32_scalar = Vec::new();
            let mut out_f32_simd = Vec::new();
            interleave_f32_le_scalar(&bus, n_frames, &mut out_f32_scalar);
            interleave_f32_le_simd(arch, &bus, n_frames, &mut out_f32_simd);
            assert_eq!(
                out_f32_simd, out_f32_scalar,
                "f32 mismatch at n_ch={n_ch}, n_frames={n_frames}"
            );

            let mut out_f64_scalar = Vec::new();
            let mut out_f64_simd = Vec::new();
            interleave_f64_le_scalar(&bus, n_frames, &mut out_f64_scalar);
            interleave_f64_le_simd(arch, &bus, n_frames, &mut out_f64_simd);
            assert_eq!(
                out_f64_simd, out_f64_scalar,
                "f64 mismatch at n_ch={n_ch}, n_frames={n_frames}"
            );
        }
    }
}

proptest! {
    #[test]
    fn proptest_simd_kernels_match_scalar(
        n_frames in 1_usize..=130,
        n_ch in 1_usize..=18,
        output_scale in prop::sample::select(vec![1.0_f64, 0.5, 1.8]),
        seed in 0_u64..1000,
    ) {
        let arch = Arch::new();
        let next = |idx: usize| -> f64 {
            let x = ((idx as u64).wrapping_add(seed) as f64) * 0.37123;
            x.sin() * 2.0
        };

        let mut gains0 = vec![0.0_f64; n_ch];
        let mut gains1 = vec![0.0_f64; n_ch];
        for ch in 0..n_ch {
            if ch % 5 == 0 {
                continue;
            }
            gains0[ch] = next(ch * 3).abs();
            gains1[ch] = if ch % 3 == 0 {
                gains0[ch]
            } else {
                next(ch * 3 + 1).abs()
            };
        }

        let mono: Vec<f64> = (0..n_frames).map(|s| next(100 + s)).collect();
        let mut bus_scalar = PcmBus::new(n_ch, n_frames);
        for ch in 0..n_ch {
            for s in 0..n_frames {
                bus_scalar.channel_mut(ch)[s] = next(500 + ch * 131 + s) * 0.25;
            }
        }
        let mut bus_simd = bus_scalar.clone();

        accumulate_linear_ramp_scalar(&mono, &gains0, &gains1, &mut bus_scalar, n_frames);
        accumulate_linear_ramp_simd(arch, &mono, &gains0, &gains1, &mut bus_simd, n_frames);
        for ch in 0..n_ch {
            for s in 0..n_frames {
                prop_assert!((bus_simd.channel(ch)[s] - bus_scalar.channel(ch)[s]).abs() < 1e-12);
            }
        }

        let mut stats_scalar = make_stats(n_ch);
        let mut stats_simd = make_stats(n_ch);
        scale_and_meter_bus_scalar(&mut bus_scalar, n_frames, output_scale, &mut stats_scalar);
        scale_and_meter_bus_simd(arch, &mut bus_simd, n_frames, output_scale, &mut stats_simd);
        for ch in 0..n_ch {
            prop_assert!((stats_simd[ch].peak_linear - stats_scalar[ch].peak_linear).abs() < 1e-12);
            prop_assert!((stats_simd[ch].sum_squares - stats_scalar[ch].sum_squares).abs() < 1e-10);
            prop_assert_eq!(stats_simd[ch].clipped_samples, stats_scalar[ch].clipped_samples);
        }

        let mut out_f32_s = Vec::new();
        let mut out_f32_v = Vec::new();
        interleave_f32_le_scalar(&bus_scalar, n_frames, &mut out_f32_s);
        interleave_f32_le_simd(arch, &bus_scalar, n_frames, &mut out_f32_v);
        prop_assert_eq!(out_f32_v, out_f32_s);

        let mut out_f64_s = Vec::new();
        let mut out_f64_v = Vec::new();
        interleave_f64_le_scalar(&bus_scalar, n_frames, &mut out_f64_s);
        interleave_f64_le_simd(arch, &bus_scalar, n_frames, &mut out_f64_v);
        prop_assert_eq!(out_f64_v, out_f64_s);
    }
}
