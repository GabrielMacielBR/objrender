//! Spatial panning abstractions and VBAP rendering engine.

mod convhull;
pub mod vbap;
pub(crate) mod vbap_native;

pub use vbap::{DEFAULT_NEAR_ORIGIN_RADIUS, VbapRenderer};
pub use vbap_native::OutOfHullMode;

use crate::math::Vec3;
use crate::scene::SpatialParams;

/// Computes normalized speaker gain weights for a 3D source position.
pub trait Spatializer: Sync {
    fn num_speakers(&self) -> usize;

    /// Writes panning gains into `out_gains` (whose length must equal `num_speakers()`).
    fn compute_gains(&self, position: Vec3, params: SpatialParams, out_gains: &mut [f64]);
}
