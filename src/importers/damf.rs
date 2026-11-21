//! DAMF / OAMD format adapter translating normalized room-cube coordinates to core space.
//!
//! Phase 1 ships a static stub validating the importer-to-core decoupling boundary;
//! Phase 6 will add full decoded DAMF + CAF + OAMD timeline parsing.

use crate::importers::{ImportError, SceneImporter};
use crate::math::Vec3;
use crate::scene::{
    AudioObject, AudioSource, ImpulseSource, InMemorySource, ObjectId, ObjectRole, Scene,
    SceneBuilder, SineSource, SpatialParams,
};

/// Translates normalized DAMF / OAMD room-cube coordinates `(ax, ay, az)` into core space.
///
/// Input convention:
/// - `ax in [0.0, 1.0]`: Left (`0.0`) -> Right (`1.0`)
/// - `ay in [0.0, 1.0]`: Front (`0.0`) -> Back (`1.0`)
/// - `az in [-1.0, 1.0]`: Floor (`-1.0`) -> Ear (`0.0`) -> Ceiling (`+1.0`)
///
/// Output core convention: listener-centered right-handed (`+X` Right, `+Y` Front, `+Z` Up).
#[inline]
pub fn damf_cube_to_core(ax: f64, ay: f64, az: f64) -> Vec3 {
    Vec3::new(2.0 * ax - 1.0, 1.0 - 2.0 * ay, az)
}

#[derive(Debug, Clone)]
pub enum DamfSignalSpec {
    Sine { freq_hz: f64, amplitude: f64 },
    Impulse { impulse_frame: u64, amplitude: f64 },
    Samples(Vec<f64>),
}

#[derive(Debug, Clone)]
pub struct DamfStaticDescriptor {
    pub sample_rate: u32,
    pub total_frames: u64,
    pub object_id: u32,
    pub label: String,
    pub role: ObjectRole,
    pub pos_x: f64,
    pub pos_y: f64,
    pub pos_z: f64,
    pub gain: f64,
    pub width: f64,
    pub divergence: f64,
    pub signal: DamfSignalSpec,
}

/// Phase 1 static DAMF descriptor importer implementing [`SceneImporter`].
pub struct DamfStaticImporter {
    pub descriptor: DamfStaticDescriptor,
}

impl DamfStaticImporter {
    pub fn new(descriptor: DamfStaticDescriptor) -> Self {
        Self { descriptor }
    }
}

impl SceneImporter for DamfStaticImporter {
    fn import(&self) -> Result<Scene, ImportError> {
        let d = &self.descriptor;
        if !(0.0..=1.0).contains(&d.pos_x)
            || !(0.0..=1.0).contains(&d.pos_y)
            || !(-1.0..=1.0).contains(&d.pos_z)
        {
            return Err(ImportError::InvalidDescriptor(format!(
                "DAMF coordinates out of range: ({}, {}, {})",
                d.pos_x, d.pos_y, d.pos_z
            )));
        }

        let core_pos = damf_cube_to_core(d.pos_x, d.pos_y, d.pos_z);
        let source: Box<dyn AudioSource> = match &d.signal {
            DamfSignalSpec::Sine { freq_hz, amplitude } => Box::new(SineSource::new(
                d.sample_rate,
                *freq_hz,
                *amplitude,
                d.total_frames,
            )),
            DamfSignalSpec::Impulse {
                impulse_frame,
                amplitude,
            } => Box::new(ImpulseSource::new(
                *impulse_frame,
                *amplitude,
                d.total_frames,
            )),
            DamfSignalSpec::Samples(samples) => Box::new(InMemorySource::new(samples.clone())),
        };

        let mut obj = AudioObject::static_mono(
            ObjectId(d.object_id),
            d.label.clone(),
            d.role,
            core_pos,
            d.gain,
            source,
        );
        obj.frame_count = d.total_frames;
        obj.initial_spatial = SpatialParams {
            width: d.width,
            divergence: d.divergence,
        };

        let scene = SceneBuilder::new(d.sample_rate, d.total_frames)
            .add_object(obj)
            .build()?;
        Ok(scene)
    }
}
