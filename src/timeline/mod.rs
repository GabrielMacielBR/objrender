//! Format-agnostic keyframe evaluation and trajectory interpolation.

use crate::math::Vec3;
use crate::scene::{AudioObject, InterpMode, Keyframe, SpatialParams};

/// Instantaneous object state evaluated at a specific sample frame.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct ObjectState {
    pub position: Vec3,
    pub gain: f64,
    pub spatial: SpatialParams,
}

impl ObjectState {
    #[inline]
    fn from_keyframe(kf: &Keyframe) -> Self {
        Self {
            position: kf.position,
            gain: kf.gain,
            spatial: kf.spatial,
        }
    }

    #[inline]
    fn initial(obj: &AudioObject) -> Self {
        Self {
            position: obj.initial_position,
            gain: obj.initial_gain,
            spatial: obj.initial_spatial,
        }
    }
}

/// Evaluates `obj` state at `frame` using its sorted keyframe track.
pub fn eval_at(obj: &AudioObject, frame: u64) -> ObjectState {
    let kfs = &obj.keyframes;
    if kfs.is_empty() {
        return ObjectState::initial(obj);
    }

    // `idx` is the first keyframe strictly after `frame`.
    let idx = kfs.partition_point(|k| k.frame <= frame);
    if idx == 0 {
        return ObjectState::initial(obj);
    }
    if idx >= kfs.len() {
        return ObjectState::from_keyframe(&kfs[kfs.len() - 1]);
    }

    let ka = &kfs[idx - 1];
    let kb = &kfs[idx];
    if ka.frame == frame || ka.frame >= kb.frame {
        return ObjectState::from_keyframe(ka);
    }

    match ka.interp {
        InterpMode::Step => ObjectState::from_keyframe(ka),
        InterpMode::Linear | InterpMode::Spherical => {
            let span = (kb.frame - ka.frame) as f64;
            let u = ((frame - ka.frame) as f64 / span).clamp(0.0, 1.0);
            interpolate_states(ka, kb, u, ka.interp)
        }
    }
}

/// Evaluates `[start_frame, end_frame)` cleanly: `start` is evaluated at `start_frame`
/// and `end` is the left-hand limit at `end_frame^-` so `Step` jumps never leak backward.
pub fn eval_interval(
    obj: &AudioObject,
    start_frame: u64,
    end_frame: u64,
) -> (ObjectState, ObjectState) {
    let start_state = eval_at(obj, start_frame);
    if end_frame <= start_frame {
        return (start_state, start_state);
    }
    let end_state = eval_left_limit(obj, end_frame);
    (start_state, end_state)
}

fn eval_left_limit(obj: &AudioObject, frame: u64) -> ObjectState {
    let kfs = &obj.keyframes;
    if kfs.is_empty() {
        return ObjectState::initial(obj);
    }

    // `idx` is the first keyframe at or after `frame` (strictly `< frame` precedes it).
    let idx = kfs.partition_point(|k| k.frame < frame);
    if idx == 0 {
        return ObjectState::initial(obj);
    }
    if idx >= kfs.len() {
        return ObjectState::from_keyframe(&kfs[kfs.len() - 1]);
    }

    let ka = &kfs[idx - 1];
    let kb = &kfs[idx];
    if ka.frame >= kb.frame {
        return ObjectState::from_keyframe(ka);
    }

    match ka.interp {
        InterpMode::Step => ObjectState::from_keyframe(ka),
        InterpMode::Linear | InterpMode::Spherical => {
            let span = (kb.frame - ka.frame) as f64;
            let u = ((frame - ka.frame) as f64 / span).clamp(0.0, 1.0);
            interpolate_states(ka, kb, u, ka.interp)
        }
    }
}

/// Returns `true` if `obj` has a keyframe marked `discontinuous` at exact `frame`.
pub fn is_discontinuous_at(obj: &AudioObject, frame: u64) -> bool {
    let start = obj.keyframes.partition_point(|k| k.frame < frame);
    obj.keyframes[start..]
        .iter()
        .take_while(|k| k.frame == frame)
        .any(|k| k.discontinuous)
}

/// Finds the earliest discontinuous keyframe strictly inside `(start_exclusive, end_exclusive)`.
pub fn next_discontinuity_in(
    obj: &AudioObject,
    start_exclusive: u64,
    end_exclusive: u64,
) -> Option<u64> {
    if end_exclusive <= start_exclusive.saturating_add(1) {
        return None;
    }
    let start = obj
        .keyframes
        .partition_point(|k| k.frame <= start_exclusive);
    obj.keyframes[start..]
        .iter()
        .take_while(|k| k.frame < end_exclusive)
        .find(|k| k.discontinuous)
        .map(|k| k.frame)
}

fn interpolate_states(ka: &Keyframe, kb: &Keyframe, u: f64, mode: InterpMode) -> ObjectState {
    let inv = 1.0 - u;
    let position = match mode {
        InterpMode::Linear => ka.position * inv + kb.position * u,
        InterpMode::Spherical => slerp_vec3(ka.position, kb.position, u),
        InterpMode::Step => ka.position,
    };

    ObjectState {
        position,
        gain: ka.gain * inv + kb.gain * u,
        spatial: SpatialParams {
            width: ka.spatial.width * inv + kb.spatial.width * u,
            divergence: ka.spatial.divergence * inv + kb.spatial.divergence * u,
        },
    }
}

fn slerp_vec3(a: Vec3, b: Vec3, u: f64) -> Vec3 {
    let ra = a.length();
    let rb = b.length();
    let r = ra * (1.0 - u) + rb * u;
    if r <= 1e-12 {
        return Vec3::ZERO;
    }
    let (Some(ua), Some(ub)) = (a.normalized(), b.normalized()) else {
        return a * (1.0 - u) + b * u;
    };

    let dot = ua.dot(ub).clamp(-1.0, 1.0);
    if dot > 0.999_999_999 {
        return (ua * (1.0 - u) + ub * u).normalized().unwrap_or(ua) * r;
    }

    if dot < -0.999_9 {
        // Nearly antipodal: pick a deterministic orthogonal axis for a clean 180-deg geodesic.
        let candidate = if ua.x.abs() < 0.9 {
            Vec3::new(1.0, 0.0, 0.0)
        } else {
            Vec3::new(0.0, 1.0, 0.0)
        };
        let ortho = ua
            .cross(candidate)
            .normalized()
            .unwrap_or(Vec3::new(0.0, 0.0, 1.0));
        let angle = std::f64::consts::PI * u;
        return (ua * angle.cos() + ortho * angle.sin()) * r;
    }

    let omega = dot.acos();
    let sin_omega = omega.sin();
    let w_a = ((1.0 - u) * omega).sin() / sin_omega;
    let w_b = (u * omega).sin() / sin_omega;
    (ua * w_a + ub * w_b) * r
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::math::SphericalCoord;
    use crate::scene::{ImpulseSource, ObjectId, ObjectRole};
    use approx::assert_relative_eq;

    fn dummy_object(keyframes: Vec<Keyframe>) -> AudioObject {
        let mut obj = AudioObject::static_mono(
            ObjectId(1),
            "test",
            ObjectRole::Dynamic,
            Vec3::new(0.0, 1.0, 0.0),
            0.5,
            Box::new(ImpulseSource::new(0, 1.0, 1000)),
        );
        obj.keyframes = keyframes;
        obj
    }

    #[test]
    fn linear_and_step_interpolation_respect_boundaries() {
        let obj = dummy_object(vec![
            Keyframe {
                frame: 100,
                position: Vec3::new(-1.0, 0.0, 0.0),
                gain: 0.2,
                spatial: SpatialParams {
                    width: 0.0,
                    divergence: 0.0,
                },
                interp: InterpMode::Linear,
                discontinuous: false,
            },
            Keyframe {
                frame: 200,
                position: Vec3::new(1.0, 0.0, 0.0),
                gain: 1.0,
                spatial: SpatialParams {
                    width: 0.8,
                    divergence: 0.4,
                },
                interp: InterpMode::Step,
                discontinuous: true,
            },
            Keyframe {
                frame: 300,
                position: Vec3::new(0.0, 1.0, 0.0),
                gain: 0.5,
                spatial: SpatialParams::default(),
                interp: InterpMode::Step,
                discontinuous: false,
            },
        ]);

        // Before first keyframe -> initial state
        let pre = eval_at(&obj, 50);
        assert_relative_eq!(pre.position.y, 1.0, epsilon = 1e-12);
        assert_relative_eq!(pre.gain, 0.5, epsilon = 1e-12);

        // Midpoint of [100, 200] linear ramp
        let mid = eval_at(&obj, 150);
        assert_relative_eq!(mid.position.x, 0.0, epsilon = 1e-12);
        assert_relative_eq!(mid.gain, 0.6, epsilon = 1e-12);
        assert_relative_eq!(mid.spatial.width, 0.4, epsilon = 1e-12);

        // Step hold between [200, 300)
        let hold = eval_at(&obj, 250);
        assert_relative_eq!(hold.position.x, 1.0, epsilon = 1e-12);
        assert_relative_eq!(hold.gain, 1.0, epsilon = 1e-12);

        // At and past final keyframe
        let end = eval_at(&obj, 500);
        assert_relative_eq!(end.position.y, 1.0, epsilon = 1e-12);
        assert_relative_eq!(end.gain, 0.5, epsilon = 1e-12);

        assert!(is_discontinuous_at(&obj, 200));
        assert!(!is_discontinuous_at(&obj, 100));
        assert_eq!(next_discontinuity_in(&obj, 150, 250), Some(200));
        assert_eq!(next_discontinuity_in(&obj, 200, 250), None);
    }

    #[test]
    fn spherical_slerp_preserves_unit_radius_along_arc() {
        let p_left = Vec3::from_spherical(SphericalCoord::new(90.0, 0.0, 1.0));
        let p_front = Vec3::from_spherical(SphericalCoord::new(0.0, 0.0, 1.0));
        let obj = dummy_object(vec![
            Keyframe {
                frame: 0,
                position: p_left,
                gain: 1.0,
                spatial: SpatialParams::default(),
                interp: InterpMode::Spherical,
                discontinuous: false,
            },
            Keyframe {
                frame: 100,
                position: p_front,
                gain: 1.0,
                spatial: SpatialParams::default(),
                interp: InterpMode::Step,
                discontinuous: false,
            },
        ]);

        let mid = eval_at(&obj, 50);
        assert_relative_eq!(mid.position.length(), 1.0, epsilon = 1e-12);
        let sph = mid.position.to_spherical();
        assert_relative_eq!(sph.azimuth_deg, 45.0, epsilon = 1e-9);
        assert_relative_eq!(sph.elevation_deg, 0.0, epsilon = 1e-9);
    }
}
