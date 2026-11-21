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

/// Translates normalized OAMD `[0, 1]` room-cube coordinates `(ax, ay, az)` into spherical-mapped
/// core space.
///
/// Note: `truehdd` / `harletty` `.atmos.metadata` YAML files already convert `pos` to `[-1, +1]`
/// listener-centered coordinates; use this helper only when consuming raw `[0, 1]` OAMD cubes.
#[inline]
pub fn damf_cube_to_core(ax: f64, ay: f64, az: f64) -> Vec3 {
    room_cube_to_sphere(Vec3::new(2.0 * ax - 1.0, 1.0 - 2.0 * ay, az))
}

const SPHERE_FRONT_CORNER_DEG: f64 = 30.0;
const SPHERE_TOP_FRONT_CORNER_DEG: f64 = 45.0;
const SPHERE_REAR_CORNER_DEG: f64 = 135.0;

#[inline]
fn sphere_front_corner_deg(height: f64) -> f64 {
    SPHERE_FRONT_CORNER_DEG + (SPHERE_TOP_FRONT_CORNER_DEG - SPHERE_FRONT_CORNER_DEG) * height
}

/// Projects listener-centered DAMF room-cube coordinates `[-1, 1]^3` onto the
/// nominal listener sphere so cube corners land on ITU / Atmos nominal speaker
/// angles (`L/R` at `+-30 deg`, `Lss/Rss` at `+-90 deg`, `Lrs/Rrs` at `+-135 deg`,
/// `Ltf/Rtf` at `+-45 deg / +45 deg`, `Ltr/Rtr` at `+-135 deg / +45 deg`, and
/// ceiling edges at `+45 deg` elevation).
///
/// Adapted from `mgth/Omniphony` (`omniphony_geometry::sphere_reading`, GPL-3.0-or-later).
#[inline]
pub fn room_cube_to_sphere(pos: Vec3) -> Vec3 {
    let reach = pos.x.abs().max(pos.y.abs());
    if reach <= 0.0 {
        return pos;
    }
    let (ux, uy) = (pos.x.abs() / reach, pos.y / reach);
    let front = sphere_front_corner_deg((pos.z.abs() / reach).min(1.0));
    let azimuth = if uy >= ux {
        front * ux
    } else if uy <= -ux {
        180.0 - (180.0 - SPHERE_REAR_CORNER_DEG) * ux
    } else if uy >= 0.0 {
        90.0 - (90.0 - front) * uy
    } else {
        90.0 - (SPHERE_REAR_CORNER_DEG - 90.0) * uy
    };
    let (sin, cos) = azimuth.to_radians().sin_cos();
    let heard = [reach * sin.copysign(pos.x), reach * cos, pos.z];
    let max_heard = heard[0].abs().max(heard[1].abs()).max(heard[2].abs());
    if max_heard <= 0.0 {
        return pos;
    }
    let scale = reach.max(pos.z.abs()) / max_heard;
    Vec3::new(heard[0] * scale, heard[1] * scale, heard[2] * scale)
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

        let (mut specs, bed_count) = build_track_specs(presentation)?;
        let caf_channels = resolve_caf_channels(
            presentation.source_codec.as_deref(),
            &specs,
            bed_count,
            caf_info.num_channels,
        )?;

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
        for (caf_channel, (spec, track)) in
            caf_channels.into_iter().zip(specs.drain(..).zip(states))
        {
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

fn build_track_specs(pres: &DamfPresentation) -> Result<(Vec<TrackSpec>, usize), ImportError> {
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
    let bed_count = specs.len();
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
    Ok((specs, bed_count))
}

fn resolve_caf_channels(
    source_codec: Option<&str>,
    specs: &[TrackSpec],
    bed_count: usize,
    caf_channels: usize,
) -> Result<Vec<usize>, ImportError> {
    if specs.len() > caf_channels {
        return Err(ImportError::InvalidDescriptor(format!(
            "DAMF manifest declares {} channels, but CAF stream only has {}",
            specs.len(),
            caf_channels
        )));
    }

    if source_codec.is_some_and(|c| c.eq_ignore_ascii_case("EAC3-JOC")) {
        // harletty writes E-AC-3 JOC CAF streams as `[core_fullband..., optional LFE, objects...]`.
        let object_count = specs.len() - bed_count;
        let core_total = caf_channels - object_count;
        let beds = &specs[..bed_count];
        let mut mapping = Vec::with_capacity(specs.len());

        if bed_count == 1 && beds[0].role == ObjectRole::Lfe {
            if core_total == 0 {
                return Err(ImportError::InvalidDescriptor(
                    "EAC3-JOC stream declares LFE bed but CAF has zero core channels".into(),
                ));
            }
            mapping.push(core_total - 1);
        } else if bed_count > 0 {
            let explicit = map_eac3_bed_channels(beds, core_total);
            if core_total == bed_count {
                match explicit {
                    Some(indices) => mapping.extend(indices),
                    None => mapping.extend(0..bed_count),
                }
            } else {
                let indices = explicit.ok_or_else(|| {
                    ImportError::InvalidDescriptor(format!(
                        "unable to map {bed_count} DAMF bed channels onto {core_total} EAC3-JOC core channels"
                    ))
                })?;
                mapping.extend(indices);
            }
        }

        for k in 0..object_count {
            mapping.push(core_total + k);
        }
        return Ok(mapping);
    }

    Ok((0..specs.len()).collect())
}

fn map_eac3_bed_channels(beds: &[TrackSpec], core_total: usize) -> Option<Vec<usize>> {
    let has_lfe = beds.iter().any(|b| b.role == ObjectRole::Lfe);
    if has_lfe && core_total == 0 {
        return None;
    }
    let fullband_limit = if has_lfe { core_total - 1 } else { core_total };
    let mut used = vec![false; core_total];
    let mut out = Vec::with_capacity(beds.len());

    for bed in beds {
        let slot = if bed.role == ObjectRole::Lfe {
            core_total - 1
        } else {
            let s = match bed.label.trim().to_ascii_uppercase().as_str() {
                "L" => 0,
                "C" => 1,
                "R" => 2,
                "LS" | "LSS" => 3,
                "RS" | "RSS" => 4,
                "LB" | "LRS" => 5,
                "RB" | "RRS" => 6,
                _ => return None,
            };
            if s >= fullband_limit {
                return None;
            }
            s
        };
        if used[slot] {
            return None;
        }
        used[slot] = true;
        out.push(slot);
    }

    Some(out)
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
        "LRS" | "LB" => (135.0, 0.0),
        "RRS" | "RB" => (-135.0, 0.0),
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
    raw_pos: Option<Vec3>,
    pos: Vec3,
    linear_gain: f64,
    active: bool,
    elevation_enabled: bool,
    spatial: SpatialParams,
    keyframes: Vec<Keyframe>,
}

impl TrackTimeline {
    fn new(initial_pos: Vec3) -> Self {
        Self {
            raw_pos: None,
            pos: initial_pos,
            linear_gain: 1.0,
            active: true,
            elevation_enabled: true,
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
        if let Some(elev) = ev.elevation {
            self.elevation_enabled = elev;
        }
        if let Some([x, y, z]) = ev.pos {
            let raw = Vec3::new(x, y, z);
            self.raw_pos = Some(raw);
            let z_eff = if self.elevation_enabled { raw.z } else { 0.0 };
            self.pos = room_cube_to_sphere(Vec3::new(raw.x, raw.y, z_eff));
        } else if ev.elevation.is_some()
            && let Some(raw) = self.raw_pos
        {
            let z_eff = if self.elevation_enabled { raw.z } else { 0.0 };
            self.pos = room_cube_to_sphere(Vec3::new(raw.x, raw.y, z_eff));
        }
        if let Some(gain) = ev.gain {
            self.linear_gain = db_to_linear(gain.0);
        }
        if let Some([sx, sy, sz]) = ev.size_3d {
            self.spatial.width = ((sx * sx + sy * sy + sz * sz) / 3.0).sqrt().clamp(0.0, 1.0);
        } else if let Some(size) = ev.size {
            self.spatial.width = size.clamp(0.0, 1.0);
        }
        if let Some(div) = ev.divergence {
            self.spatial.divergence = div.clamp(0.0, 1.0);
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
    source_codec: Option<String>,
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
    elevation: Option<bool>,
    #[serde(default)]
    size: Option<f64>,
    #[serde(default, rename = "size3D")]
    size_3d: Option<[f64; 3]>,
    #[serde(default)]
    divergence: Option<f64>,
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

#[cfg(test)]
mod tests {
    use super::*;
    use approx::assert_relative_eq;

    #[test]
    fn room_cube_to_sphere_maps_nominal_corners_and_edges() {
        let cases = [
            (Vec3::new(-1.0, 1.0, 0.0), 30.0, 0.0),
            (Vec3::new(1.0, 1.0, 0.0), -30.0, 0.0),
            (Vec3::new(-1.0, 1.0, 1.0), 45.0, 45.0),
            (Vec3::new(1.0, 1.0, 1.0), -45.0, 45.0),
            (Vec3::new(-1.0, 0.0, 0.0), 90.0, 0.0),
            (Vec3::new(1.0, 0.0, 0.0), -90.0, 0.0),
            (Vec3::new(-1.0, 0.0, 1.0), 90.0, 45.0),
            (Vec3::new(1.0, 0.0, 1.0), -90.0, 45.0),
            (Vec3::new(-1.0, -1.0, 0.0), 135.0, 0.0),
            (Vec3::new(1.0, -1.0, 0.0), -135.0, 0.0),
            (Vec3::new(-1.0, -1.0, 1.0), 135.0, 45.0),
            (Vec3::new(1.0, -1.0, 1.0), -135.0, 45.0),
            (Vec3::new(0.0, 1.0, 0.0), 0.0, 0.0),
        ];

        for (cube_pos, expected_az, expected_el) in cases {
            let mapped = room_cube_to_sphere(cube_pos);
            let sph = mapped.to_spherical();
            assert_relative_eq!(sph.azimuth_deg, expected_az, epsilon = 1e-9);
            assert_relative_eq!(sph.elevation_deg, expected_el, epsilon = 1e-9);
        }

        let zenith = room_cube_to_sphere(Vec3::new(0.0, 0.0, 1.0));
        assert_relative_eq!(zenith.to_spherical().elevation_deg, 90.0, epsilon = 1e-9);

        let origin = room_cube_to_sphere(Vec3::ZERO);
        assert_relative_eq!(origin.x, 0.0, epsilon = 1e-12);
        assert_relative_eq!(origin.y, 0.0, epsilon = 1e-12);
        assert_relative_eq!(origin.z, 0.0, epsilon = 1e-12);
    }
}
