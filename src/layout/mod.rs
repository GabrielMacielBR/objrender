//! Arbitrary 3D speaker layout geometry, validation, standard presets, and TOML parsing.

use std::fs;
use std::path::Path;

use serde::Deserialize;
use thiserror::Error;

use crate::math::{SphericalCoord, Vec3};

/// Zero-based index of a speaker inside a [`SpeakerLayout`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct SpeakerId(pub usize);

/// Acoustic role determining whether a speaker participates in spatial panning.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SpeakerRole {
    FullRange,
    Lfe,
}

/// Physical or virtual reproduction loudspeaker positioned in 3D space.
#[derive(Debug, Clone, PartialEq)]
pub struct Speaker {
    pub id: SpeakerId,
    pub label: String,
    pub position: Vec3,
    pub role: SpeakerRole,
    pub channel_mask_bit: Option<u32>,
}

/// Arbitrary speaker configuration without fixed channel count assumptions.
#[derive(Debug, Clone)]
pub struct SpeakerLayout {
    pub name: String,
    pub speakers: Vec<Speaker>,
}

#[derive(Debug, Error)]
pub enum LayoutError {
    #[error("layout `{0}` must contain at least one full-range speaker")]
    NoFullRangeSpeakers(String),
    #[error("speaker `{label}` has near-zero position vector")]
    ZeroPosition { label: String },
    #[error("speakers `{first}` and `{second}` occupy coincident directions (< 1 deg)")]
    CoincidentSpeakers { first: String, second: String },
    #[error("failed to read layout file `{path}`: {source}")]
    Io {
        path: String,
        #[source]
        source: std::io::Error,
    },
    #[error("invalid layout TOML: {0}")]
    TomlParse(#[from] toml::de::Error),
    #[error("speaker `{label}` must specify either (azimuth_deg, elevation_deg) or (x, y, z)")]
    MissingCoordinates { label: String },
    #[error("failed to triangulate speaker layout `{0}`")]
    TriangulationFailed(String),
}

// Standard WAVE_FORMAT_EXTENSIBLE speaker mask bits.
const MASK_FL: u32 = 0x0000_0001;
const MASK_FR: u32 = 0x0000_0002;
const MASK_FC: u32 = 0x0000_0004;
const MASK_LFE: u32 = 0x0000_0008;
const MASK_BL: u32 = 0x0000_0010;
const MASK_BR: u32 = 0x0000_0020;
const MASK_SL: u32 = 0x0000_0200;
const MASK_SR: u32 = 0x0000_0400;
const MASK_TFL: u32 = 0x0000_1000;
const MASK_TFR: u32 = 0x0000_4000;
const MASK_TBL: u32 = 0x0000_8000;
const MASK_TBR: u32 = 0x0002_0000;

impl SpeakerLayout {
    pub fn new(name: impl Into<String>, mut speakers: Vec<Speaker>) -> Result<Self, LayoutError> {
        for (idx, spk) in speakers.iter_mut().enumerate() {
            spk.id = SpeakerId(idx);
        }
        let layout = Self {
            name: name.into(),
            speakers,
        };
        layout.validate()?;
        Ok(layout)
    }

    /// Ensures at least one valid full-range speaker and no coincident directions.
    pub fn validate(&self) -> Result<(), LayoutError> {
        let mut full_range_dirs: Vec<(&str, Vec3)> = Vec::new();
        let min_sep_cos = 1.0_f64.to_radians().cos();

        for spk in &self.speakers {
            if spk.role != SpeakerRole::FullRange {
                continue;
            }
            let dir = spk
                .position
                .normalized()
                .ok_or_else(|| LayoutError::ZeroPosition {
                    label: spk.label.clone(),
                })?;
            for &(prev_label, prev_dir) in &full_range_dirs {
                if dir.dot(prev_dir) > min_sep_cos {
                    return Err(LayoutError::CoincidentSpeakers {
                        first: prev_label.to_string(),
                        second: spk.label.clone(),
                    });
                }
            }
            full_range_dirs.push((&spk.label, dir));
        }

        if full_range_dirs.is_empty() {
            return Err(LayoutError::NoFullRangeSpeakers(self.name.clone()));
        }

        Ok(())
    }

    /// Computes a `WAVE_FORMAT_EXTENSIBLE` channel mask when every channel specifies
    /// a unique bit in strictly ascending order; otherwise returns `0` (arbitrary order).
    pub fn channel_mask(&self) -> u32 {
        let mut mask = 0u32;
        let mut last_bit = 0u32;
        for spk in &self.speakers {
            let Some(bit) = spk.channel_mask_bit else {
                return 0;
            };
            if bit == 0 || !bit.is_power_of_two() || bit <= last_bit {
                return 0;
            }
            mask |= bit;
            last_bit = bit;
        }
        mask
    }

    pub fn from_toml_str(input: &str) -> Result<Self, LayoutError> {
        let raw: TomlLayout = toml::from_str(input)?;
        let mut speakers = Vec::with_capacity(raw.speakers.len());
        for (idx, entry) in raw.speakers.into_iter().enumerate() {
            let pos = match (
                entry.azimuth_deg,
                entry.elevation_deg,
                entry.x,
                entry.y,
                entry.z,
            ) {
                (Some(az), Some(el), None, None, None) => {
                    Vec3::from_spherical(SphericalCoord::new(az, el, entry.radius.unwrap_or(1.0)))
                }
                (None, None, Some(x), Some(y), Some(z)) => Vec3::new(x, y, z),
                _ => return Err(LayoutError::MissingCoordinates { label: entry.label }),
            };
            speakers.push(Speaker {
                id: SpeakerId(idx),
                label: entry.label,
                position: pos,
                role: entry.role.unwrap_or(SpeakerRole::FullRange),
                channel_mask_bit: entry.channel_mask_bit,
            });
        }
        Self::new(raw.name, speakers)
    }

    pub fn from_toml_file(path: impl AsRef<Path>) -> Result<Self, LayoutError> {
        let path_ref = path.as_ref();
        let text = fs::read_to_string(path_ref).map_err(|source| LayoutError::Io {
            path: path_ref.display().to_string(),
            source,
        })?;
        Self::from_toml_str(&text)
    }

    /// Looks up a standard layout preset by case-insensitive name.
    pub fn preset(name: &str) -> Option<Self> {
        match name.to_ascii_lowercase().as_str() {
            "mono" | "1.0" => Some(Self::mono()),
            "stereo" | "2.0" => Some(Self::stereo()),
            "5.1" => Some(Self::surround_5_1()),
            "7.1" => Some(Self::surround_7_1()),
            "5.1.4" => Some(Self::layout_5_1_4()),
            "7.1.4" => Some(Self::layout_7_1_4()),
            "7.1.6" => Some(Self::layout_7_1_6()),
            "9.1.6" => Some(Self::layout_9_1_6()),
            _ => None,
        }
    }

    pub fn mono() -> Self {
        Self::new("1.0", vec![sph(0, "C", 0.0, 0.0, Some(MASK_FC))]).expect("valid mono preset")
    }

    pub fn stereo() -> Self {
        Self::new(
            "2.0",
            vec![
                sph(0, "L", 30.0, 0.0, Some(MASK_FL)),
                sph(1, "R", -30.0, 0.0, Some(MASK_FR)),
            ],
        )
        .expect("valid stereo preset")
    }

    pub fn surround_5_1() -> Self {
        Self::new(
            "5.1",
            vec![
                sph(0, "L", 30.0, 0.0, Some(MASK_FL)),
                sph(1, "R", -30.0, 0.0, Some(MASK_FR)),
                sph(2, "C", 0.0, 0.0, Some(MASK_FC)),
                lfe(3, "LFE", Some(MASK_LFE)),
                sph(4, "Ls", 110.0, 0.0, Some(MASK_BL)),
                sph(5, "Rs", -110.0, 0.0, Some(MASK_BR)),
            ],
        )
        .expect("valid 5.1 preset")
    }

    pub fn surround_7_1() -> Self {
        Self::new(
            "7.1",
            vec![
                sph(0, "L", 30.0, 0.0, Some(MASK_FL)),
                sph(1, "R", -30.0, 0.0, Some(MASK_FR)),
                sph(2, "C", 0.0, 0.0, Some(MASK_FC)),
                lfe(3, "LFE", Some(MASK_LFE)),
                sph(4, "Lrs", 140.0, 0.0, Some(MASK_BL)),
                sph(5, "Rrs", -140.0, 0.0, Some(MASK_BR)),
                sph(6, "Lss", 90.0, 0.0, Some(MASK_SL)),
                sph(7, "Rss", -90.0, 0.0, Some(MASK_SR)),
            ],
        )
        .expect("valid 7.1 preset")
    }

    pub fn layout_5_1_4() -> Self {
        Self::new(
            "5.1.4",
            vec![
                sph(0, "L", 30.0, 0.0, Some(MASK_FL)),
                sph(1, "R", -30.0, 0.0, Some(MASK_FR)),
                sph(2, "C", 0.0, 0.0, Some(MASK_FC)),
                lfe(3, "LFE", Some(MASK_LFE)),
                sph(4, "Ls", 110.0, 0.0, Some(MASK_BL)),
                sph(5, "Rs", -110.0, 0.0, Some(MASK_BR)),
                sph(6, "Ltf", 45.0, 45.0, Some(MASK_TFL)),
                sph(7, "Rtf", -45.0, 45.0, Some(MASK_TFR)),
                sph(8, "Ltr", 135.0, 45.0, Some(MASK_TBL)),
                sph(9, "Rtr", -135.0, 45.0, Some(MASK_TBR)),
            ],
        )
        .expect("valid 5.1.4 preset")
    }

    pub fn layout_7_1_4() -> Self {
        Self::new(
            "7.1.4",
            vec![
                sph(0, "L", 30.0, 0.0, Some(MASK_FL)),
                sph(1, "R", -30.0, 0.0, Some(MASK_FR)),
                sph(2, "C", 0.0, 0.0, Some(MASK_FC)),
                lfe(3, "LFE", Some(MASK_LFE)),
                sph(4, "Lrs", 140.0, 0.0, Some(MASK_BL)),
                sph(5, "Rrs", -140.0, 0.0, Some(MASK_BR)),
                sph(6, "Lss", 90.0, 0.0, Some(MASK_SL)),
                sph(7, "Rss", -90.0, 0.0, Some(MASK_SR)),
                sph(8, "Ltf", 45.0, 45.0, Some(MASK_TFL)),
                sph(9, "Rtf", -45.0, 45.0, Some(MASK_TFR)),
                sph(10, "Ltr", 135.0, 45.0, Some(MASK_TBL)),
                sph(11, "Rtr", -135.0, 45.0, Some(MASK_TBR)),
            ],
        )
        .expect("valid 7.1.4 preset")
    }

    pub fn layout_7_1_6() -> Self {
        Self::new(
            "7.1.6",
            vec![
                sph(0, "L", 30.0, 0.0, None),
                sph(1, "R", -30.0, 0.0, None),
                sph(2, "C", 0.0, 0.0, None),
                lfe(3, "LFE", None),
                sph(4, "Lrs", 140.0, 0.0, None),
                sph(5, "Rrs", -140.0, 0.0, None),
                sph(6, "Lss", 90.0, 0.0, None),
                sph(7, "Rss", -90.0, 0.0, None),
                sph(8, "Ltf", 45.0, 45.0, None),
                sph(9, "Rtf", -45.0, 45.0, None),
                sph(10, "Ltm", 90.0, 60.0, None),
                sph(11, "Rtm", -90.0, 60.0, None),
                sph(12, "Ltr", 135.0, 45.0, None),
                sph(13, "Rtr", -135.0, 45.0, None),
            ],
        )
        .expect("valid 7.1.6 preset")
    }

    pub fn layout_9_1_6() -> Self {
        Self::new(
            "9.1.6",
            vec![
                sph(0, "L", 30.0, 0.0, None),
                sph(1, "R", -30.0, 0.0, None),
                sph(2, "C", 0.0, 0.0, None),
                lfe(3, "LFE", None),
                sph(4, "Lw", 60.0, 0.0, None),
                sph(5, "Rw", -60.0, 0.0, None),
                sph(6, "Lss", 90.0, 0.0, None),
                sph(7, "Rss", -90.0, 0.0, None),
                sph(8, "Lrs", 140.0, 0.0, None),
                sph(9, "Rrs", -140.0, 0.0, None),
                sph(10, "Ltf", 45.0, 45.0, None),
                sph(11, "Rtf", -45.0, 45.0, None),
                sph(12, "Ltm", 90.0, 60.0, None),
                sph(13, "Rtm", -90.0, 60.0, None),
                sph(14, "Ltr", 135.0, 45.0, None),
                sph(15, "Rtr", -135.0, 45.0, None),
            ],
        )
        .expect("valid 9.1.6 preset")
    }
}

fn sph(id: usize, label: &str, az: f64, el: f64, mask: Option<u32>) -> Speaker {
    Speaker {
        id: SpeakerId(id),
        label: label.to_string(),
        position: Vec3::from_spherical(SphericalCoord::new(az, el, 1.0)),
        role: SpeakerRole::FullRange,
        channel_mask_bit: mask,
    }
}

fn lfe(id: usize, label: &str, mask: Option<u32>) -> Speaker {
    Speaker {
        id: SpeakerId(id),
        label: label.to_string(),
        position: Vec3::ZERO,
        role: SpeakerRole::Lfe,
        channel_mask_bit: mask,
    }
}

#[derive(Debug, Deserialize)]
struct TomlLayout {
    name: String,
    speakers: Vec<TomlSpeaker>,
}

#[derive(Debug, Deserialize)]
struct TomlSpeaker {
    label: String,
    role: Option<SpeakerRole>,
    azimuth_deg: Option<f64>,
    elevation_deg: Option<f64>,
    radius: Option<f64>,
    x: Option<f64>,
    y: Option<f64>,
    z: Option<f64>,
    channel_mask_bit: Option<u32>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn all_presets_validate_and_have_expected_counts() {
        let cases = [
            ("mono", 1),
            ("stereo", 2),
            ("5.1", 6),
            ("7.1", 8),
            ("5.1.4", 10),
            ("7.1.4", 12),
            ("7.1.6", 14),
            ("9.1.6", 16),
        ];
        for (name, count) in cases {
            let layout = SpeakerLayout::preset(name).expect("preset must exist");
            assert_eq!(layout.speakers.len(), count);
        }
        assert_eq!(SpeakerLayout::surround_5_1().channel_mask(), 0x3F);
        assert_eq!(SpeakerLayout::layout_7_1_4().channel_mask(), 0x2_D63F);
    }

    #[test]
    fn toml_parses_spherical_and_cartesian() {
        let toml = r#"
            name = "custom-5"
            [[speakers]]
            label = "FL"
            azimuth_deg = 35.0
            elevation_deg = 0.0

            [[speakers]]
            label = "FR"
            x = 0.573576
            y = 0.819152
            z = 0.0

            [[speakers]]
            label = "LFE"
            role = "lfe"
            x = 0.0
            y = 0.0
            z = 0.0
        "#;
        let layout = SpeakerLayout::from_toml_str(toml).expect("valid custom TOML");
        assert_eq!(layout.speakers.len(), 3);
        assert_eq!(layout.channel_mask(), 0);
    }

    #[test]
    fn rejects_invalid_layouts() {
        let only_lfe = SpeakerLayout::new("bad", vec![lfe(0, "LFE", None)]);
        assert!(matches!(only_lfe, Err(LayoutError::NoFullRangeSpeakers(_))));

        let duplicate = SpeakerLayout::new(
            "dup",
            vec![sph(0, "A", 30.0, 0.0, None), sph(1, "B", 30.2, 0.0, None)],
        );
        assert!(matches!(
            duplicate,
            Err(LayoutError::CoincidentSpeakers { .. })
        ));
    }
}
