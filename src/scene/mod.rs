//! Format-agnostic intermediate scene, audio object, and PCM source abstractions.

use std::f64::consts::TAU;

use thiserror::Error;

use crate::io::IoError;
use crate::math::Vec3;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct ObjectId(pub u32);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ObjectRole {
    Dynamic,
    Bed,
    Lfe,
}

#[derive(Debug, Clone, Copy, PartialEq, Default)]
pub struct SpatialParams {
    pub width: f64,
    pub divergence: f64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum InterpMode {
    Linear,
    Spherical,
    Step,
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Keyframe {
    pub frame: u64,
    pub position: Vec3,
    pub gain: f64,
    pub spatial: SpatialParams,
    pub interp: InterpMode,
    pub discontinuous: bool,
}

/// Decouples in-memory test signals from disk-streamed mono PCM tracks.
pub trait AudioSource: Send {
    fn total_frames(&self) -> u64;

    /// Reads up to `out.len()` mono samples starting at `frame_offset`, zero-padding any tail.
    fn read_frames(&mut self, frame_offset: u64, out: &mut [f64]) -> Result<usize, IoError>;
}

/// Simple in-memory mono PCM sample source.
pub struct InMemorySource {
    samples: Vec<f64>,
}

impl InMemorySource {
    pub fn new(samples: Vec<f64>) -> Self {
        Self { samples }
    }
}

impl AudioSource for InMemorySource {
    fn total_frames(&self) -> u64 {
        self.samples.len() as u64
    }

    fn read_frames(&mut self, frame_offset: u64, out: &mut [f64]) -> Result<usize, IoError> {
        out.fill(0.0);
        let start = frame_offset as usize;
        if start >= self.samples.len() {
            return Ok(0);
        }
        let avail = (self.samples.len() - start).min(out.len());
        out[..avail].copy_from_slice(&self.samples[start..start + avail]);
        Ok(avail)
    }
}

/// Deterministic mono sine wave generator for testing and CLI previews.
pub struct SineSource {
    sample_rate: u32,
    freq_hz: f64,
    amplitude: f64,
    total_frames: u64,
}

impl SineSource {
    pub fn new(sample_rate: u32, freq_hz: f64, amplitude: f64, total_frames: u64) -> Self {
        Self {
            sample_rate,
            freq_hz,
            amplitude,
            total_frames,
        }
    }
}

impl AudioSource for SineSource {
    fn total_frames(&self) -> u64 {
        self.total_frames
    }

    fn read_frames(&mut self, frame_offset: u64, out: &mut [f64]) -> Result<usize, IoError> {
        out.fill(0.0);
        if frame_offset >= self.total_frames {
            return Ok(0);
        }
        let avail = ((self.total_frames - frame_offset) as usize).min(out.len());
        let phase_step = TAU * self.freq_hz / (self.sample_rate as f64);
        for (i, sample) in out[..avail].iter_mut().enumerate() {
            let t = (frame_offset + i as u64) as f64;
            *sample = self.amplitude * (t * phase_step).sin();
        }
        Ok(avail)
    }
}

/// Deterministic unit/scaled impulse source for exact gain verification tests.
pub struct ImpulseSource {
    impulse_frame: u64,
    amplitude: f64,
    total_frames: u64,
}

impl ImpulseSource {
    pub fn new(impulse_frame: u64, amplitude: f64, total_frames: u64) -> Self {
        Self {
            impulse_frame,
            amplitude,
            total_frames,
        }
    }
}

impl AudioSource for ImpulseSource {
    fn total_frames(&self) -> u64 {
        self.total_frames
    }

    fn read_frames(&mut self, frame_offset: u64, out: &mut [f64]) -> Result<usize, IoError> {
        out.fill(0.0);
        if frame_offset >= self.total_frames {
            return Ok(0);
        }
        let avail = ((self.total_frames - frame_offset) as usize).min(out.len());
        if self.impulse_frame >= frame_offset && self.impulse_frame < frame_offset + avail as u64 {
            out[(self.impulse_frame - frame_offset) as usize] = self.amplitude;
        }
        Ok(avail)
    }
}

pub struct AudioObject {
    pub id: ObjectId,
    pub label: String,
    pub role: ObjectRole,
    pub priority: u8,
    pub start_frame: u64,
    pub frame_count: u64,
    pub initial_position: Vec3,
    pub initial_gain: f64,
    pub initial_spatial: SpatialParams,
    pub keyframes: Vec<Keyframe>,
    pub source: Box<dyn AudioSource>,
}

impl AudioObject {
    pub fn static_mono(
        id: ObjectId,
        label: impl Into<String>,
        role: ObjectRole,
        position: Vec3,
        gain: f64,
        source: Box<dyn AudioSource>,
    ) -> Self {
        let frame_count = source.total_frames();
        Self {
            id,
            label: label.into(),
            role,
            priority: 0,
            start_frame: 0,
            frame_count,
            initial_position: position,
            initial_gain: gain,
            initial_spatial: SpatialParams::default(),
            keyframes: Vec::new(),
            source,
        }
    }

    #[inline]
    pub fn end_frame(&self) -> u64 {
        self.start_frame.saturating_add(self.frame_count)
    }
}

pub struct Scene {
    pub sample_rate: u32,
    pub total_frames: u64,
    pub objects: Vec<AudioObject>,
}

#[derive(Debug, Error)]
pub enum SceneError {
    #[error("sample_rate must be greater than zero")]
    InvalidSampleRate,
    #[error("object `{0}` has non-monotonic keyframes")]
    UnsortedKeyframes(u32),
}

pub struct SceneBuilder {
    sample_rate: u32,
    total_frames: u64,
    objects: Vec<AudioObject>,
}

impl SceneBuilder {
    pub fn new(sample_rate: u32, total_frames: u64) -> Self {
        Self {
            sample_rate,
            total_frames,
            objects: Vec::new(),
        }
    }

    pub fn add_object(mut self, object: AudioObject) -> Self {
        self.objects.push(object);
        self
    }

    pub fn build(self) -> Result<Scene, SceneError> {
        if self.sample_rate == 0 {
            return Err(SceneError::InvalidSampleRate);
        }
        for obj in &self.objects {
            if obj.keyframes.windows(2).any(|w| w[0].frame > w[1].frame) {
                return Err(SceneError::UnsortedKeyframes(obj.id.0));
            }
        }
        Ok(Scene {
            sample_rate: self.sample_rate,
            total_frames: self.total_frames,
            objects: self.objects,
        })
    }
}
