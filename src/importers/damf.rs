//! DAMF / OAMD format adapter for decoded `truehdd` / `harletty` master sets.
//!
//! Supports:
//! - [`DamfImporter`]: real 3-file DAMF bundles (`.atmos` YAML manifest, `.atmos.metadata`
//!   delta OAMD timeline, `.atmos.audio` multichannel CAF stream)
//! - [`DamfStaticImporter`] / [`damf_cube_to_core`]: Phase 1 synthetic static helper

use std::collections::HashMap;
use std::fs;
use std::io::{Read, Seek};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use serde::Deserialize;
use serde::de::{self, Visitor};

use crate::importers::caf::{CafChannelSource, SharedCafStream};
use crate::importers::{ImportError, SceneImporter};
use crate::math::{SphericalCoord, Vec3};
use crate::scene::{
    AudioObject, AudioSource, ImpulseSource, InMemorySource, InterpMode, Keyframe, ObjectId,
    ObjectRole, Scene, SceneBuilder, SineSource, SpatialParams,
};

/// Translates normalized OAMD `[0, 1]` room-cube coordinates `(ax, ay, az)` into core space.
///
/// Note: `truehdd` / `harletty` `.atmos.metadata` YAML files already convert `pos` to `[-1, +1]`
/// listener-centered coordinates; use this helper only when consuming raw `[0, 1]` OAMD cubes.
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

/// Synthetic static DAMF descriptor importer implementing [`SceneImporter`].
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

        Ok(SceneBuilder::new(d.sample_rate, d.total_frames)
            .add_object(obj)
            .build()?)
    }
}

/// Real `truehdd` / `harletty` DAMF master bundle importer (`.atmos` + `.atmos.metadata` + `.atmos.audio`).
pub struct DamfImporter {
    pub manifest_path: PathBuf,
}

impl DamfImporter {
    pub fn new(manifest_path: impl AsRef<Path>) -> Self {
        Self {
            manifest_path: manifest_path.as_ref().to_path_buf(),
        }
    }

    /// Builds a generic [`Scene`] from in-memory YAML manifests and an open CAF stream.
    pub fn from_parts<R: Read + Seek + Send + 'static>(
        manifest_yaml: &str,
        metadata_yaml: &str,
        caf_stream: Arc<Mutex<SharedCafStream<R>>>,
    ) -> Result<Scene, ImportError> {
        let manifest: DamfManifest = serde_yaml_ng::from_str(manifest_yaml)?;
        let presentation = manifest.presentations.first().ok_or_else(|| {
            ImportError::InvalidDescriptor("DAMF manifest contains no presentations".into())
        })?;
        let metadata: DamfMetadata = serde_yaml_ng::from_str(metadata_yaml)?;

        let caf_info = caf_stream
            .lock()
            .map_err(|_| ImportError::InvalidDescriptor("CAF mutex poisoned".into()))?
            .info();
        let sample_rate = metadata.sample_rate.unwrap_or(caf_info.sample_rate);
        if sample_rate != caf_info.sample_rate {
            return Err(ImportError::InvalidDescriptor(format!(
                "metadata sample rate ({sample_rate}) != CAF sample rate ({})",
                caf_info.sample_rate
            )));
        }

        let mut specs = build_track_specs(presentation)?;
        if specs.len() > caf_info.num_channels {
            return Err(ImportError::InvalidDescriptor(format!(
                "DAMF manifest declares {} channels, but CAF stream only has {}",
                specs.len(),
                caf_info.num_channels
            )));
        }

        let mut tracks_by_id: HashMap<u32, usize> = HashMap::with_capacity(specs.len());
        for (idx, spec) in specs.iter().enumerate() {
            if tracks_by_id.insert(spec.id, idx).is_some() {
                return Err(ImportError::InvalidDescriptor(format!(
                    "duplicate DAMF track ID {}",
                    spec.id
                )));
            }
        }

        let mut states: Vec<TrackTimeline> = specs
            .iter()
            .map(|s| TrackTimeline::new(s.initial_pos))
            .collect();

        for ev in &metadata.events {
            if let Some(&idx) = tracks_by_id.get(&ev.id) {
                states[idx].apply_event(ev);
            }
        }

        let mut builder = SceneBuilder::new(sample_rate, caf_info.total_frames);
        for (caf_channel, (spec, track)) in specs.drain(..).zip(states).enumerate() {
            let source: Box<dyn AudioSource> =
                Box::new(CafChannelSource::new(Arc::clone(&caf_stream), caf_channel)?);
            let mut obj = AudioObject::static_mono(
                ObjectId(spec.id),
                spec.label,
                spec.role,
                spec.initial_pos,
                1.0,
                source,
            );
            obj.frame_count = caf_info.total_frames;

            if let Some(first) = track.keyframes.first() {
                obj.initial_position = first.position;
                obj.initial_gain = first.gain;
                obj.initial_spatial = first.spatial;
            }
            obj.keyframes = track.keyframes;
            builder = builder.add_object(obj);
        }

        Ok(builder.build()?)
    }
}

impl SceneImporter for DamfImporter {
    fn import(&self) -> Result<Scene, ImportError> {
        let base_dir = self
            .manifest_path
            .parent()
            .filter(|p| !p.as_os_str().is_empty())
            .unwrap_or_else(|| Path::new("."));

        let manifest_yaml =
            fs::read_to_string(&self.manifest_path).map_err(|source| ImportError::Fs {
                path: self.manifest_path.display().to_string(),
                source,
            })?;
        let manifest: DamfManifest = serde_yaml_ng::from_str(&manifest_yaml)?;
        let presentation = manifest.presentations.first().ok_or_else(|| {
            ImportError::InvalidDescriptor("DAMF manifest contains no presentations".into())
        })?;

        let metadata_path = resolve_rel_path(base_dir, &presentation.metadata);
        let audio_path = resolve_rel_path(base_dir, &presentation.audio);

        let metadata_yaml =
            fs::read_to_string(&metadata_path).map_err(|source| ImportError::Fs {
                path: metadata_path.display().to_string(),
                source,
            })?;
        let caf_stream = SharedCafStream::open(&audio_path)?;

        Self::from_parts(&manifest_yaml, &metadata_yaml, caf_stream)
    }
}

fn resolve_rel_path(base_dir: &Path, child: &str) -> PathBuf {
    let p = Path::new(child);
    if p.is_absolute() {
        p.to_path_buf()
    } else {
        base_dir.join(p)
    }
}

struct TrackSpec {
    id: u32,
    label: String,
    role: ObjectRole,
    initial_pos: Vec3,
}

fn build_track_specs(pres: &DamfPresentation) -> Result<Vec<TrackSpec>, ImportError> {
    let mut specs = Vec::new();
    for bed in &pres.bed_instances {
        for ch in &bed.channels {
            let (role, pos) = bed_label_to_role_and_pos(&ch.channel).ok_or_else(|| {
                ImportError::InvalidDescriptor(format!("unknown DAMF bed channel `{}`", ch.channel))
            })?;
            specs.push(TrackSpec {
                id: ch.id,
                label: ch.channel.clone(),
                role,
                initial_pos: pos,
            });
        }
    }
    for obj in &pres.objects {
        let label = obj
            .description
            .clone()
            .or_else(|| obj.group_name.clone())
            .unwrap_or_else(|| format!("obj_{}", obj.id));
        specs.push(TrackSpec {
            id: obj.id,
            label,
            role: ObjectRole::Dynamic,
            initial_pos: Vec3::new(0.0, 1.0, 0.0),
        });
    }
    if specs.is_empty() {
        return Err(ImportError::InvalidDescriptor(
            "DAMF presentation declares zero beds or objects".into(),
        ));
    }
    Ok(specs)
}

fn bed_label_to_role_and_pos(label: &str) -> Option<(ObjectRole, Vec3)> {
    let upper = label.trim().to_ascii_uppercase();
    if matches!(upper.as_str(), "LFE" | "LFE1" | "LFE2") {
        return Some((ObjectRole::Lfe, Vec3::ZERO));
    }
    let (az, el) = match upper.as_str() {
        "L" => (30.0, 0.0),
        "R" => (-30.0, 0.0),
        "C" => (0.0, 0.0),
        "LC" => (15.0, 0.0),
        "RC" => (-15.0, 0.0),
        "LW" => (60.0, 0.0),
        "RW" => (-60.0, 0.0),
        "LSS" => (90.0, 0.0),
        "RSS" => (-90.0, 0.0),
        "LS" => (110.0, 0.0),
        "RS" => (-110.0, 0.0),
        "LRS" | "LB" => (140.0, 0.0),
        "RRS" | "RB" => (-140.0, 0.0),
        "CS" | "CB" => (180.0, 0.0),
        "LTF" => (45.0, 45.0),
        "RTF" => (-45.0, 45.0),
        "CTF" => (0.0, 45.0),
        "LTS" | "LTM" => (90.0, 60.0),
        "RTS" | "RTM" => (-90.0, 60.0),
        "LTR" => (135.0, 45.0),
        "RTR" => (-135.0, 45.0),
        "CTR" => (180.0, 45.0),
        "TC" | "VHC" | "TOP" => (0.0, 90.0),
        _ => return None,
    };
    Some((
        ObjectRole::Bed,
        Vec3::from_spherical(SphericalCoord::new(az, el, 1.0)),
    ))
}

struct TrackTimeline {
    pos: Vec3,
    linear_gain: f64,
    active: bool,
    spatial: SpatialParams,
    keyframes: Vec<Keyframe>,
}

impl TrackTimeline {
    fn new(initial_pos: Vec3) -> Self {
        Self {
            pos: initial_pos,
            linear_gain: 1.0,
            active: true,
            spatial: SpatialParams::default(),
            keyframes: Vec::new(),
        }
    }

    fn effective_gain(&self) -> f64 {
        if self.active { self.linear_gain } else { 0.0 }
    }

    fn apply_event(&mut self, ev: &DamfEvent) {
        let t = ev.sample_pos;
        let ramp = ev.ramp_length.unwrap_or(0);

        // If an earlier ramp extends past `t`, sample its in-flight state at `t` and truncate it.
        if let Some(last) = self.keyframes.last().copied()
            && last.frame > t
        {
            if let Some(prev) = self.keyframes.iter().rev().nth(1).copied()
                && prev.frame <= t
            {
                let span = (last.frame - prev.frame) as f64;
                let u = ((t - prev.frame) as f64 / span).clamp(0.0, 1.0);
                let inv = 1.0 - u;
                self.pos = prev.position * inv + last.position * u;
                self.linear_gain = prev.gain * inv + last.gain * u;
                self.active = self.linear_gain > 0.0;
                self.spatial = SpatialParams {
                    width: prev.spatial.width * inv + last.spatial.width * u,
                    divergence: prev.spatial.divergence * inv + last.spatial.divergence * u,
                };
            }
            self.keyframes.pop();
        }

        let prev_pos = self.pos;
        let prev_gain = self.effective_gain();
        let prev_spatial = self.spatial;

        if let Some(active) = ev.active {
            self.active = active;
        }
        if let Some([x, y, z]) = ev.pos {
            self.pos = Vec3::new(x, y, z);
        }
        if let Some(gain) = ev.gain {
            self.linear_gain = db_to_linear(gain.0);
        }
        if let Some([sx, sy, sz]) = ev.size_3d {
            self.spatial.width = ((sx + sy + sz) / 3.0).clamp(0.0, 1.0);
        } else if let Some(size) = ev.size {
            self.spatial.width = size.clamp(0.0, 1.0);
        }

        let new_gain = self.effective_gain();
        let new_pos = self.pos;
        let new_spatial = self.spatial;

        if ramp == 0 {
            let is_disc = t > 0 && !self.keyframes.is_empty();
            push_or_replace(
                &mut self.keyframes,
                Keyframe {
                    frame: t,
                    position: new_pos,
                    gain: new_gain,
                    spatial: new_spatial,
                    interp: InterpMode::Step,
                    discontinuous: is_disc,
                },
            );
        } else {
            push_or_replace(
                &mut self.keyframes,
                Keyframe {
                    frame: t,
                    position: prev_pos,
                    gain: prev_gain,
                    spatial: prev_spatial,
                    interp: InterpMode::Linear,
                    discontinuous: false,
                },
            );
            push_or_replace(
                &mut self.keyframes,
                Keyframe {
                    frame: t.saturating_add(ramp),
                    position: new_pos,
                    gain: new_gain,
                    spatial: new_spatial,
                    interp: InterpMode::Step,
                    discontinuous: false,
                },
            );
        }
    }
}

fn push_or_replace(keyframes: &mut Vec<Keyframe>, kf: Keyframe) {
    if let Some(last) = keyframes.last_mut()
        && last.frame == kf.frame
    {
        *last = kf;
    } else {
        keyframes.push(kf);
    }
}

#[inline]
fn db_to_linear(db: f64) -> f64 {
    if (!db.is_finite() && db.is_sign_negative()) || db <= -128.0 {
        0.0
    } else {
        10.0_f64.powf(db / 20.0)
    }
}

#[derive(Debug, Deserialize)]
struct DamfManifest {
    #[serde(default)]
    presentations: Vec<DamfPresentation>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct DamfPresentation {
    metadata: String,
    audio: String,
    #[serde(default)]
    bed_instances: Vec<DamfBedInstance>,
    #[serde(default)]
    objects: Vec<DamfObjectRef>,
}

#[derive(Debug, Deserialize)]
struct DamfBedInstance {
    #[serde(default)]
    channels: Vec<DamfBedChannel>,
}

#[derive(Debug, Deserialize)]
struct DamfBedChannel {
    channel: String,
    #[serde(rename = "ID")]
    id: u32,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct DamfObjectRef {
    #[serde(rename = "ID")]
    id: u32,
    #[serde(default)]
    description: Option<String>,
    #[serde(default)]
    group_name: Option<String>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct DamfMetadata {
    sample_rate: Option<u32>,
    #[serde(default)]
    events: Vec<DamfEvent>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct DamfEvent {
    #[serde(rename = "ID")]
    id: u32,
    #[serde(default)]
    sample_pos: u64,
    #[serde(default)]
    active: Option<bool>,
    #[serde(default)]
    pos: Option<[f64; 3]>,
    #[serde(default)]
    size: Option<f64>,
    #[serde(default, rename = "size3D")]
    size_3d: Option<[f64; 3]>,
    #[serde(default)]
    gain: Option<DamfGain>,
    #[serde(default)]
    ramp_length: Option<u64>,
}

#[derive(Debug, Clone, Copy)]
struct DamfGain(f64);

impl<'de> Deserialize<'de> for DamfGain {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        struct GainVisitor;
        impl Visitor<'_> for GainVisitor {
            type Value = DamfGain;

            fn expecting(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
                f.write_str("a dB number or string (e.g. -6.0 or \"-inf\")")
            }

            fn visit_i64<E: de::Error>(self, v: i64) -> Result<Self::Value, E> {
                Ok(DamfGain(v as f64))
            }

            fn visit_u64<E: de::Error>(self, v: u64) -> Result<Self::Value, E> {
                Ok(DamfGain(v as f64))
            }

            fn visit_f64<E: de::Error>(self, v: f64) -> Result<Self::Value, E> {
                Ok(DamfGain(v))
            }

            fn visit_str<E: de::Error>(self, v: &str) -> Result<Self::Value, E> {
                let s = v.trim();
                if s.eq_ignore_ascii_case("-inf")
                    || s.eq_ignore_ascii_case("-.inf")
                    || s.eq_ignore_ascii_case("-infinity")
                {
                    return Ok(DamfGain(f64::NEG_INFINITY));
                }
                s.parse::<f64>()
                    .map(DamfGain)
                    .map_err(|_| E::custom(format!("invalid gain string `{s}`")))
            }
        }
        deserializer.deserialize_any(GainVisitor)
    }
}
