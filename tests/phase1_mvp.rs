use std::io::Cursor;

use approx::assert_relative_eq;
use proptest::prelude::*;

use objrender::importers::SceneImporter;
use objrender::importers::damf::{DamfSignalSpec, DamfStaticDescriptor, DamfStaticImporter};
use objrender::io::{MemorySink, SampleFormat, SampleWriter, WaveConfig, WaveWriter};
use objrender::layout::{SpeakerLayout, SpeakerRole};
use objrender::math::{SphericalCoord, Vec3};
use objrender::mixer::{ClippingPolicy, LfePolicy, RenderConfig, render_scene};
use objrender::renderer::{Spatializer, VbapRenderer};
use objrender::scene::{
    AudioObject, ImpulseSource, ObjectId, ObjectRole, SceneBuilder, SineSource, SpatialParams,
};

fn render_impulse_gains(layout: &SpeakerLayout, pos: Vec3, width: f64) -> Vec<f64> {
    let spatializer = VbapRenderer::build(layout).expect("valid VBAP layout");
    let mut obj = AudioObject::static_mono(
        ObjectId(1),
        "impulse",
        ObjectRole::Dynamic,
        pos,
        1.0,
        Box::new(ImpulseSource::new(10, 1.0, 128)),
    );
    obj.initial_spatial = SpatialParams {
        width,
        divergence: 0.0,
    };

    let mut scene = SceneBuilder::new(48_000, 128)
        .add_object(obj)
        .build()
        .expect("valid scene");

    let sink = Box::new(MemorySink::new(layout.speakers.len()));
    let report = render_scene(
        &mut scene,
        layout,
        &spatializer,
        RenderConfig::default(),
        sink,
    )
    .expect("render succeeds");

    // Peak linear of a unit impulse equals the exact rendered channel gain.
    report.channels.iter().map(|c| c.peak_linear).collect()
}

#[test]
fn speaker_aligned_object_produces_unit_gain_on_target_channel() {
    let layouts = [
        SpeakerLayout::stereo(),
        SpeakerLayout::surround_5_1(),
        SpeakerLayout::layout_7_1_4(),
    ];

    for layout in &layouts {
        for (idx, spk) in layout.speakers.iter().enumerate() {
            if spk.role != SpeakerRole::FullRange {
                continue;
            }
            let gains = render_impulse_gains(layout, spk.position, 0.0);
            for (ch, &g) in gains.iter().enumerate() {
                let expected = if ch == idx { 1.0 } else { 0.0 };
                assert_relative_eq!(g, expected, epsilon = 1e-4);
            }
        }
    }
}

#[test]
fn stereo_center_midpoint_splits_equal_power() {
    let stereo = SpeakerLayout::stereo();
    let front_center = Vec3::from_spherical(SphericalCoord::new(0.0, 0.0, 1.0));
    let gains = render_impulse_gains(&stereo, front_center, 0.0);
    let expected = std::f64::consts::FRAC_1_SQRT_2;
    assert_relative_eq!(gains[0], expected, epsilon = 1e-5);
    assert_relative_eq!(gains[1], expected, epsilon = 1e-5);
}

#[test]
fn overhead_zenith_on_7_1_4_is_symmetric_across_four_height_speakers() {
    let layout = SpeakerLayout::layout_7_1_4();
    let zenith = Vec3::new(0.0, 0.0, 1.0);
    let gains = render_impulse_gains(&layout, zenith, 0.0);

    // Height speakers in 7.1.4 are indices 8, 9, 10, 11 (Ltf, Rtf, Ltr, Rtr).
    for (idx, &g) in gains.iter().enumerate() {
        if (8..=11).contains(&idx) {
            assert_relative_eq!(g, 0.5, epsilon = 1e-4);
        } else {
            assert_relative_eq!(g, 0.0, epsilon = 1e-4);
        }
    }
}

#[test]
fn non_zero_width_spreads_energy_while_conserving_l2_power() {
    let layout = SpeakerLayout::layout_7_1_4();
    let front_left = layout.speakers[0].position; // L (+30 deg)
    let narrow = render_impulse_gains(&layout, front_left, 0.0);
    let wide = render_impulse_gains(&layout, front_left, 0.5);

    assert_relative_eq!(narrow[0], 1.0, epsilon = 1e-4);
    assert!(
        wide[0] < 0.99,
        "spread should distribute energy away from L"
    );
    assert!(
        wide[2] > 0.05,
        "spread should excite neighbor Center channel"
    );

    let power: f64 = wide.iter().map(|g| g * g).sum();
    assert_relative_eq!(power, 1.0, epsilon = 1e-6);
}

#[test]
fn custom_asymmetric_5_speaker_toml_renders_cleanly() {
    let toml = r#"
        name = "asymmetric-dome-5"
        [[speakers]]
        label = "S1"
        azimuth_deg = 22.0
        elevation_deg = 0.0

        [[speakers]]
        label = "S2"
        azimuth_deg = -40.0
        elevation_deg = 5.0

        [[speakers]]
        label = "S3"
        azimuth_deg = 125.0
        elevation_deg = 10.0

        [[speakers]]
        label = "S4"
        azimuth_deg = -140.0
        elevation_deg = 0.0

        [[speakers]]
        label = "TOP"
        azimuth_deg = 10.0
        elevation_deg = 70.0
    "#;
    let layout = SpeakerLayout::from_toml_str(toml).expect("valid custom dome");
    let pos = Vec3::from_spherical(SphericalCoord::new(-15.0, 30.0, 1.0));
    let gains = render_impulse_gains(&layout, pos, 0.0);
    let power: f64 = gains.iter().map(|g| g * g).sum();
    assert_relative_eq!(power, 1.0, epsilon = 1e-6);
}

#[test]
fn damf_importer_maps_front_center_cube_to_center_speaker() {
    let importer = DamfStaticImporter::new(DamfStaticDescriptor {
        sample_rate: 48_000,
        total_frames: 256,
        object_id: 10,
        label: "dialog_center".to_string(),
        role: ObjectRole::Dynamic,
        pos_x: 0.5, // Center L-R
        pos_y: 0.0, // Front wall
        pos_z: 0.0, // Ear level
        gain: 1.0,
        width: 0.0,
        divergence: 0.0,
        signal: DamfSignalSpec::Impulse {
            impulse_frame: 12,
            amplitude: 1.0,
        },
    });

    let mut scene = importer.import().expect("DAMF import");
    let layout = SpeakerLayout::surround_5_1();
    let vbap = VbapRenderer::build(&layout).unwrap();
    let report = render_scene(
        &mut scene,
        &layout,
        &vbap,
        RenderConfig::default(),
        Box::new(MemorySink::new(layout.speakers.len())),
    )
    .unwrap();

    // Channel 2 in 5.1 is C.
    assert_relative_eq!(report.channels[2].peak_linear, 1.0, epsilon = 1e-5);
    assert_relative_eq!(report.channels[0].peak_linear, 0.0, epsilon = 1e-5);
    assert_relative_eq!(report.channels[1].peak_linear, 0.0, epsilon = 1e-5);
}

#[test]
fn lfe_object_routes_exclusively_to_lfe_and_respects_missing_policy() {
    let layout_5_1 = SpeakerLayout::surround_5_1();
    let vbap_5_1 = VbapRenderer::build(&layout_5_1).unwrap();
    let mut scene = SceneBuilder::new(48_000, 64)
        .add_object(AudioObject::static_mono(
            ObjectId(1),
            "sub",
            ObjectRole::Lfe,
            Vec3::ZERO,
            1.0,
            Box::new(ImpulseSource::new(4, 1.0, 64)),
        ))
        .build()
        .unwrap();

    let rep = render_scene(
        &mut scene,
        &layout_5_1,
        &vbap_5_1,
        RenderConfig::default(),
        Box::new(MemorySink::new(6)),
    )
    .unwrap();
    // LFE is index 3 in 5.1.
    for (idx, ch) in rep.channels.iter().enumerate() {
        let expected = if idx == 3 { 1.0 } else { 0.0 };
        assert_relative_eq!(ch.peak_linear, expected, epsilon = 1e-6);
    }

    // Stereo lacks LFE: DropIfAbsent yields silence, FoldDownIfAbsent splits to L/R.
    let stereo = SpeakerLayout::stereo();
    let vbap_stereo = VbapRenderer::build(&stereo).unwrap();

    let rep_drop = render_scene(
        &mut scene,
        &stereo,
        &vbap_stereo,
        RenderConfig {
            lfe_policy: LfePolicy::DropIfAbsent,
            ..RenderConfig::default()
        },
        Box::new(MemorySink::new(2)),
    )
    .unwrap();
    assert_relative_eq!(rep_drop.channels[0].peak_linear, 0.0, epsilon = 1e-6);
    assert_relative_eq!(rep_drop.channels[1].peak_linear, 0.0, epsilon = 1e-6);

    let rep_fold = render_scene(
        &mut scene,
        &stereo,
        &vbap_stereo,
        RenderConfig {
            lfe_policy: LfePolicy::FoldDownIfAbsent { gain_db: 0.0 },
            ..RenderConfig::default()
        },
        Box::new(MemorySink::new(2)),
    )
    .unwrap();
    assert_relative_eq!(
        rep_fold.channels[0].peak_linear,
        std::f64::consts::FRAC_1_SQRT_2,
        epsilon = 1e-6
    );
    assert_relative_eq!(
        rep_fold.channels[1].peak_linear,
        std::f64::consts::FRAC_1_SQRT_2,
        epsilon = 1e-6
    );
}

#[test]
fn wav_output_validates_with_hound_on_7_1_4() {
    let layout = SpeakerLayout::layout_7_1_4();
    let vbap = VbapRenderer::build(&layout).unwrap();
    let left_pos = layout.speakers[0].position; // L (+30 deg)
    let right_pos = layout.speakers[1].position; // R (-30 deg)
    let mid_front = (left_pos + right_pos).normalized().unwrap();

    let total_frames = 512u64;
    let mut scene = SceneBuilder::new(48_000, total_frames)
        .add_object(AudioObject::static_mono(
            ObjectId(1),
            "sine_l",
            ObjectRole::Dynamic,
            left_pos,
            1.0,
            Box::new(SineSource::new(48_000, 1000.0, 0.5, total_frames)),
        ))
        .add_object(AudioObject::static_mono(
            ObjectId(2),
            "impulse_mid",
            ObjectRole::Dynamic,
            mid_front,
            0.25,
            Box::new(ImpulseSource::new(100, 1.0, total_frames)),
        ))
        .build()
        .unwrap();

    let wav_path = std::env::temp_dir().join(format!("objrender_test_{}.wav", std::process::id()));
    let wave_cfg = WaveConfig::new(
        scene.sample_rate,
        layout.speakers.len() as u16,
        layout.channel_mask(),
        SampleFormat::Float32,
    );
    let writer = WaveWriter::create(&wav_path, wave_cfg).expect("create temp wav");

    let report = render_scene(
        &mut scene,
        &layout,
        &vbap,
        RenderConfig {
            block_size: 64,
            clipping: ClippingPolicy::PassThrough,
            lfe_policy: LfePolicy::DropIfAbsent,
            ..RenderConfig::default()
        },
        Box::new(writer),
    )
    .expect("render wav");
    assert!(!report.rf64_promoted);

    let mut reader = hound::WavReader::open(&wav_path).expect("hound reads generated WAV");
    let spec = reader.spec();
    assert_eq!(spec.channels, 12);
    assert_eq!(spec.sample_rate, 48_000);
    assert_eq!(spec.bits_per_sample, 32);
    assert_eq!(spec.sample_format, hound::SampleFormat::Float);

    let decoded: Vec<f32> = reader
        .samples::<f32>()
        .map(|s| s.expect("valid sample"))
        .collect();
    assert_eq!(decoded.len(), (total_frames as usize) * 12);

    // Frame 0: sine(0) = 0, impulse = 0 -> all 12 channels are 0.0.
    for &sample in decoded.iter().take(12) {
        assert_relative_eq!(sample, 0.0_f32, epsilon = 1e-6);
    }

    // Frame 1: L channel (ch 0) carries sine at t = 1/48000; LFE (ch 3) is silent.
    let expected_l = (0.5 * (2.0 * std::f64::consts::PI * 1000.0 / 48_000.0).sin()) as f32;
    assert_relative_eq!(decoded[12], expected_l, epsilon = 1e-5);
    assert_relative_eq!(decoded[12 + 3], 0.0_f32, epsilon = 1e-6);

    // Frame 100: Center (ch 2) gets 0.25 from mid_front impulse.
    assert_relative_eq!(decoded[100 * 12 + 2], 0.25_f32, epsilon = 1e-5);

    let _ = std::fs::remove_file(&wav_path);
}

#[test]
fn rf64_promotion_writes_valid_ds64_header_for_f32_and_f64() {
    for format in [SampleFormat::Float32, SampleFormat::Float64] {
        let mut cursor = Cursor::new(Vec::<u8>::new());
        let mut cfg = WaveConfig::new(48_000, 12, 0x2_D63F, format);
        cfg.rf64_threshold_bytes = 512;

        let frames = 64u64;
        let mut bus = objrender::io::PcmBus::new(12, frames as usize);
        bus.channel_mut(0)[0] = 0.75;

        {
            let mut writer: Box<dyn SampleWriter> =
                Box::new(WaveWriter::from_writer(&mut cursor, cfg).unwrap());
            writer.write_block(&bus, frames as usize).unwrap();
            let meta = writer.finalize().unwrap();
            assert!(meta.rf64_promoted);
            assert_eq!(meta.frames_written, frames);
        }

        let bytes = cursor.into_inner();
        let data_bytes = frames * 12 * (format.bytes_per_sample() as u64);
        let riff_size = (104 + data_bytes) - 8;

        assert_eq!(&bytes[0..4], b"RF64");
        assert_eq!(
            u32::from_le_bytes(bytes[4..8].try_into().unwrap()),
            u32::MAX
        );
        assert_eq!(&bytes[8..12], b"WAVE");
        assert_eq!(&bytes[12..16], b"ds64");
        assert_eq!(u32::from_le_bytes(bytes[16..20].try_into().unwrap()), 28);
        assert_eq!(
            u64::from_le_bytes(bytes[20..28].try_into().unwrap()),
            riff_size
        );
        assert_eq!(
            u64::from_le_bytes(bytes[28..36].try_into().unwrap()),
            data_bytes
        );
        assert_eq!(
            u64::from_le_bytes(bytes[36..44].try_into().unwrap()),
            frames
        );
        assert_eq!(u32::from_le_bytes(bytes[44..48].try_into().unwrap()), 0);
        assert_eq!(&bytes[96..100], b"data");
        assert_eq!(
            u32::from_le_bytes(bytes[100..104].try_into().unwrap()),
            u32::MAX
        );
    }
}

proptest! {
    #[test]
    fn vbap_conserves_l2_power_and_excludes_lfe_across_arbitrary_layouts(
        az in -180.0_f64..=180.0,
        el in -90.0_f64..=90.0,
        width in prop::sample::select(vec![0.0_f64, 0.25, 0.8]),
    ) {
        let custom_open_front = SpeakerLayout::from_toml_str(r#"
            name = "open-front-3d"
            [[speakers]]
            label = "FL"
            azimuth_deg = 35.0
            elevation_deg = 0.0
            [[speakers]]
            label = "FR"
            azimuth_deg = -35.0
            elevation_deg = 0.0
            [[speakers]]
            label = "FC"
            azimuth_deg = 0.0
            elevation_deg = 0.0
            [[speakers]]
            label = "TFL"
            azimuth_deg = 40.0
            elevation_deg = 40.0
            [[speakers]]
            label = "TFR"
            azimuth_deg = -40.0
            elevation_deg = 40.0
        "#).unwrap();

        let layouts = [
            SpeakerLayout::mono(),
            SpeakerLayout::stereo(),
            SpeakerLayout::surround_5_1(),
            SpeakerLayout::surround_7_1(),
            SpeakerLayout::layout_5_1_4(),
            SpeakerLayout::layout_7_1_4(),
            SpeakerLayout::layout_7_1_6(),
            SpeakerLayout::layout_9_1_6(),
            custom_open_front,
        ];

        let pos = Vec3::from_spherical(SphericalCoord::new(az, el, 1.0));
        let params = SpatialParams { width, divergence: 0.0 };

        for layout in &layouts {
            let vbap = VbapRenderer::build(layout).unwrap();
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
            prop_assert!(
                (sum_sq - 1.0).abs() < 1e-6,
                "energy not conserved on {}: sum_sq={sum_sq} at az={az}, el={el}, width={width}",
                layout.name
            );
        }
    }
}
