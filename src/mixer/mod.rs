//! Offline multi-object mixing, LFE routing, headroom policies, and render orchestration.

use thiserror::Error;

use crate::io::{IoError, OutputMetadata, PcmBus, SampleWriter};
use crate::layout::{SpeakerLayout, SpeakerRole};
use crate::renderer::Spatializer;
use crate::scene::{ObjectRole, Scene};

#[derive(Debug, Clone, Copy, PartialEq)]
pub enum ClippingPolicy {
    /// Writes raw `f64` sum without attenuation; tracks peaks above `0 dBFS`.
    PassThrough,
    /// Applies static attenuation of `headroom_db` dB (e.g., `6.0` scales by `-6 dB`).
    FixedHeadroomDb(f64),
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
    pub clipping: ClippingPolicy,
    pub lfe_policy: LfePolicy,
}

impl Default for RenderConfig {
    fn default() -> Self {
        Self {
            block_size: 64,
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
    pub output_metadata: OutputMetadata,
    pub channels: Vec<ChannelStats>,
}

#[derive(Debug, Error)]
pub enum RenderError {
    #[error("invalid render configuration: {0}")]
    InvalidConfig(String),
    #[error("I/O error during render: {0}")]
    Io(#[from] IoError),
}

/// Renders all objects in `scene` onto `layout` using `spatializer`, writing blocks to `writer`.
pub fn render_scene(
    scene: &mut Scene,
    layout: &SpeakerLayout,
    spatializer: &dyn Spatializer,
    config: RenderConfig,
    mut writer: Box<dyn SampleWriter>,
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

    let headroom_scale = match config.clipping {
        ClippingPolicy::PassThrough => 1.0,
        ClippingPolicy::FixedHeadroomDb(db) => {
            if !db.is_finite() || db < 0.0 {
                return Err(RenderError::InvalidConfig(
                    "FixedHeadroomDb must be finite and >= 0.0".to_string(),
                ));
            }
            10.0_f64.powf(-db / 20.0)
        }
    };

    let mut bus = PcmBus::new(n_spk, config.block_size);
    let mut mono_buf = vec![0.0_f64; config.block_size];
    let mut gains = vec![0.0_f64; n_spk];

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
        let remaining = scene.total_frames - frame_start;
        let n_frames = (config.block_size as u64).min(remaining) as usize;
        bus.clear(n_frames);

        for obj in &mut scene.objects {
            if frame_start >= obj.end_frame()
                || frame_start.saturating_add(n_frames as u64) <= obj.start_frame
            {
                continue;
            }

            let local_offset = frame_start.saturating_sub(obj.start_frame);
            obj.source
                .read_frames(local_offset, &mut mono_buf[..n_frames])?;

            match obj.role {
                ObjectRole::Lfe => {
                    compute_lfe_gains(
                        obj.initial_gain,
                        &lfe_channels,
                        &full_range_channels,
                        config.lfe_policy,
                        &mut gains,
                    );
                }
                ObjectRole::Dynamic | ObjectRole::Bed => {
                    spatializer.compute_gains(
                        obj.initial_position,
                        obj.initial_spatial,
                        &mut gains,
                    );
                    if (obj.initial_gain - 1.0).abs() > f64::EPSILON {
                        for g in &mut gains {
                            *g *= obj.initial_gain;
                        }
                    }
                }
            }

            for (ch, &g) in gains.iter().enumerate() {
                if g == 0.0 {
                    continue;
                }
                let out = bus.channel_mut(ch);
                for (dst, &src) in out[..n_frames].iter_mut().zip(&mono_buf[..n_frames]) {
                    *dst += src * g;
                }
            }
        }

        if (headroom_scale - 1.0).abs() > f64::EPSILON {
            for ch in 0..n_spk {
                for sample in &mut bus.channel_mut(ch)[..n_frames] {
                    *sample *= headroom_scale;
                }
            }
        }

        for (ch, ch_stat) in stats.iter_mut().enumerate() {
            for &sample in &bus.channel(ch)[..n_frames] {
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

        writer.write_block(&bus, n_frames)?;
        frame_start += n_frames as u64;
    }

    let output_metadata = writer.finalize()?;
    Ok(RenderReport {
        frames_rendered: scene.total_frames,
        rf64_promoted: output_metadata.rf64_promoted,
        output_metadata,
        channels: stats,
    })
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
