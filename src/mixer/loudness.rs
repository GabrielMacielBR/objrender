//! ITU-R BS.1770-4 / EBU R128 integrated loudness (LUFS) and 4x true-peak (dBTP) metering.

use std::f64::consts::PI;
use std::sync::{Arc, Mutex};

use crate::io::{IoError, OutputMetadata, PcmBus, SampleWriter};
use crate::layout::{Speaker, SpeakerLayout, SpeakerRole};

const ABSOLUTE_GATE_LUFS: f64 = -70.0;
const RELATIVE_GATE_OFFSET_LU: f64 = 10.0;
const TP_PHASES: usize = 4;
const TP_TAPS_PER_PHASE: usize = 12;

/// Final integrated loudness and 4x oversampled true-peak measurement.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct LoudnessMeasurement {
    pub integrated_lufs: f64,
    pub true_peak_linear: f64,
}

impl LoudnessMeasurement {
    #[inline]
    pub fn true_peak_dbtp(&self) -> f64 {
        if self.true_peak_linear <= 0.0 {
            f64::NEG_INFINITY
        } else {
            20.0 * self.true_peak_linear.log10()
        }
    }
}

#[derive(Debug, Clone, Copy)]
struct BiquadCoeffs {
    b0: f64,
    b1: f64,
    b2: f64,
    a1: f64,
    a2: f64,
}

#[derive(Debug, Clone, Copy, Default)]
struct BiquadState {
    z1: f64,
    z2: f64,
}

impl BiquadState {
    #[inline]
    fn process(&mut self, x: f64, c: &BiquadCoeffs) -> f64 {
        let y = c.b0 * x + self.z1;
        self.z1 = c.b1 * x - c.a1 * y + self.z2;
        self.z2 = c.b2 * x - c.a2 * y;
        y
    }
}

#[derive(Debug, Clone)]
struct KWeightFilter {
    stage1_c: BiquadCoeffs,
    stage2_c: BiquadCoeffs,
    stage1_s: Vec<BiquadState>,
    stage2_s: Vec<BiquadState>,
}

impl KWeightFilter {
    fn new(sample_rate: u32, num_channels: usize) -> Self {
        let (stage1_c, stage2_c) = k_weight_coeffs(sample_rate);
        Self {
            stage1_c,
            stage2_c,
            stage1_s: vec![BiquadState::default(); num_channels],
            stage2_s: vec![BiquadState::default(); num_channels],
        }
    }

    #[inline]
    fn process_sample(&mut self, ch: usize, x: f64) -> f64 {
        let s1 = self.stage1_s[ch].process(x, &self.stage1_c);
        self.stage2_s[ch].process(s1, &self.stage2_c)
    }
}

fn k_weight_coeffs(sample_rate: u32) -> (BiquadCoeffs, BiquadCoeffs) {
    if sample_rate == 48_000 {
        // Exact ITU-R BS.1770-4 Table 1 and Table 2 coefficients for 48 kHz.
        let stage1 = BiquadCoeffs {
            b0: 1.535_124_859_586_97,
            b1: -2.691_696_189_406_38,
            b2: 1.198_392_810_852_85,
            a1: -1.690_659_293_182_41,
            a2: 0.732_480_774_215_85,
        };
        let stage2 = BiquadCoeffs {
            b0: 1.0,
            b1: -2.0,
            b2: 1.0,
            a1: -1.990_047_454_833_98,
            a2: 0.990_072_250_366_21,
        };
        return (stage1, stage2);
    }

    let fs = (sample_rate.max(1)) as f64;
    // Stage 1: pre-filter high shelf (Brecht De Man / ITU-R BS.1770-4 continuous-time prototype).
    let f0_1 = 1_681.974_450_955_533;
    let g_db = 3.999_843_853_973_347;
    let q_1 = 0.707_175_236_955_419_6;
    let k1 = (PI * f0_1 / fs).tan();
    let vh = 10.0_f64.powf(g_db / 20.0);
    let vb = vh.powf(0.499_666_774_154_541_6);
    let a0_1 = 1.0 + k1 / q_1 + k1 * k1;
    let stage1 = BiquadCoeffs {
        b0: (vh + vb * k1 / q_1 + k1 * k1) / a0_1,
        b1: 2.0 * (k1 * k1 - vh) / a0_1,
        b2: (vh - vb * k1 / q_1 + k1 * k1) / a0_1,
        a1: 2.0 * (k1 * k1 - 1.0) / a0_1,
        a2: (1.0 - k1 / q_1 + k1 * k1) / a0_1,
    };

    // Stage 2: RLB high-pass scaled to match BS.1770-4's 48 kHz gain normalization.
    let f0_2 = 38.135_470_876_024_44;
    let q_2 = 0.500_327_037_323_877_3;
    let k2 = (PI * f0_2 / fs).tan();
    let a0_2 = 1.0 + k2 / q_2 + k2 * k2;
    let a0_48k = 1.004_994_880_296_088;
    let g2 = a0_48k / a0_2;
    let stage2 = BiquadCoeffs {
        b0: g2,
        b1: -2.0 * g2,
        b2: g2,
        a1: 2.0 * (k2 * k2 - 1.0) / a0_2,
        a2: (1.0 - k2 / q_2 + k2 * k2) / a0_2,
    };
    (stage1, stage2)
}

#[derive(Debug, Clone)]
struct TruePeakFilter {
    phases: [[f64; TP_TAPS_PER_PHASE]; TP_PHASES],
    histories: Vec<[f64; TP_TAPS_PER_PHASE]>,
}

impl TruePeakFilter {
    fn new(num_channels: usize) -> Self {
        Self {
            phases: build_polyphase_4x_coeffs(),
            histories: vec![[0.0; TP_TAPS_PER_PHASE]; num_channels],
        }
    }

    #[inline]
    fn process_sample(&mut self, ch: usize, x: f64) -> f64 {
        let hist = &mut self.histories[ch];
        hist.copy_within(0..TP_TAPS_PER_PHASE - 1, 1);
        hist[0] = x;

        // Phase 0 evaluates the aligned integer center sample (tap 5).
        let mut max_abs = hist[5].abs();
        for coeffs in &self.phases[1..] {
            let mut sum = 0.0;
            for (&h, &c) in hist.iter().zip(coeffs.iter()) {
                sum += h * c;
            }
            let a = sum.abs();
            if a > max_abs {
                max_abs = a;
            }
        }
        max_abs
    }

    fn flush_channel_peak(&mut self, ch: usize) -> f64 {
        let mut peak = 0.0_f64;
        for _ in 0..5 {
            peak = peak.max(self.process_sample(ch, 0.0));
        }
        peak
    }
}

fn build_polyphase_4x_coeffs() -> [[f64; TP_TAPS_PER_PHASE]; TP_PHASES] {
    let mut phases = [[0.0_f64; TP_TAPS_PER_PHASE]; TP_PHASES];
    phases[0][5] = 1.0;

    let total_width = TP_TAPS_PER_PHASE as f64;
    for (p, coeffs) in phases.iter_mut().enumerate().skip(1) {
        let frac = (p as f64) / (TP_PHASES as f64);
        let mut sum = 0.0;
        for (k, slot) in coeffs.iter_mut().enumerate() {
            let u = (k as f64) - 5.0 - frac;
            let sinc = if u.abs() < 1e-12 {
                1.0
            } else {
                let pi_u = PI * u;
                pi_u.sin() / pi_u
            };
            let w_pos = (0.5 + u / total_width).clamp(0.0, 1.0);
            let t = 2.0 * PI * w_pos;
            let bh =
                0.35875 - 0.48829 * t.cos() + 0.14128 * (2.0 * t).cos() - 0.01168 * (3.0 * t).cos();
            let tap = sinc * bh;
            *slot = tap;
            sum += tap;
        }
        if sum.abs() > 1e-12 {
            for slot in coeffs.iter_mut() {
                *slot /= sum;
            }
        }
    }
    phases
}

/// Returns the ITU-R BS.1770-4 loudness channel weight `G_i` for a given speaker.
pub fn channel_loudness_weight(speaker: &Speaker) -> f64 {
    if speaker.role == SpeakerRole::Lfe {
        return 0.0;
    }
    let sph = speaker.position.to_spherical();
    let abs_az = sph.azimuth_deg.abs();
    // Ear-level side/rear surrounds (+-75 to +-145 deg azimuth, |elevation| < 30 deg) get +1.5 dB.
    if sph.elevation_deg.abs() < 30.0 && (75.0..=145.0).contains(&abs_az) {
        1.41
    } else {
        1.0
    }
}

/// Stateful ITU-R BS.1770-4 gated loudness & 4x true-peak meter for planar [`PcmBus`] streams.
#[derive(Debug, Clone)]
pub struct LoudnessMeter {
    weights: Vec<f64>,
    k_filter: KWeightFilter,
    tp_filter: TruePeakFilter,
    frame_energy: Vec<f64>,
    hop_frames: usize,
    hop_pos: usize,
    cur_hop_energy: f64,
    hop_energies: Vec<f64>,
    total_weighted_energy: f64,
    total_frames: u64,
    true_peak_linear: f64,
}

impl LoudnessMeter {
    pub fn new(sample_rate: u32, layout: &SpeakerLayout) -> Self {
        let weights: Vec<f64> = layout
            .speakers
            .iter()
            .map(channel_loudness_weight)
            .collect();
        let n_ch = weights.len();
        let hop_frames = (((sample_rate as f64) * 0.1).round() as usize).max(1);

        Self {
            weights,
            k_filter: KWeightFilter::new(sample_rate, n_ch),
            tp_filter: TruePeakFilter::new(n_ch),
            frame_energy: Vec::new(),
            hop_frames,
            hop_pos: 0,
            cur_hop_energy: 0.0,
            hop_energies: Vec::new(),
            total_weighted_energy: 0.0,
            total_frames: 0,
            true_peak_linear: 0.0,
        }
    }

    /// Feeds `frames` planar samples from `bus` sequentially into the loudness & true-peak filters.
    pub fn process_block(&mut self, bus: &PcmBus, frames: usize) {
        if frames == 0 {
            return;
        }
        let channels = bus.channels();
        let n_ch = self.weights.len().min(channels.len());
        self.frame_energy.resize(frames, 0.0);
        let energy_slice = &mut self.frame_energy[..frames];
        energy_slice.fill(0.0);

        for (ch, (ch_buf, &w)) in channels
            .iter()
            .zip(self.weights.iter())
            .take(n_ch)
            .enumerate()
        {
            for (&sample, e_slot) in ch_buf[..frames].iter().zip(energy_slice.iter_mut()) {
                let clean = if sample.is_finite() { sample } else { 0.0 };
                let tp = self.tp_filter.process_sample(ch, clean);
                if tp > self.true_peak_linear {
                    self.true_peak_linear = tp;
                }
                if w > 0.0 {
                    let kw = self.k_filter.process_sample(ch, clean);
                    *e_slot += w * kw * kw;
                }
            }
        }

        for &frame_weighted in &self.frame_energy[..frames] {
            self.cur_hop_energy += frame_weighted;
            self.total_weighted_energy += frame_weighted;
            self.total_frames += 1;
            self.hop_pos += 1;
            if self.hop_pos >= self.hop_frames {
                self.hop_energies.push(self.cur_hop_energy);
                self.cur_hop_energy = 0.0;
                self.hop_pos = 0;
            }
        }
    }

    /// Flushes trailing filter delay for accurate end-of-stream true-peak detection and computes
    /// the final ITU-R BS.1770-4 measurement.
    pub fn finalize_measurement(&mut self) -> LoudnessMeasurement {
        for ch in 0..self.weights.len() {
            let tp = self.tp_filter.flush_channel_peak(ch);
            if tp > self.true_peak_linear {
                self.true_peak_linear = tp;
            }
        }
        self.measure()
    }

    /// Computes gated integrated loudness (LUFS) and max 4x true-peak linear amplitude so far.
    pub fn measure(&self) -> LoudnessMeasurement {
        if self.total_frames == 0 {
            return LoudnessMeasurement {
                integrated_lufs: f64::NEG_INFINITY,
                true_peak_linear: self.true_peak_linear,
            };
        }

        if self.hop_energies.len() < 4 {
            let mean_pow = self.total_weighted_energy / (self.total_frames as f64);
            let lufs = power_to_lufs(mean_pow);
            return LoudnessMeasurement {
                integrated_lufs: if lufs > ABSOLUTE_GATE_LUFS {
                    lufs
                } else {
                    f64::NEG_INFINITY
                },
                true_peak_linear: self.true_peak_linear,
            };
        }

        let block_len = (4 * self.hop_frames) as f64;
        let mut block_powers = Vec::with_capacity(self.hop_energies.len() - 3);
        let mut above_abs_sum = 0.0;
        let mut above_abs_count = 0usize;

        for w in self.hop_energies.windows(4) {
            let mean_pow = (w[0] + w[1] + w[2] + w[3]) / block_len;
            let l_b = power_to_lufs(mean_pow);
            if l_b > ABSOLUTE_GATE_LUFS {
                above_abs_sum += mean_pow;
                above_abs_count += 1;
                block_powers.push((l_b, mean_pow));
            }
        }

        if above_abs_count == 0 {
            return LoudnessMeasurement {
                integrated_lufs: f64::NEG_INFINITY,
                true_peak_linear: self.true_peak_linear,
            };
        }

        let rel_threshold =
            power_to_lufs(above_abs_sum / (above_abs_count as f64)) - RELATIVE_GATE_OFFSET_LU;

        let mut gated_sum = 0.0;
        let mut gated_count = 0usize;
        for (l_b, mean_pow) in block_powers {
            if l_b > rel_threshold {
                gated_sum += mean_pow;
                gated_count += 1;
            }
        }

        let integrated_lufs = if gated_count == 0 {
            f64::NEG_INFINITY
        } else {
            power_to_lufs(gated_sum / (gated_count as f64))
        };

        LoudnessMeasurement {
            integrated_lufs,
            true_peak_linear: self.true_peak_linear,
        }
    }
}

#[inline]
fn power_to_lufs(power: f64) -> f64 {
    if power <= 0.0 {
        f64::NEG_INFINITY
    } else {
        -0.691 + 10.0 * power.log10()
    }
}

/// [`SampleWriter`] sink feeding sequential blocks into [`LoudnessMeter`] during Pass 1.
pub struct LoudnessAnalyzerWriter {
    meter: LoudnessMeter,
    frames_written: u64,
    out: Arc<Mutex<Option<LoudnessMeasurement>>>,
}

impl LoudnessAnalyzerWriter {
    pub fn pair(
        sample_rate: u32,
        layout: &SpeakerLayout,
    ) -> (
        Box<dyn SampleWriter>,
        Arc<Mutex<Option<LoudnessMeasurement>>>,
    ) {
        let out = Arc::new(Mutex::new(None));
        let writer = Self {
            meter: LoudnessMeter::new(sample_rate, layout),
            frames_written: 0,
            out: Arc::clone(&out),
        };
        (Box::new(writer), out)
    }
}

impl SampleWriter for LoudnessAnalyzerWriter {
    fn write_block(&mut self, bus: &PcmBus, frames: usize) -> Result<(), IoError> {
        self.meter.process_block(bus, frames);
        self.frames_written += frames as u64;
        Ok(())
    }

    fn finalize(mut self: Box<Self>) -> Result<OutputMetadata, IoError> {
        let measurement = self.meter.finalize_measurement();
        *self.out.lock().unwrap() = Some(measurement);
        Ok(OutputMetadata {
            rf64_promoted: false,
            frames_written: self.frames_written,
            total_bytes: 0,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use approx::assert_relative_eq;
    use std::f64::consts::TAU;

    fn make_bus_sine(
        num_channels: usize,
        active_channels: &[usize],
        sample_rate: u32,
        freq_hz: f64,
        peak_amp: f64,
        frames: usize,
    ) -> PcmBus {
        let mut bus = PcmBus::new(num_channels, frames);
        let step = TAU * freq_hz / (sample_rate as f64);
        for &ch in active_channels {
            let dst = bus.channel_mut(ch);
            for (i, sample) in dst.iter_mut().enumerate() {
                *sample = peak_amp * ((i as f64) * step).sin();
            }
        }
        bus
    }

    #[test]
    fn ebu_tech_3341_stereo_997hz_minus_23_dbfs_across_sample_rates() {
        let layout = SpeakerLayout::stereo();
        let peak_amp = 10.0_f64.powf(-23.0 / 20.0);

        for sr in [44_100u32, 48_000, 96_000] {
            let frames = (sr as usize) * 2;
            let bus = make_bus_sine(2, &[0, 1], sr, 997.0, peak_amp, frames);
            let mut meter = LoudnessMeter::new(sr, &layout);
            meter.process_block(&bus, frames);
            let res = meter.finalize_measurement();
            assert_relative_eq!(res.integrated_lufs, -23.0, epsilon = 0.05);
            assert_relative_eq!(res.true_peak_dbtp(), -23.0, epsilon = 0.05);
        }
    }

    #[test]
    fn surround_weighting_adds_1_5_db_and_lfe_is_excluded() {
        let layout = SpeakerLayout::surround_5_1();
        let sr = 48_000u32;
        let frames = sr as usize;
        let peak_amp = 10.0_f64.powf(-23.0 / 20.0);

        // Ch 3 is LFE -> must produce silence (-inf LUFS).
        let bus_lfe = make_bus_sine(6, &[3], sr, 80.0, 0.5, frames);
        let mut meter_lfe = LoudnessMeter::new(sr, &layout);
        meter_lfe.process_block(&bus_lfe, frames);
        assert!(
            meter_lfe
                .finalize_measurement()
                .integrated_lufs
                .is_infinite()
        );

        // Compare Ch 0 (L front, weight 1.0) vs Ch 4 (Ls surround at +110 deg, weight 1.41 -> +1.49 dB).
        let bus_front = make_bus_sine(6, &[0], sr, 997.0, peak_amp, frames);
        let bus_sur = make_bus_sine(6, &[4], sr, 997.0, peak_amp, frames);
        let mut meter_f = LoudnessMeter::new(sr, &layout);
        meter_f.process_block(&bus_front, frames);
        let mut meter_s = LoudnessMeter::new(sr, &layout);
        meter_s.process_block(&bus_sur, frames);
        let diff = meter_s.finalize_measurement().integrated_lufs
            - meter_f.finalize_measurement().integrated_lufs;
        assert_relative_eq!(diff, 10.0 * 1.41_f64.log10(), epsilon = 0.02);
    }

    #[test]
    fn gating_ignores_extended_silence() {
        let layout = SpeakerLayout::stereo();
        let sr = 48_000u32;
        let peak_amp = 10.0_f64.powf(-20.0 / 20.0);
        let active_frames = (sr as usize) * 2;
        let silent_frames = (sr as usize) * 10;

        let active_bus = make_bus_sine(2, &[0, 1], sr, 997.0, peak_amp, active_frames);
        let silent_bus = PcmBus::new(2, silent_frames);

        let mut meter = LoudnessMeter::new(sr, &layout);
        meter.process_block(&active_bus, active_frames);
        meter.process_block(&silent_bus, silent_frames);
        let res = meter.finalize_measurement();
        // 17 full active 400ms blocks + 3 transition blocks (0.75 + 0.50 + 0.25) = 18.5 / 20 -> -20.339 LUFS;
        // pure silence blocks are completely gated out rather than diluting to -27.8 LUFS.
        let expected_bs1770 = -20.0 + 10.0 * (18.5_f64 / 20.0).log10();
        assert_relative_eq!(res.integrated_lufs, expected_bs1770, epsilon = 0.03);
    }

    #[test]
    fn true_peak_4x_oversampler_catches_intersample_peak() {
        let layout = SpeakerLayout::stereo();
        let sr = 48_000u32;
        let frames = 4096usize;
        let mut bus = PcmBus::new(2, frames);

        // fs/4 sine shifted by +45 deg: sample peak = cos(45 deg) = 0.7071 (-3.01 dBFS),
        // true inter-sample peak = 1.0000 (0.00 dBTP).
        for i in 0..frames {
            let v = ((i as f64) * PI * 0.5 + PI * 0.25).sin();
            bus.channel_mut(0)[i] = v;
            bus.channel_mut(1)[i] = v;
        }

        let mut meter = LoudnessMeter::new(sr, &layout);
        meter.process_block(&bus, frames);
        let res = meter.finalize_measurement();
        assert_relative_eq!(res.true_peak_linear, 1.0, epsilon = 0.01);
        assert_relative_eq!(res.true_peak_dbtp(), 0.0, epsilon = 0.05);
    }
}
