//! Offline multi-object mixing, timeline ramping, LFE routing, and render orchestration.

pub mod loudness;
pub mod simd;

use std::f64::consts::PI;

use rayon::prelude::*;
use thiserror::Error;

pub use loudness::{LoudnessMeasurement, LoudnessMeter};

use crate::io::{IoError, NullWriter, OutputMetadata, PcmBus, SampleWriter};
use crate::layout::{SpeakerLayout, SpeakerRole};
use crate::mixer::loudness::LoudnessAnalyzerWriter;
use crate::renderer::Spatializer;
use crate::scene::{AudioObject, ObjectRole, Scene};
use crate::timeline::{self, ObjectState};

const DEFAULT_CHUNK_FRAMES: usize = 4096;

#[derive(Debug, Clone, Copy, PartialEq)]
pub enum ClippingPolicy {
    /// Writes raw `f64` sum without attenuation; tracks peaks above `0 dBFS`.
    PassThrough,
    /// Applies static attenuation of `headroom_db` dB (e.g., `6.0` scales by `-6 dB`).
    FixedHeadroomDb(f64),
    /// Renders an analysis pass to measure global peak, then scales all channels
    /// uniformly so the highest peak reaches `target_dbfs` (must be finite and `<= 0.0`).
    TwoPassPeakNormalize { target_dbfs: f64 },
    /// Renders an analysis pass to measure ITU-R BS.1770-4 integrated loudness and 4x true-peak,
    /// then scales uniformly to reach `target_lufs` capped by `true_peak_ceiling_dbtp`.
    TwoPassLoudnessNormalize {
        target_lufs: f64,
        true_peak_ceiling_dbtp: f64,
    },
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
    /// Worker thread count (`0` = automatic via Rayon global pool, `1` = single-threaded).
    pub threads: usize,
}

impl Default for RenderConfig {
    fn default() -> Self {
        Self {
            block_size: 64,
            crossfade_frames: 128,
            clipping: ClippingPolicy::PassThrough,
            lfe_policy: LfePolicy::DropIfAbsent,
            threads: 0,
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
    pub integrated_lufs: Option<f64>,
    pub true_peak_dbtp: Option<f64>,
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

    fn reset(&mut self) {
        self.prev_gains.fill(0.0);
        self.xfade_from.fill(0.0);
        self.xfade_total = 0;
        self.xfade_remaining = 0;
        self.initialized = false;
    }
}

struct RenderPassCtx<'a> {
    scene: &'a Scene,
    spatializer: &'a dyn Spatializer,
    config: RenderConfig,
    output_scale: f64,
    lfe_channels: Vec<usize>,
    full_range_channels: Vec<usize>,
    arch: pulp::Arch,
}

struct ChunkScratch {
    chunk_mono: Vec<f64>,
    sub_mono: Vec<f64>,
    gains0: Vec<f64>,
    gains1: Vec<f64>,
    obj_state: ObjectRenderState,
    sub_blocks: Vec<(u64, u64)>,
}

impl ChunkScratch {
    fn new(n_spk: usize, chunk_frames: usize, block_size: usize) -> Self {
        Self {
            chunk_mono: vec![0.0_f64; chunk_frames],
            sub_mono: vec![0.0_f64; block_size],
            gains0: vec![0.0_f64; n_spk],
            gains1: vec![0.0_f64; n_spk],
            obj_state: ObjectRenderState::new(n_spk),
            sub_blocks: Vec::with_capacity((chunk_frames / block_size).saturating_add(4)),
        }
    }
}

struct BatchSlot {
    bus: PcmBus,
    scratch: ChunkScratch,
    frames: usize,
    stats: Vec<ChannelStats>,
}

/// Progress update emitted during offline rendering passes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RenderProgress {
    /// 1-based current pass (`1` for single-pass or analysis pass, `2` for second pass).
    pub pass: u8,
    /// Total number of passes (`1` or `2`).
    pub total_passes: u8,
    /// Frames completed so far in the current pass.
    pub frames_done: u64,
    /// Total frames in the scene.
    pub total_frames: u64,
}

struct RenderPassOptions<'a, 'p, 'f> {
    output_scale: f64,
    pool: Option<&'a rayon::ThreadPool>,
    pass: u8,
    total_passes: u8,
    progress: Option<&'p mut (dyn FnMut(RenderProgress) + 'f)>,
}

/// Renders all objects in `scene` onto `layout` using `spatializer`, writing blocks to `writer`.
pub fn render_scene(
    scene: &mut Scene,
    layout: &SpeakerLayout,
    spatializer: &dyn Spatializer,
    config: RenderConfig,
    writer: Box<dyn SampleWriter>,
) -> Result<RenderReport, RenderError> {
    render_scene_with_progress(scene, layout, spatializer, config, writer, None)
}

/// Renders all objects in `scene` onto `layout` while invoking `progress` after each pass/batch.
pub fn render_scene_with_progress(
    scene: &mut Scene,
    layout: &SpeakerLayout,
    spatializer: &dyn Spatializer,
    config: RenderConfig,
    writer: Box<dyn SampleWriter>,
    mut progress: Option<&mut dyn FnMut(RenderProgress)>,
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

    let custom_pool = if config.threads > 1 {
        Some(
            rayon::ThreadPoolBuilder::new()
                .num_threads(config.threads)
                .build()
                .map_err(|e| {
                    RenderError::InvalidConfig(format!("failed to build rayon thread pool: {e}"))
                })?,
        )
    } else {
        None
    };
    let pool = custom_pool.as_ref();

    match config.clipping {
        ClippingPolicy::PassThrough => run_render_pass(
            scene,
            layout,
            spatializer,
            config,
            writer,
            RenderPassOptions {
                output_scale: 1.0,
                pool,
                pass: 1,
                total_passes: 1,
                progress,
            },
        ),
        ClippingPolicy::FixedHeadroomDb(db) => {
            if !db.is_finite() || db < 0.0 {
                return Err(RenderError::InvalidConfig(
                    "FixedHeadroomDb must be finite and >= 0.0".to_string(),
                ));
            }
            let scale = 10.0_f64.powf(-db / 20.0);
            run_render_pass(
                scene,
                layout,
                spatializer,
                config,
                writer,
                RenderPassOptions {
                    output_scale: scale,
                    pool,
                    pass: 1,
                    total_passes: 1,
                    progress,
                },
            )
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
                Box::new(NullWriter::default()),
                RenderPassOptions {
                    output_scale: 1.0,
                    pool,
                    pass: 1,
                    total_passes: 2,
                    progress: progress.as_deref_mut(),
                },
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
            run_render_pass(
                scene,
                layout,
                spatializer,
                config,
                writer,
                RenderPassOptions {
                    output_scale: norm_scale,
                    pool,
                    pass: 2,
                    total_passes: 2,
                    progress,
                },
            )
        }
        ClippingPolicy::TwoPassLoudnessNormalize {
            target_lufs,
            true_peak_ceiling_dbtp,
        } => {
            if !target_lufs.is_finite() || target_lufs > 0.0 {
                return Err(RenderError::InvalidConfig(
                    "TwoPassLoudnessNormalize target_lufs must be finite and <= 0.0".to_string(),
                ));
            }
            if !true_peak_ceiling_dbtp.is_finite() || true_peak_ceiling_dbtp > 0.0 {
                return Err(RenderError::InvalidConfig(
                    "TwoPassLoudnessNormalize true_peak_ceiling_dbtp must be finite and <= 0.0"
                        .to_string(),
                ));
            }
            let (analysis_writer, measurement_slot) =
                LoudnessAnalyzerWriter::pair(scene.sample_rate, layout);
            run_render_pass(
                scene,
                layout,
                spatializer,
                config,
                analysis_writer,
                RenderPassOptions {
                    output_scale: 1.0,
                    pool,
                    pass: 1,
                    total_passes: 2,
                    progress: progress.as_deref_mut(),
                },
            )?;
            let measured = measurement_slot
                .lock()
                .map_err(|_| RenderError::InvalidConfig("loudness meter mutex poisoned".into()))?
                .take()
                .unwrap_or(LoudnessMeasurement {
                    integrated_lufs: f64::NEG_INFINITY,
                    true_peak_linear: 0.0,
                });

            let norm_scale = if !measured.integrated_lufs.is_finite() {
                1.0
            } else {
                let desired = 10.0_f64.powf((target_lufs - measured.integrated_lufs) / 20.0);
                if measured.true_peak_linear > 1e-12 {
                    let max_tp =
                        10.0_f64.powf(true_peak_ceiling_dbtp / 20.0) / measured.true_peak_linear;
                    desired.min(max_tp)
                } else {
                    desired
                }
            };

            let mut report = run_render_pass(
                scene,
                layout,
                spatializer,
                config,
                writer,
                RenderPassOptions {
                    output_scale: norm_scale,
                    pool,
                    pass: 2,
                    total_passes: 2,
                    progress,
                },
            )?;
            if measured.integrated_lufs.is_finite() {
                let gain_db = 20.0 * norm_scale.log10();
                report.integrated_lufs = Some(measured.integrated_lufs + gain_db);
                report.true_peak_dbtp = Some(measured.true_peak_dbtp() + gain_db);
            } else {
                report.integrated_lufs = Some(f64::NEG_INFINITY);
                report.true_peak_dbtp = Some(measured.true_peak_dbtp());
            }
            Ok(report)
        }
    }
}

fn init_channel_stats(layout: &SpeakerLayout) -> Vec<ChannelStats> {
    layout
        .speakers
        .iter()
        .map(|s| ChannelStats {
            speaker_label: s.label.clone(),
            peak_linear: 0.0,
            sum_squares: 0.0,
            clipped_samples: 0,
        })
        .collect()
}

fn reset_channel_stats(stats: &mut [ChannelStats]) {
    for s in stats {
        s.peak_linear = 0.0;
        s.sum_squares = 0.0;
        s.clipped_samples = 0;
    }
}

fn fold_chunk_stats(acc: &mut [ChannelStats], chunk: &[ChannelStats]) {
    for (a, c) in acc.iter_mut().zip(chunk.iter()) {
        if c.peak_linear > a.peak_linear {
            a.peak_linear = c.peak_linear;
        }
        a.sum_squares += c.sum_squares;
        a.clipped_samples += c.clipped_samples;
    }
}

fn run_render_pass(
    scene: &Scene,
    layout: &SpeakerLayout,
    spatializer: &dyn Spatializer,
    config: RenderConfig,
    mut writer: Box<dyn SampleWriter>,
    opts: RenderPassOptions<'_, '_, '_>,
) -> Result<RenderReport, RenderError> {
    let RenderPassOptions {
        output_scale,
        pool,
        pass,
        total_passes,
        mut progress,
    } = opts;
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

    let ctx = RenderPassCtx {
        scene,
        spatializer,
        config,
        output_scale,
        lfe_channels,
        full_range_channels,
        arch: pulp::Arch::new(),
    };

    let mut stats = init_channel_stats(layout);
    let chunk_frames = ((DEFAULT_CHUNK_FRAMES / config.block_size).max(1)) * config.block_size;
    let num_chunks = scene.total_frames.div_ceil(chunk_frames as u64) as usize;

    let effective_threads = if config.threads == 1 {
        1
    } else if let Some(p) = pool {
        p.current_num_threads()
    } else {
        rayon::current_num_threads().max(1)
    };

    let mut report_progress = |frames_done: u64| {
        if let Some(cb) = progress.as_deref_mut() {
            cb(RenderProgress {
                pass,
                total_passes,
                frames_done,
                total_frames: scene.total_frames,
            });
        }
    };
    report_progress(0);

    if effective_threads <= 1 || num_chunks <= 1 {
        let mut bus = PcmBus::new(n_spk, chunk_frames);
        let mut scratch = ChunkScratch::new(n_spk, chunk_frames, config.block_size);
        let mut chunk_stats = init_channel_stats(layout);

        for chunk_idx in 0..num_chunks {
            let chunk_start = (chunk_idx as u64) * (chunk_frames as u64);
            let chunk_end = chunk_start
                .saturating_add(chunk_frames as u64)
                .min(scene.total_frames);
            let chunk_len = (chunk_end - chunk_start) as usize;

            reset_channel_stats(&mut chunk_stats);
            render_chunk_into(
                &ctx,
                chunk_start,
                chunk_end,
                &mut bus,
                &mut scratch,
                &mut chunk_stats,
            )?;
            fold_chunk_stats(&mut stats, &chunk_stats);
            writer.write_block(&bus, chunk_len)?;
            report_progress(chunk_end);
        }
    } else {
        let batch_size = (effective_threads * 2).clamp(4, 32);
        let mut batch_slots: Vec<BatchSlot> = (0..batch_size.min(num_chunks))
            .map(|_| BatchSlot {
                bus: PcmBus::new(n_spk, chunk_frames),
                scratch: ChunkScratch::new(n_spk, chunk_frames, config.block_size),
                frames: 0,
                stats: init_channel_stats(layout),
            })
            .collect();

        let mut chunk_idx = 0usize;
        while chunk_idx < num_chunks {
            let batch_end = (chunk_idx + batch_slots.len()).min(num_chunks);
            let batch_len = batch_end - chunk_idx;
            let active_slots = &mut batch_slots[..batch_len];

            let mut render_batch = || -> Result<(), RenderError> {
                active_slots
                    .par_iter_mut()
                    .enumerate()
                    .try_for_each(|(offset, slot)| {
                        let idx = chunk_idx + offset;
                        let chunk_start = (idx as u64) * (chunk_frames as u64);
                        let chunk_end = chunk_start
                            .saturating_add(chunk_frames as u64)
                            .min(scene.total_frames);
                        let chunk_len = (chunk_end - chunk_start) as usize;

                        reset_channel_stats(&mut slot.stats);
                        render_chunk_into(
                            &ctx,
                            chunk_start,
                            chunk_end,
                            &mut slot.bus,
                            &mut slot.scratch,
                            &mut slot.stats,
                        )?;
                        slot.frames = chunk_len;
                        Ok(())
                    })
            };

            match pool {
                Some(p) => p.install(render_batch)?,
                None => render_batch()?,
            }

            for slot in &*active_slots {
                fold_chunk_stats(&mut stats, &slot.stats);
                writer.write_block(&slot.bus, slot.frames)?;
            }
            let done = ((batch_end as u64) * (chunk_frames as u64)).min(scene.total_frames);
            report_progress(done);
            chunk_idx = batch_end;
        }
    }

    let output_metadata = writer.finalize()?;
    Ok(RenderReport {
        frames_rendered: scene.total_frames,
        rf64_promoted: output_metadata.rf64_promoted,
        applied_gain_linear: output_scale,
        integrated_lufs: None,
        true_peak_dbtp: None,
        output_metadata,
        channels: stats,
    })
}

fn render_chunk_into(
    ctx: &RenderPassCtx<'_>,
    chunk_start: u64,
    chunk_end: u64,
    bus: &mut PcmBus,
    scratch: &mut ChunkScratch,
    chunk_stats: &mut [ChannelStats],
) -> Result<(), RenderError> {
    let chunk_len = (chunk_end - chunk_start) as usize;
    bus.clear(chunk_len);

    // Precompute deterministic sub-block schedule so discontinuous keyframes never smear early.
    scratch.sub_blocks.clear();
    let mut frame_start = chunk_start;
    while frame_start < chunk_end {
        let mut block_end = frame_start
            .saturating_add(ctx.config.block_size as u64)
            .min(chunk_end);
        for obj in &ctx.scene.objects {
            if let Some(disc_frame) = timeline::next_discontinuity_in(obj, frame_start, block_end) {
                block_end = block_end.min(disc_frame);
            }
        }
        scratch.sub_blocks.push((frame_start, block_end));
        frame_start = block_end;
    }

    for obj in &ctx.scene.objects {
        if chunk_start >= obj.end_frame() || chunk_end <= obj.start_frame {
            continue;
        }

        init_object_state_at(obj, chunk_start, ctx, &mut scratch.obj_state);

        let use_preread = chunk_start >= obj.start_frame;
        let mut preread_done = false;

        for &(frame_start, block_end) in &scratch.sub_blocks {
            if frame_start >= obj.end_frame() || block_end <= obj.start_frame {
                scratch.obj_state.initialized = false;
                scratch.obj_state.xfade_remaining = 0;
                continue;
            }

            let n_frames = (block_end - frame_start) as usize;
            let (state0, state1) = timeline::eval_interval(obj, frame_start, block_end);
            let is_disc = timeline::is_discontinuous_at(obj, frame_start);

            if state0.gain == 0.0
                && state1.gain == 0.0
                && scratch.obj_state.xfade_remaining == 0
                && !is_disc
            {
                scratch.obj_state.prev_gains.fill(0.0);
                scratch.obj_state.initialized = true;
                continue;
            }

            compute_object_gains(obj, state0, ctx, &mut scratch.gains0);
            if state0 == state1 {
                scratch.gains1.copy_from_slice(&scratch.gains0);
            } else {
                compute_object_gains(obj, state1, ctx, &mut scratch.gains1);
            }

            if scratch.obj_state.initialized && ctx.config.crossfade_frames > 0 && is_disc {
                scratch
                    .obj_state
                    .xfade_from
                    .copy_from_slice(&scratch.obj_state.prev_gains);
                scratch.obj_state.xfade_total = ctx.config.crossfade_frames;
                scratch.obj_state.xfade_remaining = ctx.config.crossfade_frames;
            }

            let mono_slice: &[f64] = if use_preread {
                if !preread_done {
                    obj.source.read_frames(
                        chunk_start - obj.start_frame,
                        &mut scratch.chunk_mono[..chunk_len],
                    )?;
                    preread_done = true;
                }
                let rel = (frame_start - chunk_start) as usize;
                &scratch.chunk_mono[rel..rel + n_frames]
            } else {
                let local_offset = frame_start.saturating_sub(obj.start_frame);
                obj.source
                    .read_frames(local_offset, &mut scratch.sub_mono[..n_frames])?;
                &scratch.sub_mono[..n_frames]
            };

            let bus_offset = (frame_start - chunk_start) as usize;
            if scratch.obj_state.xfade_remaining == 0 {
                simd::accumulate_linear_ramp_simd_at(
                    ctx.arch,
                    mono_slice,
                    &scratch.gains0,
                    &scratch.gains1,
                    bus,
                    bus_offset,
                    n_frames,
                );
            } else {
                accumulate_with_crossfade(
                    mono_slice,
                    &scratch.gains0,
                    &scratch.gains1,
                    &mut scratch.obj_state,
                    bus,
                    bus_offset,
                    n_frames,
                );
            }

            scratch
                .obj_state
                .prev_gains
                .copy_from_slice(&scratch.gains1);
            scratch.obj_state.initialized = true;
        }
    }

    simd::scale_and_meter_bus_simd(ctx.arch, bus, chunk_len, ctx.output_scale, chunk_stats);
    Ok(())
}

fn init_object_state_at(
    obj: &AudioObject,
    chunk_start: u64,
    ctx: &RenderPassCtx<'_>,
    obj_state: &mut ObjectRenderState,
) {
    obj_state.reset();
    if chunk_start == 0 || chunk_start <= obj.start_frame || chunk_start >= obj.end_frame() {
        return;
    }

    let prev_state = timeline::eval_left_limit(obj, chunk_start);
    compute_object_gains(obj, prev_state, ctx, &mut obj_state.prev_gains);
    obj_state.initialized = true;

    if ctx.config.crossfade_frames > 1 {
        let min_frame = chunk_start.saturating_sub((ctx.config.crossfade_frames - 1) as u64);
        if let Some(disc_frame) = timeline::last_discontinuity_in(obj, min_frame, chunk_start)
            && disc_frame > obj.start_frame
        {
            let elapsed = (chunk_start - disc_frame) as usize;
            if elapsed < ctx.config.crossfade_frames {
                let prior_state = timeline::eval_left_limit(obj, disc_frame);
                compute_object_gains(obj, prior_state, ctx, &mut obj_state.xfade_from);
                obj_state.xfade_total = ctx.config.crossfade_frames;
                obj_state.xfade_remaining = ctx.config.crossfade_frames - elapsed;
            }
        }
    }
}

#[inline]
fn compute_object_gains(
    obj: &AudioObject,
    state: ObjectState,
    ctx: &RenderPassCtx<'_>,
    out_gains: &mut [f64],
) {
    if state.gain == 0.0 {
        out_gains.fill(0.0);
        return;
    }
    match obj.role {
        ObjectRole::Lfe => {
            compute_lfe_gains(
                state.gain,
                &ctx.lfe_channels,
                &ctx.full_range_channels,
                ctx.config.lfe_policy,
                out_gains,
            );
        }
        ObjectRole::Dynamic | ObjectRole::Bed => {
            ctx.spatializer
                .compute_gains(state.position, state.spatial, out_gains);
            scale_gains(out_gains, state.gain);
        }
    }
}

fn accumulate_with_crossfade(
    mono: &[f64],
    gains0: &[f64],
    gains1: &[f64],
    obj_state: &mut ObjectRenderState,
    bus: &mut PcmBus,
    bus_offset: usize,
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
        let out = &mut bus.channel_mut(ch)[bus_offset..bus_offset + n_frames];

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
