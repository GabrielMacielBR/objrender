//! Offline multi-object mixing, timeline ramping, LFE routing, and render orchestration.

pub mod simd;

use std::f64::consts::PI;

use thiserror::Error;

use crate::io::{IoError, NullWriter, OutputMetadata, PcmBus, SampleWriter};
use crate::layout::{SpeakerLayout, SpeakerRole};
use crate::renderer::Spatializer;
use crate::scene::{ObjectRole, Scene};
use crate::timeline;

#[derive(Debug, Clone, Copy, PartialEq)]
pub enum ClippingPolicy {
    /// Writes raw `f64` sum without attenuation; tracks peaks above `0 dBFS`.
    PassThrough,
    /// Applies static attenuation of `headroom_db` dB (e.g., `6.0` scales by `-6 dB`).
    FixedHeadroomDb(f64),
    /// Renders an analysis pass to measure global peak, then scales all channels
    /// uniformly so the highest peak reaches `target_dbfs` (must be finite and `<= 0.0`).
    TwoPassPeakNormalize { target_dbfs: f64 },
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub enum LfePolicy {
    /// Drops LFE-role objects if the target layout has no `SpeakerRole::Lfe` speaker.
    DropIfAbsent,
    /// Folds LFE-role objects into full-range speakers at `gain_db` if LFE is absent.
    FoldDownIfAbsent { gain_db: f64 },
}

#[derive(Debug, Clone, Copy)]
pub struct RenderConfig {
    pub block_size: usize,
    /// Length of raised-cosine (Hann) anti-click crossfade on discontinuous keyframes (`0` disables).
    pub crossfade_frames: usize,
    pub clipping: ClippingPolicy,
    pub lfe_policy: LfePolicy,
}

impl Default for RenderConfig {
    fn default() -> Self {
        Self {
            block_size: 64,
            crossfade_frames: 128,
            clipping: ClippingPolicy::PassThrough,
            lfe_policy: LfePolicy::DropIfAbsent,
        }
    }
}

#[derive(Debug, Clone)]
pub struct ChannelStats {
    pub speaker_label: String,
    pub peak_linear: f64,
    pub sum_squares: f64,
    pub clipped_samples: u64,
}

impl ChannelStats {
    pub fn peak_dbfs(&self) -> f64 {
        if self.peak_linear <= 0.0 {
            f64::NEG_INFINITY
        } else {
            20.0 * self.peak_linear.log10()
        }
    }

    pub fn rms_dbfs(&self, total_frames: u64) -> f64 {
        if total_frames == 0 || self.sum_squares <= 0.0 {
            f64::NEG_INFINITY
        } else {
            let rms = (self.sum_squares / (total_frames as f64)).sqrt();
            20.0 * rms.log10()
        }
    }
}

#[derive(Debug, Clone)]
pub struct RenderReport {
    pub frames_rendered: u64,
    pub rf64_promoted: bool,
    pub applied_gain_linear: f64,
    pub output_metadata: OutputMetadata,
    pub channels: Vec<ChannelStats>,
}

impl RenderReport {
    pub fn applied_gain_db(&self) -> f64 {
        if self.applied_gain_linear <= 0.0 {
            f64::NEG_INFINITY
        } else {
            20.0 * self.applied_gain_linear.log10()
        }
    }
}

#[derive(Debug, Error)]
pub enum RenderError {
    #[error("invalid render configuration: {0}")]
    InvalidConfig(String),
    #[error("I/O error during render: {0}")]
    Io(#[from] IoError),
}

struct ObjectRenderState {
    prev_gains: Vec<f64>,
    xfade_from: Vec<f64>,
    xfade_total: usize,
    xfade_remaining: usize,
    initialized: bool,
}

impl ObjectRenderState {
    fn new(n_spk: usize) -> Self {
        Self {
            prev_gains: vec![0.0; n_spk],
            xfade_from: vec![0.0; n_spk],
            xfade_total: 0,
            xfade_remaining: 0,
            initialized: false,
        }
    }
}

/// Renders all objects in `scene` onto `layout` using `spatializer`, writing blocks to `writer`.
pub fn render_scene(
    scene: &mut Scene,
    layout: &SpeakerLayout,
    spatializer: &dyn Spatializer,
    config: RenderConfig,
    writer: Box<dyn SampleWriter>,
) -> Result<RenderReport, RenderError> {
    if config.block_size == 0 {
        return Err(RenderError::InvalidConfig(
            "block_size must be greater than zero".to_string(),
        ));
    }
    let n_spk = layout.speakers.len();
    if spatializer.num_speakers() != n_spk {
        return Err(RenderError::InvalidConfig(format!(
            "spatializer expects {} speakers, layout has {n_spk}",
            spatializer.num_speakers()
        )));
    }
    if let LfePolicy::FoldDownIfAbsent { gain_db } = config.lfe_policy
        && !gain_db.is_finite()
    {
        return Err(RenderError::InvalidConfig(
            "FoldDownIfAbsent gain_db must be finite".to_string(),
        ));
    }

    match config.clipping {
        ClippingPolicy::PassThrough => {
            run_render_pass(scene, layout, spatializer, config, 1.0, writer)
        }
        ClippingPolicy::FixedHeadroomDb(db) => {
            if !db.is_finite() || db < 0.0 {
                return Err(RenderError::InvalidConfig(
                    "FixedHeadroomDb must be finite and >= 0.0".to_string(),
                ));
            }
            let scale = 10.0_f64.powf(-db / 20.0);
            run_render_pass(scene, layout, spatializer, config, scale, writer)
        }
        ClippingPolicy::TwoPassPeakNormalize { target_dbfs } => {
            if !target_dbfs.is_finite() || target_dbfs > 0.0 {
                return Err(RenderError::InvalidConfig(
                    "TwoPassPeakNormalize target_dbfs must be finite and <= 0.0".to_string(),
                ));
            }
            let analysis = run_render_pass(
                scene,
                layout,
                spatializer,
                config,
                1.0,
                Box::new(NullWriter::default()),
            )?;
            let max_peak = analysis
                .channels
                .iter()
                .map(|c| c.peak_linear)
                .fold(0.0_f64, f64::max);
            let norm_scale = if max_peak <= 1e-12 {
                1.0
            } else {
                10.0_f64.powf(target_dbfs / 20.0) / max_peak
            };
            run_render_pass(scene, layout, spatializer, config, norm_scale, writer)
        }
    }
}

fn run_render_pass(
    scene: &mut Scene,
    layout: &SpeakerLayout,
    spatializer: &dyn Spatializer,
    config: RenderConfig,
    output_scale: f64,
    mut writer: Box<dyn SampleWriter>,
) -> Result<RenderReport, RenderError> {
    let n_spk = layout.speakers.len();
    let lfe_channels: Vec<usize> = layout
        .speakers
        .iter()
        .enumerate()
        .filter(|(_, s)| s.role == SpeakerRole::Lfe)
        .map(|(i, _)| i)
        .collect();
    let full_range_channels: Vec<usize> = layout
        .speakers
        .iter()
        .enumerate()
        .filter(|(_, s)| s.role == SpeakerRole::FullRange)
        .map(|(i, _)| i)
        .collect();

    let arch = pulp::Arch::new();
    let mut bus = PcmBus::new(n_spk, config.block_size);
    let mut mono_buf = vec![0.0_f64; config.block_size];
    let mut gains0 = vec![0.0_f64; n_spk];
    let mut gains1 = vec![0.0_f64; n_spk];
    let mut obj_states: Vec<ObjectRenderState> = scene
        .objects
        .iter()
        .map(|_| ObjectRenderState::new(n_spk))
        .collect();

    let mut stats: Vec<ChannelStats> = layout
        .speakers
        .iter()
        .map(|s| ChannelStats {
            speaker_label: s.label.clone(),
            peak_linear: 0.0,
            sum_squares: 0.0,
            clipped_samples: 0,
        })
        .collect();

    let mut frame_start: u64 = 0;
    while frame_start < scene.total_frames {
        let mut block_end = frame_start
            .saturating_add(config.block_size as u64)
            .min(scene.total_frames);

        // Split sub-blocks at discontinuous keyframe boundaries so teleports never smear early.
        for obj in &scene.objects {
            if let Some(disc_frame) = timeline::next_discontinuity_in(obj, frame_start, block_end) {
                block_end = block_end.min(disc_frame);
            }
        }

        let n_frames = (block_end - frame_start) as usize;
        bus.clear(n_frames);

        for (obj, obj_state) in scene.objects.iter_mut().zip(obj_states.iter_mut()) {
            if frame_start >= obj.end_frame() || block_end <= obj.start_frame {
                obj_state.initialized = false;
                obj_state.xfade_remaining = 0;
                continue;
            }

            let (state0, state1) = timeline::eval_interval(obj, frame_start, block_end);
            let is_disc = timeline::is_discontinuous_at(obj, frame_start);

            if state0.gain == 0.0
                && state1.gain == 0.0
                && obj_state.xfade_remaining == 0
                && !is_disc
            {
                obj_state.prev_gains.fill(0.0);
                obj_state.initialized = true;
                continue;
            }

            match obj.role {
                ObjectRole::Lfe => {
                    compute_lfe_gains(
                        state0.gain,
                        &lfe_channels,
                        &full_range_channels,
                        config.lfe_policy,
                        &mut gains0,
                    );
                    if (state0.gain - state1.gain).abs() <= f64::EPSILON {
                        gains1.copy_from_slice(&gains0);
                    } else {
                        compute_lfe_gains(
                            state1.gain,
                            &lfe_channels,
                            &full_range_channels,
                            config.lfe_policy,
                            &mut gains1,
                        );
                    }
                }
                ObjectRole::Dynamic | ObjectRole::Bed => {
                    spatializer.compute_gains(state0.position, state0.spatial, &mut gains0);
                    scale_gains(&mut gains0, state0.gain);
                    if state0 == state1 {
                        gains1.copy_from_slice(&gains0);
                    } else {
                        spatializer.compute_gains(state1.position, state1.spatial, &mut gains1);
                        scale_gains(&mut gains1, state1.gain);
                    }
                }
            }

            if obj_state.initialized && config.crossfade_frames > 0 && is_disc {
                obj_state.xfade_from.copy_from_slice(&obj_state.prev_gains);
                obj_state.xfade_total = config.crossfade_frames;
                obj_state.xfade_remaining = config.crossfade_frames;
            }

            let local_offset = frame_start.saturating_sub(obj.start_frame);
            obj.source
                .read_frames(local_offset, &mut mono_buf[..n_frames])?;

            if obj_state.xfade_remaining == 0 {
                simd::accumulate_linear_ramp_simd(
                    arch,
                    &mono_buf[..n_frames],
                    &gains0,
                    &gains1,
                    &mut bus,
                    n_frames,
                );
            } else {
                accumulate_with_crossfade(
                    &mono_buf[..n_frames],
                    &gains0,
                    &gains1,
                    obj_state,
                    &mut bus,
                    n_frames,
                );
            }

            obj_state.prev_gains.copy_from_slice(&gains1);
            obj_state.initialized = true;
        }

        simd::scale_and_meter_bus_simd(arch, &mut bus, n_frames, output_scale, &mut stats);

        writer.write_block(&bus, n_frames)?;
        frame_start = block_end;
    }

    let output_metadata = writer.finalize()?;
    Ok(RenderReport {
        frames_rendered: scene.total_frames,
        rf64_promoted: output_metadata.rf64_promoted,
        applied_gain_linear: output_scale,
        output_metadata,
        channels: stats,
    })
}

fn accumulate_with_crossfade(
    mono: &[f64],
    gains0: &[f64],
    gains1: &[f64],
    obj_state: &mut ObjectRenderState,
    bus: &mut PcmBus,
    n_frames: usize,
) {
    let inv_n = 1.0 / (n_frames as f64);
    let xfade_len = obj_state.xfade_remaining.min(n_frames);
    let elapsed_base = obj_state.xfade_total - obj_state.xfade_remaining;
    let total_f = obj_state.xfade_total as f64;

    for ch in 0..gains0.len() {
        let g0 = gains0[ch];
        let g1 = gains1[ch];
        let g_old = obj_state.xfade_from[ch];
        if g0 == 0.0 && g1 == 0.0 && g_old == 0.0 {
            continue;
        }
        let dg = (g1 - g0) * inv_n;
        let out = bus.channel_mut(ch);

        for (s, &src) in mono[..xfade_len].iter().enumerate() {
            let p = ((elapsed_base + s + 1) as f64 / total_f).clamp(0.0, 1.0);
            let alpha = 0.5 * (1.0 - (PI * p).cos());
            let g_target = g0 + (s as f64) * dg;
            let g = g_old * (1.0 - alpha) + g_target * alpha;
            out[s] += src * g;
        }

        if xfade_len < n_frames {
            let mut g = g0 + (xfade_len as f64) * dg;
            for (dst, &src) in out[xfade_len..n_frames]
                .iter_mut()
                .zip(&mono[xfade_len..n_frames])
            {
                *dst += src * g;
                g += dg;
            }
        }
    }

    obj_state.xfade_remaining = obj_state.xfade_remaining.saturating_sub(n_frames);
}

#[inline]
fn scale_gains(gains: &mut [f64], scale: f64) {
    if (scale - 1.0).abs() > f64::EPSILON {
        for g in gains {
            *g *= scale;
        }
    }
}

fn compute_lfe_gains(
    obj_gain: f64,
    lfe_channels: &[usize],
    full_range_channels: &[usize],
    policy: LfePolicy,
    out_gains: &mut [f64],
) {
    out_gains.fill(0.0);
    if !lfe_channels.is_empty() {
        let g = obj_gain / (lfe_channels.len() as f64).sqrt();
        for &ch in lfe_channels {
            out_gains[ch] = g;
        }
        return;
    }

    if let LfePolicy::FoldDownIfAbsent { gain_db } = policy
        && !full_range_channels.is_empty()
    {
        let scale = 10.0_f64.powf(gain_db / 20.0);
        let g = (obj_gain * scale) / (full_range_channels.len() as f64).sqrt();
        for &ch in full_range_channels {
            out_gains[ch] = g;
        }
    }
}
