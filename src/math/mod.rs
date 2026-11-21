//! Core 3D linear algebra and spherical coordinate conversions.

use std::ops::{Add, Mul, Neg, Sub};

/// Right-handed listener-centered 3D Cartesian vector.
///
/// Axis convention:
/// - `+X` = Right, `-X` = Left
/// - `+Y` = Front, `-Y` = Back
/// - `+Z` = Up (Zenith), `-Z` = Down (Nadir)
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Vec3 {
    pub x: f64,
    pub y: f64,
    pub z: f64,
}

impl Vec3 {
    pub const ZERO: Self = Self {
        x: 0.0,
        y: 0.0,
        z: 0.0,
    };

    #[inline]
    pub const fn new(x: f64, y: f64, z: f64) -> Self {
        Self { x, y, z }
    }

    #[inline]
    pub fn dot(self, rhs: Self) -> f64 {
        self.x * rhs.x + self.y * rhs.y + self.z * rhs.z
    }

    #[inline]
    pub fn cross(self, rhs: Self) -> Self {
        Self {
            x: self.y * rhs.z - self.z * rhs.y,
            y: self.z * rhs.x - self.x * rhs.z,
            z: self.x * rhs.y - self.y * rhs.x,
        }
    }

    #[inline]
    pub fn length_sq(self) -> f64 {
        self.dot(self)
    }

    #[inline]
    pub fn length(self) -> f64 {
        self.length_sq().sqrt()
    }

    /// Returns a unit vector along `self`, or `None` when length is near zero.
    #[inline]
    pub fn normalized(self) -> Option<Self> {
        let len = self.length();
        if len <= 1e-12 || !len.is_finite() {
            None
        } else {
            let inv = 1.0 / len;
            Some(Self::new(self.x * inv, self.y * inv, self.z * inv))
        }
    }

    /// Converts ITU-R BS.2076 / Pulkki spherical coordinates to Cartesian.
    pub fn from_spherical(coord: SphericalCoord) -> Self {
        let az = coord.azimuth_deg.to_radians();
        let el = coord.elevation_deg.to_radians();
        let cos_el = el.cos();
        Self {
            x: -coord.radius * cos_el * az.sin(),
            y: coord.radius * cos_el * az.cos(),
            z: coord.radius * el.sin(),
        }
    }

    /// Converts Cartesian coordinates to ITU-R BS.2076 / Pulkki spherical coordinates.
    pub fn to_spherical(self) -> SphericalCoord {
        let r = self.length();
        if r <= 1e-12 {
            return SphericalCoord::new(0.0, 0.0, 0.0);
        }
        let az = (-self.x).atan2(self.y).to_degrees();
        let el = (self.z / r).clamp(-1.0, 1.0).asin().to_degrees();
        SphericalCoord::new(az, el, r)
    }
}

impl Add for Vec3 {
    type Output = Self;

    #[inline]
    fn add(self, rhs: Self) -> Self {
        Self::new(self.x + rhs.x, self.y + rhs.y, self.z + rhs.z)
    }
}

impl Sub for Vec3 {
    type Output = Self;

    #[inline]
    fn sub(self, rhs: Self) -> Self {
        Self::new(self.x - rhs.x, self.y - rhs.y, self.z - rhs.z)
    }
}

impl Neg for Vec3 {
    type Output = Self;

    #[inline]
    fn neg(self) -> Self {
        Self::new(-self.x, -self.y, -self.z)
    }
}

impl Mul<f64> for Vec3 {
    type Output = Self;

    #[inline]
    fn mul(self, rhs: f64) -> Self {
        Self::new(self.x * rhs, self.y * rhs, self.z * rhs)
    }
}

impl Mul<Vec3> for f64 {
    type Output = Vec3;

    #[inline]
    fn mul(self, rhs: Vec3) -> Vec3 {
        rhs * self
    }
}

/// Spherical coordinates following ITU-R BS.2076 / Pulkki VBAP conventions.
///
/// - `azimuth_deg`: `0` = Front (`+Y`), `+90` = Left (`-X`), `-90` = Right (`+X`), `+-180` = Back (`-Y`)
/// - `elevation_deg`: `0` = Horizon, `+90` = Zenith (`+Z`), `-90` = Nadir (`-Z`)
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct SphericalCoord {
    pub azimuth_deg: f64,
    pub elevation_deg: f64,
    pub radius: f64,
}

impl SphericalCoord {
    #[inline]
    pub const fn new(azimuth_deg: f64, elevation_deg: f64, radius: f64) -> Self {
        Self {
            azimuth_deg,
            elevation_deg,
            radius,
        }
    }
}

/// Column-major 3x3 matrix (`cols[col][row]`).
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Mat3 {
    pub cols: [[f64; 3]; 3],
}

impl Mat3 {
    #[inline]
    pub const fn from_cols(c0: Vec3, c1: Vec3, c2: Vec3) -> Self {
        Self {
            cols: [[c0.x, c0.y, c0.z], [c1.x, c1.y, c1.z], [c2.x, c2.y, c2.z]],
        }
    }

    #[inline]
    pub fn determinant(&self) -> f64 {
        let a = self.cols[0];
        let b = self.cols[1];
        let c = self.cols[2];

        a[0] * (b[1] * c[2] - b[2] * c[1]) - b[0] * (a[1] * c[2] - a[2] * c[1])
            + c[0] * (a[1] * b[2] - a[2] * b[1])
    }

    /// Computes the closed-form inverse, returning `None` for singular matrices.
    pub fn inverse(&self) -> Option<Self> {
        let det = self.determinant();
        if det.abs() <= 1e-12 || !det.is_finite() {
            return None;
        }
        let inv_det = 1.0 / det;

        let c0 = Vec3::new(self.cols[0][0], self.cols[0][1], self.cols[0][2]);
        let c1 = Vec3::new(self.cols[1][0], self.cols[1][1], self.cols[1][2]);
        let c2 = Vec3::new(self.cols[2][0], self.cols[2][1], self.cols[2][2]);

        // Rows of adjugate(M) = cross products of columns (transposed cofactors).
        let r0 = c1.cross(c2) * inv_det;
        let r1 = c2.cross(c0) * inv_det;
        let r2 = c0.cross(c1) * inv_det;

        Some(Self {
            cols: [[r0.x, r1.x, r2.x], [r0.y, r1.y, r2.y], [r0.z, r1.z, r2.z]],
        })
    }

    #[inline]
    pub fn mul_vec3(&self, v: Vec3) -> Vec3 {
        let a = self.cols[0];
        let b = self.cols[1];
        let c = self.cols[2];
        Vec3::new(
            a[0] * v.x + b[0] * v.y + c[0] * v.z,
            a[1] * v.x + b[1] * v.y + c[1] * v.z,
            a[2] * v.x + b[2] * v.y + c[2] * v.z,
        )
    }
}

/// Cubic Hermite interpolation clamped between `edge0` and `edge1`.
#[inline]
pub fn smoothstep(edge0: f64, edge1: f64, x: f64) -> f64 {
    if (edge1 - edge0).abs() <= 1e-12 {
        return if x >= edge1 { 1.0 } else { 0.0 };
    }
    let t = ((x - edge0) / (edge1 - edge0)).clamp(0.0, 1.0);
    t * t * (3.0 - 2.0 * t)
}

#[cfg(test)]
mod tests {
    use super::*;
    use approx::assert_relative_eq;

    #[test]
    fn spherical_cardinal_directions() {
        let front = Vec3::from_spherical(SphericalCoord::new(0.0, 0.0, 1.0));
        assert_relative_eq!(front.x, 0.0, epsilon = 1e-12);
        assert_relative_eq!(front.y, 1.0, epsilon = 1e-12);
        assert_relative_eq!(front.z, 0.0, epsilon = 1e-12);

        let left = Vec3::from_spherical(SphericalCoord::new(90.0, 0.0, 1.0));
        assert_relative_eq!(left.x, -1.0, epsilon = 1e-12);
        assert_relative_eq!(left.y, 0.0, epsilon = 1e-12);
        assert_relative_eq!(left.z, 0.0, epsilon = 1e-12);

        let right = Vec3::from_spherical(SphericalCoord::new(-90.0, 0.0, 1.0));
        assert_relative_eq!(right.x, 1.0, epsilon = 1e-12);
        assert_relative_eq!(right.y, 0.0, epsilon = 1e-12);
        assert_relative_eq!(right.z, 0.0, epsilon = 1e-12);

        let zenith = Vec3::from_spherical(SphericalCoord::new(0.0, 90.0, 1.0));
        assert_relative_eq!(zenith.x, 0.0, epsilon = 1e-12);
        assert_relative_eq!(zenith.y, 0.0, epsilon = 1e-12);
        assert_relative_eq!(zenith.z, 1.0, epsilon = 1e-12);
    }

    #[test]
    fn spherical_cartesian_round_trip() {
        let angles = [
            (0.0, 0.0),
            (30.0, 0.0),
            (-45.0, 35.0),
            (135.0, -25.0),
            (-160.0, 45.0),
            (0.0, 90.0),
            (0.0, -90.0),
        ];
        for (az, el) in angles {
            let v = Vec3::from_spherical(SphericalCoord::new(az, el, 1.5));
            let s = v.to_spherical();
            let v2 = Vec3::from_spherical(s);
            assert_relative_eq!(v.x, v2.x, epsilon = 1e-12);
            assert_relative_eq!(v.y, v2.y, epsilon = 1e-12);
            assert_relative_eq!(v.z, v2.z, epsilon = 1e-12);
        }
    }

    #[test]
    fn mat3_inverse_and_singular_rejection() {
        let m = Mat3::from_cols(
            Vec3::new(2.0, 1.0, 0.0),
            Vec3::new(0.0, 3.0, -1.0),
            Vec3::new(1.0, 0.0, 4.0),
        );
        let inv = m.inverse().expect("invertible matrix");
        let v = Vec3::new(0.5, -0.3, 0.8);
        let round_trip = m.mul_vec3(inv.mul_vec3(v));
        assert_relative_eq!(round_trip.x, v.x, epsilon = 1e-12);
        assert_relative_eq!(round_trip.y, v.y, epsilon = 1e-12);
        assert_relative_eq!(round_trip.z, v.z, epsilon = 1e-12);

        let singular = Mat3::from_cols(
            Vec3::new(1.0, 0.0, 0.0),
            Vec3::new(0.0, 1.0, 0.0),
            Vec3::new(1.0, 1.0, 0.0),
        );
        assert!(singular.inverse().is_none());
    }
}
