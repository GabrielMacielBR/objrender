//! 1D / 2D / 3D VBAP renderer backed by Omniphony's pure-Rust triangulation & MDAP engine.

use std::cell::RefCell;
use std::f64::consts::{PI, TAU};

use crate::layout::{LayoutError, SpeakerLayout, SpeakerRole};
use crate::math::{Mat3, Vec3};
use crate::renderer::Spatializer;
use crate::renderer::vbap_native::{
    DummyRing, OutOfHullMode, Vbap3dScratch, invert_ls_mtx_3d, prepare_triangulation, vbap3d_into,
};
use crate::scene::SpatialParams;

thread_local! {
    static VBAP_SCRATCH: RefCell<(Vbap3dScratch, Vec<f32>)> =
        RefCell::new((Vbap3dScratch::new(0), Vec::new()));
}

/// Default radius around the origin within which directional panning smoothly
/// blends toward an omnidirectional field across full-range speakers.
pub const DEFAULT_NEAR_ORIGIN_RADIUS: f64 = 0.15;

#[derive(Debug, Clone)]
struct Pair2D {
    vert_a: usize,
    vert_b: usize,
    inv_mat: Mat3,
}

#[derive(Debug, Clone)]
enum VbapBackend {
    Mono {
        channel: usize,
    },
    TwoSpeakerArc {
        pairs: Vec<Pair2D>,
        /// For each hull vertex (real or virtual), maps to `(real_slot_0..2, linear_weight)`.
        vertex_weights: Vec<Vec<(usize, f64)>>,
    },
    Native3D {
        ls_groups: Vec<[usize; 3]>,
        layout_inv_mtx: Vec<[f32; 9]>,
        is_dummy: Vec<bool>,
        mode: OutOfHullMode,
        dummy_rings: Vec<DummyRing>,
    },
}

/// Vector Base Amplitude Panning (VBAP) spatializer supporting arbitrary 1D/2D/3D layouts.
#[derive(Debug, Clone)]
pub struct VbapRenderer {
    num_total_speakers: usize,
    full_range_channels: Vec<usize>,
    full_range_dirs: Vec<Vec3>,
    near_origin_radius: f64,
    backend: VbapBackend,
}

impl VbapRenderer {
    /// Builds a VBAP renderer using [`OutOfHullMode::VirtualPoles`] (ITU-R BS.2127 closure).
    pub fn build(layout: &SpeakerLayout) -> Result<Self, LayoutError> {
        Self::build_with_mode(layout, OutOfHullMode::VirtualPoles)
    }

    /// Builds a VBAP renderer with an explicit out-of-hull handling mode.
    pub fn build_with_mode(
        layout: &SpeakerLayout,
        mode: OutOfHullMode,
    ) -> Result<Self, LayoutError> {
        layout.validate()?;
        let mode = match mode {
            OutOfHullMode::Blend { power } => OutOfHullMode::Blend {
                power: if power.is_finite() {
                    power.max(1.0)
                } else {
                    12.0
                },
            },
            other => other,
        };

        let mut full_range_channels = Vec::new();
        let mut full_range_dirs = Vec::new();
        let mut dirs_deg: Vec<[f32; 2]> = Vec::new();

        for (idx, spk) in layout.speakers.iter().enumerate() {
            if spk.role != SpeakerRole::FullRange {
                continue;
            }
            let dir = spk
                .position
                .normalized()
                .ok_or_else(|| LayoutError::ZeroPosition {
                    label: spk.label.clone(),
                })?;
            let sph = dir.to_spherical();
            full_range_channels.push(idx);
            full_range_dirs.push(dir);
            dirs_deg.push([sph.azimuth_deg as f32, sph.elevation_deg as f32]);
        }

        let n_fr = full_range_channels.len();
        let backend = match n_fr {
            0 => return Err(LayoutError::NoFullRangeSpeakers(layout.name.clone())),
            1 => VbapBackend::Mono {
                channel: full_range_channels[0],
            },
            2 => build_two_speaker_arc(&full_range_dirs),
            _ => {
                let tri = prepare_triangulation(&dirs_deg, true, true, mode)
                    .or_else(|| prepare_triangulation(&dirs_deg, false, true, mode))
                    .ok_or_else(|| LayoutError::TriangulationFailed(layout.name.clone()))?;

                if tri.ls_groups.is_empty() {
                    return Err(LayoutError::TriangulationFailed(layout.name.clone()));
                }

                let layout_inv_mtx = invert_ls_mtx_3d(&tri.u_spkr, &tri.ls_groups);
                let dummy_rings = tri.downmix_rings(mode);

                VbapBackend::Native3D {
                    ls_groups: tri.ls_groups,
                    layout_inv_mtx,
                    is_dummy: tri.is_dummy,
                    mode,
                    dummy_rings,
                }
            }
        };

        Ok(Self {
            num_total_speakers: layout.speakers.len(),
            full_range_channels,
            full_range_dirs,
            near_origin_radius: DEFAULT_NEAR_ORIGIN_RADIUS,
            backend,
        })
    }

    /// Sets the inner radius within which directional gains smoothly transition to an
    /// omnidirectional field (`0.0` disables the blend).
    pub fn with_near_origin_radius(mut self, radius: f64) -> Self {
        self.near_origin_radius = if radius.is_finite() {
            radius.max(0.0)
        } else {
            DEFAULT_NEAR_ORIGIN_RADIUS
        };
        self
    }

    #[inline]
    pub fn near_origin_radius(&self) -> f64 {
        self.near_origin_radius
    }

    fn fill_omnidirectional(&self, out_gains: &mut [f64]) {
        let n = self.full_range_channels.len();
        let g = 1.0 / (n as f64).sqrt();
        for &ch in &self.full_range_channels {
            out_gains[ch] = g;
        }
    }

    fn fallback_nearest_speaker(&self, dir: Vec3, out_gains: &mut [f64]) {
        let mut best_idx = 0;
        let mut best_dot = f64::NEG_INFINITY;
        for (i, &spk_dir) in self.full_range_dirs.iter().enumerate() {
            let d = dir.dot(spk_dir);
            if d > best_dot {
                best_dot = d;
                best_idx = i;
            }
        }
        out_gains[self.full_range_channels[best_idx]] = 1.0;
    }
}

impl Spatializer for VbapRenderer {
    fn num_speakers(&self) -> usize {
        self.num_total_speakers
    }

    fn compute_gains(&self, position: Vec3, params: SpatialParams, out_gains: &mut [f64]) {
        assert_eq!(
            out_gains.len(),
            self.num_total_speakers,
            "out_gains length must match layout speaker count"
        );
        out_gains.fill(0.0);

        let r = position.length();
        if r < 1e-9 || !r.is_finite() {
            self.fill_omnidirectional(out_gains);
            return;
        }

        match &self.backend {
            VbapBackend::Mono { channel } => {
                out_gains[*channel] = 1.0;
            }
            VbapBackend::TwoSpeakerArc {
                pairs,
                vertex_weights,
            } => {
                let xy = Vec3::new(position.x, position.y, 0.0);
                let Some(dir_xy) = xy.normalized() else {
                    self.fill_omnidirectional(out_gains);
                    return;
                };

                let width = params.width.clamp(0.0, 1.0);
                let mut slot_gains = [0.0_f64; 2];

                if width <= 1e-3 {
                    eval_two_speaker_arc(pairs, vertex_weights, dir_xy, &mut slot_gains);
                } else {
                    // Spread across [-width * 90 deg, +width * 90 deg] in azimuth.
                    let half_span = width * (PI * 0.5);
                    let offsets = [-1.0, -0.5, 0.0, 0.5, 1.0];
                    let base_az = (-dir_xy.x).atan2(dir_xy.y);
                    for s in offsets {
                        let az = base_az + s * half_span;
                        let p = Vec3::new(-az.sin(), az.cos(), 0.0);
                        let mut sample = [0.0_f64; 2];
                        eval_two_speaker_arc(pairs, vertex_weights, p, &mut sample);
                        slot_gains[0] += sample[0] * sample[0];
                        slot_gains[1] += sample[1] * sample[1];
                    }
                    slot_gains[0] = slot_gains[0].sqrt();
                    slot_gains[1] = slot_gains[1].sqrt();
                }

                out_gains[self.full_range_channels[0]] = slot_gains[0];
                out_gains[self.full_range_channels[1]] = slot_gains[1];
                normalize_l2(out_gains);
            }
            VbapBackend::Native3D {
                ls_groups,
                layout_inv_mtx,
                is_dummy,
                mode,
                dummy_rings,
            } => {
                let sph = position.to_spherical();
                let spread_deg = (params.width.clamp(0.0, 1.0) as f32) * 180.0;
                let n_real = self.full_range_channels.len();

                VBAP_SCRATCH.with(|cell| {
                    let mut guard = cell.borrow_mut();
                    let (scratch, local_gains) = &mut *guard;
                    if local_gains.len() != n_real {
                        local_gains.resize(n_real, 0.0);
                    }
                    local_gains.fill(0.0);

                    vbap3d_into(
                        [sph.azimuth_deg as f32, sph.elevation_deg as f32],
                        ls_groups,
                        spread_deg,
                        layout_inv_mtx,
                        is_dummy,
                        *mode,
                        dummy_rings,
                        scratch,
                        local_gains,
                    );

                    for (slot, &g) in local_gains.iter().enumerate() {
                        let clamped = if g.is_finite() {
                            (g as f64).max(0.0)
                        } else {
                            0.0
                        };
                        out_gains[self.full_range_channels[slot]] = clamped;
                    }
                });

                let sum_sq: f64 = out_gains.iter().map(|g| g * g).sum();
                if matches!(mode, OutOfHullMode::Fade) {
                    // Fade mode intentionally attenuates toward zero at uncovered poles.
                } else if sum_sq <= 1e-18 {
                    let dir = position * (1.0 / r);
                    self.fallback_nearest_speaker(dir, out_gains);
                } else {
                    normalize_l2(out_gains);
                }
            }
        }

        let n_fr = self.full_range_channels.len();
        let div = params.divergence.clamp(0.0, 1.0);
        if n_fr > 1 && div > 1e-9 {
            let energy_dir: f64 = self
                .full_range_channels
                .iter()
                .map(|&ch| out_gains[ch] * out_gains[ch])
                .sum();
            let omni_power = energy_dir / (n_fr as f64);
            for &ch in &self.full_range_channels {
                let g_sq = (1.0 - div) * out_gains[ch] * out_gains[ch] + div * omni_power;
                out_gains[ch] = g_sq.max(0.0).sqrt();
            }
        }

        if n_fr > 1 && self.near_origin_radius > 0.0 && r < self.near_origin_radius {
            let alpha = crate::math::smoothstep(0.0, self.near_origin_radius, r);
            let norm_dir: f64 = self
                .full_range_channels
                .iter()
                .map(|&ch| out_gains[ch] * out_gains[ch])
                .sum::<f64>()
                .sqrt();
            let omni_g = 1.0 / (n_fr as f64).sqrt();
            for &ch in &self.full_range_channels {
                out_gains[ch] = (1.0 - alpha) * omni_g + alpha * out_gains[ch];
            }
            let target_norm = (1.0 - alpha) * 1.0 + alpha * norm_dir;
            let cur_norm: f64 = self
                .full_range_channels
                .iter()
                .map(|&ch| out_gains[ch] * out_gains[ch])
                .sum::<f64>()
                .sqrt();
            if cur_norm > 1e-12 && cur_norm.is_finite() {
                let scale = target_norm / cur_norm;
                for &ch in &self.full_range_channels {
                    out_gains[ch] *= scale;
                }
            }
        }
    }
}

fn build_two_speaker_arc(dirs: &[Vec3]) -> VbapBackend {
    let mut sorted: Vec<(usize, f64)> = dirs
        .iter()
        .enumerate()
        .map(|(i, d)| (i, (-d.x).atan2(d.y)))
        .collect();
    sorted.sort_by(|a, b| a.1.total_cmp(&b.1));

    let mut vert_dirs: Vec<Vec3> = Vec::new();
    let mut vertex_weights: Vec<Vec<(usize, f64)>> = Vec::new();

    for idx in 0..2 {
        let (slot_a, az_a) = sorted[idx];
        let (slot_b, az_b) = sorted[(idx + 1) % 2];
        let mut gap = az_b - az_a;
        if gap <= 0.0 {
            gap += TAU;
        }

        vert_dirs.push(Vec3::new(-az_a.sin(), az_a.cos(), 0.0));
        vertex_weights.push(vec![(slot_a, 1.0)]);

        if gap >= 160.0_f64.to_radians() {
            let sub_arcs = (gap / 120.0_f64.to_radians()).ceil() as usize;
            for step in 1..sub_arcs {
                let t = (step as f64) / (sub_arcs as f64);
                let az_v = az_a + gap * t;
                vert_dirs.push(Vec3::new(-az_v.sin(), az_v.cos(), 0.0));
                vertex_weights.push(vec![(slot_a, 1.0 - t), (slot_b, t)]);
            }
        }
    }

    let z_axis = Vec3::new(0.0, 0.0, 1.0);
    let m = vert_dirs.len();
    let mut pairs = Vec::with_capacity(m);
    for a in 0..m {
        let b = (a + 1) % m;
        let mat = Mat3::from_cols(vert_dirs[a], vert_dirs[b], z_axis);
        if let Some(inv_mat) = mat.inverse() {
            pairs.push(Pair2D {
                vert_a: a,
                vert_b: b,
                inv_mat,
            });
        }
    }

    VbapBackend::TwoSpeakerArc {
        pairs,
        vertex_weights,
    }
}

fn eval_two_speaker_arc(
    pairs: &[Pair2D],
    vertex_weights: &[Vec<(usize, f64)>],
    dir_xy: Vec3,
    slot_gains: &mut [f64; 2],
) {
    let mut best_min = f64::NEG_INFINITY;
    let mut best_pair: Option<&Pair2D> = None;
    let mut best_g = (0.0, 0.0);

    for pair in pairs {
        let g = pair.inv_mat.mul_vec3(dir_xy);
        let min_g = g.x.min(g.y);
        if min_g > best_min {
            best_min = min_g;
            best_pair = Some(pair);
            best_g = (g.x, g.y);
        }
    }

    if let Some(pair) = best_pair {
        let ga = best_g.0.max(0.0);
        let gb = best_g.1.max(0.0);
        for &(slot, w) in &vertex_weights[pair.vert_a] {
            slot_gains[slot] += ga * w;
        }
        for &(slot, w) in &vertex_weights[pair.vert_b] {
            slot_gains[slot] += gb * w;
        }
    }
}

#[inline]
fn normalize_l2(gains: &mut [f64]) {
    let norm = gains.iter().map(|g| g * g).sum::<f64>().sqrt();
    if norm > 1e-12 && norm.is_finite() {
        let inv = 1.0 / norm;
        for g in gains {
            *g *= inv;
        }
    }
}
