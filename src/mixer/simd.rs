//! Portable runtime-dispatched SIMD kernels for mixing accumulation and bus metering.

use pulp::{Arch, Simd, WithSimd};

use crate::io::PcmBus;
use crate::mixer::ChannelStats;

/// Scalar reference implementation of linear gain-ramp accumulation onto a [`PcmBus`].
pub fn accumulate_linear_ramp_scalar(
    mono: &[f64],
    gains0: &[f64],
    gains1: &[f64],
    bus: &mut PcmBus,
    n_frames: usize,
) {
    if n_frames == 0 {
        return;
    }
    let src_slice = &mono[..n_frames];
    let inv_n = 1.0 / (n_frames as f64);
    for (out, (&g0, &g1)) in bus
        .channels_mut()
        .iter_mut()
        .zip(gains0.iter().zip(gains1.iter()))
    {
        if g0 == 0.0 && g1 == 0.0 {
            continue;
        }
        if (g1 - g0).abs() <= f64::EPSILON {
            for (dst, &src) in out[..n_frames].iter_mut().zip(src_slice) {
                *dst += src * g0;
            }
        } else {
            let dg = (g1 - g0) * inv_n;
            let mut g = g0;
            for (dst, &src) in out[..n_frames].iter_mut().zip(src_slice) {
                *dst += src * g;
                g += dg;
            }
        }
    }
}

/// Runtime-dispatched `pulp` SIMD implementation of linear gain-ramp accumulation.
pub fn accumulate_linear_ramp_simd(
    arch: Arch,
    mono: &[f64],
    gains0: &[f64],
    gains1: &[f64],
    bus: &mut PcmBus,
    n_frames: usize,
) {
    if n_frames == 0 {
        return;
    }
    arch.dispatch(AccumulateLinearRamp {
        mono: &mono[..n_frames],
        gains0,
        gains1,
        channels: bus.channels_mut(),
        n_frames,
    });
}

struct AccumulateLinearRamp<'a> {
    mono: &'a [f64],
    gains0: &'a [f64],
    gains1: &'a [f64],
    channels: &'a mut [Vec<f64>],
    n_frames: usize,
}

impl WithSimd for AccumulateLinearRamp<'_> {
    type Output = ();

    #[inline(always)]
    fn with_simd<S: Simd>(self, simd: S) {
        let lanes = S::F64_LANES;
        let (src_v, _) = S::as_simd_f64s(self.mono);
        let inv_n = 1.0 / (self.n_frames as f64);

        for (ch_buf, (&g0, &g1)) in self
            .channels
            .iter_mut()
            .zip(self.gains0.iter().zip(self.gains1.iter()))
        {
            if g0 == 0.0 && g1 == 0.0 {
                continue;
            }
            let dst = &mut ch_buf[..self.n_frames];
            let (dst_v, _) = S::as_mut_simd_f64s(dst);
            let common_vecs = src_v.len().min(dst_v.len());
            let simd_frames = common_vecs * lanes;

            if (g1 - g0).abs() <= f64::EPSILON {
                let g_vec = simd.splat_f64s(g0);
                for (d, &s) in dst_v[..common_vecs].iter_mut().zip(&src_v[..common_vecs]) {
                    *d = simd.mul_add_f64s(s, g_vec, *d);
                }
                for (d, &s) in dst[simd_frames..self.n_frames]
                    .iter_mut()
                    .zip(&self.mono[simd_frames..self.n_frames])
                {
                    *d += s * g0;
                }
            } else {
                let dg = (g1 - g0) * inv_n;
                let mut init = [0.0_f64; 16];
                debug_assert!(lanes <= init.len());
                for (lane, slot) in init[..lanes].iter_mut().enumerate() {
                    *slot = g0 + (lane as f64) * dg;
                }
                let mut g_vec = simd.partial_load_f64s(&init[..lanes]);
                let step_vec = simd.splat_f64s((lanes as f64) * dg);

                for (d, &s) in dst_v[..common_vecs].iter_mut().zip(&src_v[..common_vecs]) {
                    *d = simd.mul_add_f64s(s, g_vec, *d);
                    g_vec = simd.add_f64s(g_vec, step_vec);
                }

                let mut g = g0 + (simd_frames as f64) * dg;
                for (d, &s) in dst[simd_frames..self.n_frames]
                    .iter_mut()
                    .zip(&self.mono[simd_frames..self.n_frames])
                {
                    *d += s * g;
                    g += dg;
                }
            }
        }
    }
}

/// Scalar reference implementation of uniform output scaling and channel metering.
pub fn scale_and_meter_bus_scalar(
    bus: &mut PcmBus,
    n_frames: usize,
    output_scale: f64,
    stats: &mut [ChannelStats],
) {
    if n_frames == 0 {
        return;
    }
    let needs_scale = (output_scale - 1.0).abs() > f64::EPSILON;
    for (ch_buf, ch_stat) in bus.channels_mut().iter_mut().zip(stats.iter_mut()) {
        let slice = &mut ch_buf[..n_frames];
        if needs_scale {
            for sample in slice.iter_mut() {
                *sample *= output_scale;
            }
        }
        for &sample in slice.iter() {
            let abs = sample.abs();
            if abs > ch_stat.peak_linear {
                ch_stat.peak_linear = abs;
            }
            ch_stat.sum_squares += sample * sample;
            if abs > 1.0 {
                ch_stat.clipped_samples += 1;
            }
        }
    }
}

/// Runtime-dispatched `pulp` SIMD fused output scaling and peak/RMS/clipping metering.
pub fn scale_and_meter_bus_simd(
    arch: Arch,
    bus: &mut PcmBus,
    n_frames: usize,
    output_scale: f64,
    stats: &mut [ChannelStats],
) {
    if n_frames == 0 {
        return;
    }
    arch.dispatch(ScaleAndMeterBus {
        channels: bus.channels_mut(),
        n_frames,
        output_scale,
        stats,
    });
}

struct ScaleAndMeterBus<'a> {
    channels: &'a mut [Vec<f64>],
    n_frames: usize,
    output_scale: f64,
    stats: &'a mut [ChannelStats],
}

impl WithSimd for ScaleAndMeterBus<'_> {
    type Output = ();

    #[inline(always)]
    fn with_simd<S: Simd>(self, simd: S) {
        let needs_scale = (self.output_scale - 1.0).abs() > f64::EPSILON;
        let scale_v = simd.splat_f64s(self.output_scale);
        let zero_v = simd.splat_f64s(0.0);

        for (ch_buf, ch_stat) in self.channels.iter_mut().zip(self.stats.iter_mut()) {
            let slice = &mut ch_buf[..self.n_frames];
            let mut block_peak: f64;
            let mut block_sq: f64;

            {
                let (v_slice, tail) = S::as_mut_simd_f64s(slice);
                let mut peak_v = zero_v;
                let mut sq_v = zero_v;

                if needs_scale {
                    for slot in v_slice.iter_mut() {
                        let s = simd.mul_f64s(*slot, scale_v);
                        *slot = s;
                        peak_v = simd.max_f64s(peak_v, simd.abs_f64s(s));
                        sq_v = simd.mul_add_f64s(s, s, sq_v);
                    }
                } else {
                    for &s in v_slice.iter() {
                        peak_v = simd.max_f64s(peak_v, simd.abs_f64s(s));
                        sq_v = simd.mul_add_f64s(s, s, sq_v);
                    }
                }

                block_peak = simd.reduce_max_f64s(peak_v);
                block_sq = simd.reduce_sum_f64s(sq_v);

                if needs_scale {
                    for sample in tail.iter_mut() {
                        let s = *sample * self.output_scale;
                        *sample = s;
                        let abs = s.abs();
                        if abs > block_peak {
                            block_peak = abs;
                        }
                        block_sq += s * s;
                    }
                } else {
                    for &s in tail.iter() {
                        let abs = s.abs();
                        if abs > block_peak {
                            block_peak = abs;
                        }
                        block_sq += s * s;
                    }
                }
            }

            if block_peak > ch_stat.peak_linear {
                ch_stat.peak_linear = block_peak;
            }
            ch_stat.sum_squares += block_sq;
            if block_peak > 1.0 {
                ch_stat.clipped_samples += slice.iter().filter(|s| s.abs() > 1.0).count() as u64;
            }
        }
    }
}
