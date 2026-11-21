use std::io::Cursor;
use std::sync::{Arc, Mutex};

use approx::assert_relative_eq;
use proptest::prelude::*;

use objrender::importers::caf::SharedCafStream;
use objrender::importers::damf::DamfImporter;
use objrender::io::MemorySink;
use objrender::layout::{SpeakerLayout, SpeakerRole};
use objrender::math::{SphericalCoord, Vec3};
use objrender::mixer::{ClippingPolicy, LfePolicy, RenderConfig, RenderError, render_scene};
use objrender::renderer::{DEFAULT_NEAR_ORIGIN_RADIUS, OutOfHullMode, Spatializer, VbapRenderer};
use objrender::scene::{
    AudioObject, InMemorySource, InterpMode, Keyframe, ObjectId, ObjectRole, SceneBuilder,
    SineSource, SpatialParams,
};

fn build_caf_24le(sample_rate: u32, channels_pcm: &[Vec<f64>]) -> Vec<u8> {
    let n_ch = channels_pcm.len();
    let n_frames = channels_pcm[0].len();

    let mut pcm = Vec::with_capacity(n_ch * n_frames * 3);
    for f in 0..n_frames {
        for ch in channels_pcm {
            let clamped = ch[f].clamp(-1.0, 1.0 - 1e-7);
            let q = (clamped * 8_388_608.0).round() as i32;
            let bytes = q.to_le_bytes();
            pcm.extend_from_slice(&bytes[0..3]);
        }
    }

    let mut out = Vec::new();
    out.extend_from_slice(b"caff");
    out.extend_from_slice(&1u16.to_be_bytes());
    out.extend_from_slice(&0u16.to_be_bytes());

    out.extend_from_slice(b"desc");
    out.extend_from_slice(&32i64.to_be_bytes());
    out.extend_from_slice(&(sample_rate as f64).to_be_bytes());
    out.extend_from_slice(b"lpcm");
    out.extend_from_slice(&2u32.to_be_bytes());
    out.extend_from_slice(&((n_ch * 3) as u32).to_be_bytes());
    out.extend_from_slice(&1u32.to_be_bytes());
    out.extend_from_slice(&(n_ch as u32).to_be_bytes());
    out.extend_from_slice(&24u32.to_be_bytes());

    out.extend_from_slice(b"data");
    out.extend_from_slice(&(4 + pcm.len() as i64).to_be_bytes());
    out.extend_from_slice(&0u32.to_be_bytes());
    out.extend_from_slice(&pcm);
    out
}

fn render_pass_through_origin(
    layout: &SpeakerLayout,
    start_pos: Vec3,
    end_pos: Vec3,
    near_origin_radius: f64,
    total_frames: u64,
    block_size: usize,
) -> Vec<Vec<f64>> {
    let vbap = VbapRenderer::build(layout)
        .unwrap()
        .with_near_origin_radius(near_origin_radius);

    let mut obj = AudioObject::static_mono(
        ObjectId(1),
        "origin_cross",
        ObjectRole::Dynamic,
        start_pos,
        1.0,
        Box::new(InMemorySource::new(vec![1.0; total_frames as usize])),
    );
    obj.keyframes = vec![
        Keyframe {
            frame: 0,
            position: start_pos,
            gain: 1.0,
            spatial: SpatialParams::default(),
            interp: InterpMode::Linear,
            discontinuous: false,
        },
        Keyframe {
            frame: total_frames - 1,
            position: end_pos,
            gain: 1.0,
            spatial: SpatialParams::default(),
            interp: InterpMode::Step,
            discontinuous: false,
        },
    ];

    let mut scene = SceneBuilder::new(48_000, total_frames)
        .add_object(obj)
        .build()
        .unwrap();

    let captured = Arc::new(Mutex::new(Vec::new()));
    struct Capture(Arc<Mutex<Vec<Vec<f64>>>>, MemorySink);
    impl objrender::io::SampleWriter for Capture {
        fn write_block(
            &mut self,
            bus: &objrender::io::PcmBus,
            frames: usize,
        ) -> Result<(), objrender::io::IoError> {
            self.1.write_block(bus, frames)
        }
        fn finalize(
            self: Box<Self>,
        ) -> Result<objrender::io::OutputMetadata, objrender::io::IoError> {
            *self.0.lock().unwrap() = self.1.channels;
            Ok(objrender::io::OutputMetadata {
                rf64_promoted: false,
                frames_written: self.1.frames_written,
                total_bytes: 0,
            })
        }
    }

    render_scene(
        &mut scene,
        layout,
        &vbap,
        RenderConfig {
            block_size,
            crossfade_frames: 0,
            clipping: ClippingPolicy::PassThrough,
            lfe_policy: LfePolicy::DropIfAbsent,
            ..RenderConfig::default()
        },
        Box::new(Capture(
            Arc::clone(&captured),
            MemorySink::new(layout.speakers.len()),
        )),
    )
    .unwrap();

    Arc::try_unwrap(captured).unwrap().into_inner().unwrap()
}

#[test]
fn near_origin_linear_pass_through_is_continuous_and_conserves_power() {
    // 8193 frames (~0.17 s) so exact midpoint frame 4096 lands on a block_size=64 boundary.
    let total_frames = 8193u64;
    let mid_frame = 4096usize;

    // 7.1.4 Front-to-Back crossing through exact origin with default block_size=64.
    let layout_714 = SpeakerLayout::layout_7_1_4();
    let smooth_714 = render_pass_through_origin(
        &layout_714,
        Vec3::new(0.0, 1.0, 0.0),
        Vec3::new(0.0, -1.0, 0.0),
        DEFAULT_NEAR_ORIGIN_RADIUS,
        total_frames,
        64,
    );
    // Per-sample evaluation (block_size=1) exposes the true spatial discontinuity when disabled.
    let smooth_per_sample = render_pass_through_origin(
        &layout_714,
        Vec3::new(0.0, 1.0, 0.0),
        Vec3::new(0.0, -1.0, 0.0),
        DEFAULT_NEAR_ORIGIN_RADIUS,
        total_frames,
        1,
    );
    let sharp_per_sample = render_pass_through_origin(
        &layout_714,
        Vec3::new(0.0, 1.0, 0.0),
        Vec3::new(0.0, -1.0, 0.0),
        0.0,
        total_frames,
        1,
    );

    let omni_714 = 1.0 / (11.0_f64).sqrt();
    for (ch, spk) in layout_714.speakers.iter().enumerate() {
        let expected = if spk.role == SpeakerRole::Lfe {
            0.0
        } else {
            omni_714
        };
        assert_relative_eq!(smooth_714[ch][mid_frame], expected, epsilon = 1e-6);
    }

    let max_delta = |pcm: &[Vec<f64>]| {
        pcm.iter()
            .flat_map(|ch| ch.windows(2).map(|w| (w[1] - w[0]).abs()))
            .fold(0.0_f64, f64::max)
    };
    let max_delta_smooth_64 = max_delta(&smooth_714);
    let max_delta_smooth_1 = max_delta(&smooth_per_sample);
    let max_delta_sharp_1 = max_delta(&sharp_per_sample);
    assert!(
        max_delta_smooth_64 < 0.015 && max_delta_smooth_1 < 0.015,
        "expected smooth near-origin crossing on 7.1.4, got block64={max_delta_smooth_64}, block1={max_delta_smooth_1}"
    );
    assert!(
        max_delta_sharp_1 > 0.5,
        "disabled near-origin blend should jump > 0.5 across origin (got {max_delta_sharp_1})"
    );

    for f in 0..(total_frames as usize) {
        let exact_power: f64 = smooth_per_sample.iter().map(|ch| ch[f] * ch[f]).sum();
        assert!(
            (exact_power - 1.0).abs() < 1e-6,
            "exact VBAP power drift at frame {f}: sum_sq={exact_power}"
        );
        let power: f64 = smooth_714.iter().map(|ch| ch[f] * ch[f]).sum();
        assert!(
            (0.95..=1.0001).contains(&power),
            "power out of bounds at frame {f}: sum_sq={power}"
        );
    }

    // Stereo Left-to-Right crossing through exact origin.
    let stereo = SpeakerLayout::stereo();
    let smooth_stereo = render_pass_through_origin(
        &stereo,
        Vec3::new(-1.0, 0.0, 0.0),
        Vec3::new(1.0, 0.0, 0.0),
        DEFAULT_NEAR_ORIGIN_RADIUS,
        total_frames,
        64,
    );
    assert_relative_eq!(
        smooth_stereo[0][mid_frame],
        std::f64::consts::FRAC_1_SQRT_2,
        epsilon = 1e-6
    );
    assert_relative_eq!(
        smooth_stereo[1][mid_frame],
        std::f64::consts::FRAC_1_SQRT_2,
        epsilon = 1e-6
    );
    let max_delta_stereo = max_delta(&smooth_stereo);
    assert!(max_delta_stereo < 0.015);
    for f in 0..(total_frames as usize) {
        let power: f64 = smooth_stereo.iter().map(|ch| ch[f] * ch[f]).sum();
        assert!((0.95..=1.0001).contains(&power));
    }
}

#[test]
fn divergence_blends_direction_toward_omnidirectional_while_conserving_power() {
    let layout = SpeakerLayout::layout_7_1_4();
    let vbap = VbapRenderer::build(&layout).unwrap();
    let pos_l = layout.speakers[0].position;
    let mut gains = vec![0.0_f64; layout.speakers.len()];

    // div = 0.0 -> pure L
    vbap.compute_gains(
        pos_l,
        SpatialParams {
            width: 0.0,
            divergence: 0.0,
        },
        &mut gains,
    );
    assert_relative_eq!(gains[0], 1.0, epsilon = 1e-6);
    for (idx, &g) in gains.iter().enumerate().skip(1) {
        assert_relative_eq!(g, 0.0, epsilon = 1e-6, max_relative = 1e-6);
        let _ = idx;
    }

    // div = 0.5 -> half directional power, half omnidirectional power across 11 full-range speakers
    vbap.compute_gains(
        pos_l,
        SpatialParams {
            width: 0.0,
            divergence: 0.5,
        },
        &mut gains,
    );
    let expected_l = (0.5 + 0.5 / 11.0_f64).sqrt();
    let expected_other = (0.5 / 11.0_f64).sqrt();
    assert_relative_eq!(gains[0], expected_l, epsilon = 1e-6);
    for (idx, spk) in layout.speakers.iter().enumerate().skip(1) {
        if spk.role == SpeakerRole::Lfe {
            assert_relative_eq!(gains[idx], 0.0, epsilon = 1e-12);
        } else {
            assert_relative_eq!(gains[idx], expected_other, epsilon = 1e-6);
        }
    }
    let power_half: f64 = gains.iter().map(|g| g * g).sum();
    assert_relative_eq!(power_half, 1.0, epsilon = 1e-6);

    // div = 1.0 -> fully omnidirectional across 11 full-range speakers
    vbap.compute_gains(
        pos_l,
        SpatialParams {
            width: 0.0,
            divergence: 1.0,
        },
        &mut gains,
    );
    let expected_omni = 1.0 / (11.0_f64).sqrt();
    for (idx, spk) in layout.speakers.iter().enumerate() {
        if spk.role == SpeakerRole::Lfe {
            assert_relative_eq!(gains[idx], 0.0, epsilon = 1e-12);
        } else {
            assert_relative_eq!(gains[idx], expected_omni, epsilon = 1e-6);
        }
    }
}

#[test]
fn out_of_hull_modes_behave_distinctly_below_7_1_4_horizon() {
    let layout = SpeakerLayout::layout_7_1_4();
    let pos_below = Vec3::from_spherical(SphericalCoord::new(30.0, -45.0, 1.0));
    let pos_nadir = Vec3::new(0.0, 0.0, -1.0);
    let params = SpatialParams::default();

    let vp = VbapRenderer::build_with_mode(&layout, OutOfHullMode::VirtualPoles).unwrap();
    let blend =
        VbapRenderer::build_with_mode(&layout, OutOfHullMode::Blend { power: 12.0 }).unwrap();
    let fade = VbapRenderer::build_with_mode(&layout, OutOfHullMode::Fade).unwrap();

    for (name, renderer) in [("VirtualPoles", &vp), ("Blend", &blend)] {
        for pos in [pos_below, pos_nadir] {
            let mut gains = vec![0.0_f64; layout.speakers.len()];
            renderer.compute_gains(pos, params, &mut gains);
            let sum_sq: f64 = gains.iter().map(|g| g * g).sum();
            assert_relative_eq!(sum_sq, 1.0, epsilon = 1e-6);
            assert_relative_eq!(gains[3], 0.0, epsilon = 1e-12);
        }
        let _ = name;
    }

    let mut blend_below = vec![0.0_f64; layout.speakers.len()];
    blend.compute_gains(pos_below, params, &mut blend_below);
    assert!(
        blend_below[0] > 0.7,
        "Blend mode should keep L (ch 0) strongly dominant at (30, -45), got {}",
        blend_below[0]
    );

    let mut fade_below = vec![0.0_f64; layout.speakers.len()];
    fade.compute_gains(pos_below, params, &mut fade_below);
    let fade_energy_below: f64 = fade_below.iter().map(|g| g * g).sum();
    assert!(
        fade_energy_below < 0.85,
        "Fade mode should attenuate below hull, got energy {fade_energy_below}"
    );

    let mut fade_nadir = vec![0.0_f64; layout.speakers.len()];
    fade.compute_gains(pos_nadir, params, &mut fade_nadir);
    let fade_energy_nadir: f64 = fade_nadir.iter().map(|g| g * g).sum();
    assert!(
        fade_energy_nadir < 1e-4,
        "Fade mode should reach near-silence at uncovered nadir, got {fade_energy_nadir}"
    );
}

#[test]
fn two_pass_peak_normalize_hits_exact_target_dbfs_and_preserves_spatial_ratios() {
    let n_frames = 512usize;
    let sr = 48_000u32;
    let caf_bytes = build_caf_24le(sr, &[vec![0.4; n_frames], vec![0.25; n_frames]]);
    let shared = SharedCafStream::from_reader(Cursor::new(caf_bytes)).unwrap();

    let manifest_yaml = r#"version: 0.5.1
presentations:
  - type: home
    simplified: false
    metadata: norm.atmos.metadata
    audio: norm.atmos.audio
    offset: 0.0
    objects:
      - ID: 10
      - ID: 11
"#;
    let metadata_yaml = r#"sampleRate: 48000
events:
  - ID: 10
    samplePos: 0
    active: true
    pos: [-1, 1, 0]
    gain: -3
    rampLength: 0
  - ID: 11
    samplePos: 0
    active: true
    pos: [1, -1, 0]
    gain: 0
    rampLength: 0
"#;

    let mut scene = DamfImporter::from_parts(manifest_yaml, metadata_yaml, shared).unwrap();
    let layout = SpeakerLayout::layout_7_1_4();
    let vbap = VbapRenderer::build(&layout).unwrap();

    let report_pass = render_scene(
        &mut scene,
        &layout,
        &vbap,
        RenderConfig {
            clipping: ClippingPolicy::PassThrough,
            ..RenderConfig::default()
        },
        Box::new(MemorySink::new(layout.speakers.len())),
    )
    .unwrap();

    for target_dbfs in [-1.0_f64, -6.0_f64] {
        let report_norm = render_scene(
            &mut scene,
            &layout,
            &vbap,
            RenderConfig {
                clipping: ClippingPolicy::TwoPassPeakNormalize { target_dbfs },
                ..RenderConfig::default()
            },
            Box::new(MemorySink::new(layout.speakers.len())),
        )
        .unwrap();

        let max_peak_dbfs = report_norm
            .channels
            .iter()
            .map(|c| c.peak_dbfs())
            .fold(f64::NEG_INFINITY, f64::max);
        assert!(
            (max_peak_dbfs - target_dbfs).abs() < 0.01,
            "expected global peak {target_dbfs} dBFS, got {max_peak_dbfs}"
        );

        let raw_max_peak_dbfs = report_pass
            .channels
            .iter()
            .map(|c| c.peak_dbfs())
            .fold(f64::NEG_INFINITY, f64::max);
        assert_relative_eq!(
            report_norm.applied_gain_db(),
            target_dbfs - raw_max_peak_dbfs,
            epsilon = 1e-4
        );

        // Inter-channel ratios between L (ch 0) and Rrs (ch 5) must stay identical.
        let ratio_pass = report_pass.channels[5].peak_linear / report_pass.channels[0].peak_linear;
        let ratio_norm = report_norm.channels[5].peak_linear / report_norm.channels[0].peak_linear;
        assert_relative_eq!(ratio_norm, ratio_pass, epsilon = 1e-5);
    }

    // Silent scene normalizes cleanly with unit gain and no NaN.
    let mut silent_scene = SceneBuilder::new(sr, 128)
        .add_object(AudioObject::static_mono(
            ObjectId(1),
            "silent",
            ObjectRole::Dynamic,
            Vec3::new(0.0, 1.0, 0.0),
            1.0,
            Box::new(SineSource::new(sr, 440.0, 0.0, 128)),
        ))
        .build()
        .unwrap();
    let silent_report = render_scene(
        &mut silent_scene,
        &layout,
        &vbap,
        RenderConfig {
            clipping: ClippingPolicy::TwoPassPeakNormalize { target_dbfs: -1.0 },
            ..RenderConfig::default()
        },
        Box::new(MemorySink::new(layout.speakers.len())),
    )
    .unwrap();
    assert_relative_eq!(silent_report.applied_gain_linear, 1.0, epsilon = 1e-12);
    assert!(silent_report.channels.iter().all(|c| c.peak_linear == 0.0));

    // Invalid target > 0 dBFS or NaN is rejected.
    for bad in [1.0_f64, f64::NAN] {
        let err = render_scene(
            &mut scene,
            &layout,
            &vbap,
            RenderConfig {
                clipping: ClippingPolicy::TwoPassPeakNormalize { target_dbfs: bad },
                ..RenderConfig::default()
            },
            Box::new(MemorySink::new(layout.speakers.len())),
        );
        assert!(matches!(err, Err(RenderError::InvalidConfig(_))));
    }
}

proptest! {
    #[test]
    fn phase3_vbap_invariants_with_near_origin_divergence_and_all_hull_modes(
        x in -1.2_f64..=1.2,
        y in -1.2_f64..=1.2,
        z in -1.2_f64..=1.2,
        width in prop::sample::select(vec![0.0_f64, 0.35, 0.9]),
        divergence in prop::sample::select(vec![0.0_f64, 0.4, 1.0]),
    ) {
        let custom_3d = SpeakerLayout::from_toml_str(r#"
            name = "custom-3d-dome"
            [[speakers]]
            label = "FL"
            azimuth_deg = 32.0
            elevation_deg = 0.0
            [[speakers]]
            label = "FR"
            azimuth_deg = -32.0
            elevation_deg = 0.0
            [[speakers]]
            label = "SL"
            azimuth_deg = 105.0
            elevation_deg = 5.0
            [[speakers]]
            label = "SR"
            azimuth_deg = -105.0
            elevation_deg = 5.0
            [[speakers]]
            label = "TFL"
            azimuth_deg = 45.0
            elevation_deg = 45.0
            [[speakers]]
            label = "TFR"
            azimuth_deg = -45.0
            elevation_deg = 45.0
            [[speakers]]
            label = "LFE"
            role = "lfe"
            x = 0.0
            y = 0.0
            z = 0.0
        "#).unwrap();

        let layouts = [
            SpeakerLayout::stereo(),
            SpeakerLayout::surround_5_1(),
            SpeakerLayout::layout_7_1_4(),
            custom_3d,
        ];
        let modes = [
            OutOfHullMode::VirtualPoles,
            OutOfHullMode::Blend { power: 12.0 },
            OutOfHullMode::Fade,
        ];

        let pos = Vec3::new(x, y, z);
        let params = SpatialParams { width, divergence };

        for layout in &layouts {
            for &mode in &modes {
                let vbap = VbapRenderer::build_with_mode(layout, mode).unwrap();
                let mut gains = vec![0.0_f64; layout.speakers.len()];
                vbap.compute_gains(pos, params, &mut gains);

                let mut sum_sq = 0.0;
                for (idx, &g) in gains.iter().enumerate() {
                    prop_assert!(g.is_finite(), "non-finite gain on {} ch {}", layout.name, idx);
                    prop_assert!(g >= 0.0, "negative gain {g} on {} ch {}", layout.name, idx);
                    if layout.speakers[idx].role == SpeakerRole::Lfe {
                        prop_assert_eq!(g, 0.0, "LFE channel received non-zero VBAP gain");
                    }
                    sum_sq += g * g;
                }

                if matches!(mode, OutOfHullMode::Fade) {
                    prop_assert!(
                        (0.0..=1.0 + 1e-5).contains(&sum_sq),
                        "Fade energy out of [0, 1] on {}: sum_sq={sum_sq}",
                        layout.name
                    );
                } else {
                    prop_assert!(
                        (sum_sq - 1.0).abs() < 1e-5,
                        "Energy not conserved on {} ({mode:?}): sum_sq={sum_sq}",
                        layout.name
                    );
                }
            }
        }
    }
}
