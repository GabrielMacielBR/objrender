use std::fs;
use std::io::{Cursor, Seek, SeekFrom, Write};
use std::sync::{Arc, Mutex};

use approx::assert_relative_eq;
use objrender::io::{
    DitherMode, PcmBus, RawPcmWriter, SampleFormat, StreamingWaveWriter, WaveConfig, WaveWriter,
};
use objrender::layout::{ChannelOrder, SpeakerLayout, SpeakerRole};
use objrender::math::{SphericalCoord, Vec3};
use objrender::mixer::{ClippingPolicy, LoudnessMeter, RenderConfig, render_scene};
use objrender::renderer::{Spatializer, VbapRenderer};
use objrender::scene::{
    AudioObject, ImpulseSource, ObjectId, ObjectRole, SceneBuilder, SineSource, SpatialParams,
};

#[test]
fn all_18_presets_build_vbap_and_route_key_directions() {
    let all_presets = [
        ("mono", 1),
        ("stereo", 2),
        ("2.1", 3),
        ("3.1.2", 6),
        ("4.0", 4),
        ("4.1", 5),
        ("5.0", 5),
        ("5.1", 6),
        ("5.1.2", 8),
        ("5.1.4", 10),
        ("7.0", 7),
        ("7.1", 8),
        ("7.1.2", 10),
        ("7.1.4", 12),
        ("7.1.6", 14),
        ("9.1.4", 14),
        ("9.1.6", 16),
        ("auro-10.1", 11),
    ];

    for (name, expected_ch) in all_presets {
        let layout = SpeakerLayout::preset(name).unwrap();
        assert_eq!(layout.speakers.len(), expected_ch, "preset {name}");
        let vbap = VbapRenderer::build(&layout).expect("VBAP must build on preset");
        let mut gains = vec![0.0_f64; expected_ch];

        for &(az, el) in &[(0.0, 0.0), (90.0, 0.0), (-90.0, 30.0), (180.0, 45.0)] {
            let dir = Vec3::from_spherical(SphericalCoord::new(az, el, 1.0));
            vbap.compute_gains(dir, SpatialParams::default(), &mut gains);
            let energy: f64 = gains.iter().map(|g| g * g).sum();
            assert_relative_eq!(energy, 1.0, epsilon = 1e-4);
            for (idx, spk) in layout.speakers.iter().enumerate() {
                if spk.role == SpeakerRole::Lfe {
                    assert_eq!(gains[idx], 0.0);
                }
            }
        }
    }

    // Auro 10.1 zenith Voice of God (+90 deg elevation) routes cleanly to VoG (ch 10).
    let auro = SpeakerLayout::preset("auro-10.1").unwrap();
    let auro_vbap = VbapRenderer::build(&auro).unwrap();
    let mut auro_gains = vec![0.0_f64; 11];
    auro_vbap.compute_gains(
        Vec3::from_spherical(SphericalCoord::new(0.0, 90.0, 1.0)),
        SpatialParams::default(),
        &mut auro_gains,
    );
    assert_relative_eq!(auro_gains[10], 1.0, epsilon = 1e-5);

    // 5.1.2 and 7.1.2 route overhead (+-90 deg az, +45 deg el) to Ltm / Rtm.
    let l512 = SpeakerLayout::preset("5.1.2").unwrap();
    let v512 = VbapRenderer::build(&l512).unwrap();
    let mut g512 = vec![0.0_f64; 8];
    v512.compute_gains(
        Vec3::from_spherical(SphericalCoord::new(90.0, 45.0, 1.0)),
        SpatialParams::default(),
        &mut g512,
    );
    assert_relative_eq!(g512[6], 1.0, epsilon = 1e-5);

    let l712 = SpeakerLayout::preset_with_order("7.1.2", ChannelOrder::Smpte).unwrap();
    assert_eq!(l712.speakers[4].label, "Lss");
    assert_eq!(l712.speakers[6].label, "Lrs");
    let v712 = VbapRenderer::build(&l712).unwrap();
    let mut g712 = vec![0.0_f64; 10];
    v712.compute_gains(
        Vec3::from_spherical(SphericalCoord::new(-90.0, 45.0, 1.0)),
        SpatialParams::default(),
        &mut g712,
    );
    assert_relative_eq!(g712[9], 1.0, epsilon = 1e-5);

    // 3.1.2 routes front height (+45, +45) to Ltf (ch 4).
    let l312 = SpeakerLayout::preset("3.1.2").unwrap();
    let v312 = VbapRenderer::build(&l312).unwrap();
    let mut g312 = vec![0.0_f64; 6];
    v312.compute_gains(
        Vec3::from_spherical(SphericalCoord::new(45.0, 45.0, 1.0)),
        SpatialParams::default(),
        &mut g312,
    );
    assert_relative_eq!(g312[4], 1.0, epsilon = 1e-5);

    // 4.0 routes rear left (+135, 0) to Ls (ch 2).
    let l40 = SpeakerLayout::preset("quad").unwrap();
    let v40 = VbapRenderer::build(&l40).unwrap();
    let mut g40 = vec![0.0_f64; 4];
    v40.compute_gains(
        Vec3::from_spherical(SphericalCoord::new(135.0, 0.0, 1.0)),
        SpatialParams::default(),
        &mut g40,
    );
    assert_relative_eq!(g40[2], 1.0, epsilon = 1e-5);
}

#[test]
fn integer_pcm_i16_and_i24_wav_decode_with_hound_and_tpdf_is_deterministic() {
    let tmp_dir = std::env::temp_dir().join(format!("objrender_p6_pcm_{}", std::process::id()));
    let _ = fs::remove_dir_all(&tmp_dir);
    fs::create_dir_all(&tmp_dir).unwrap();

    let layout = SpeakerLayout::preset("5.1.2").unwrap();
    let vbap = VbapRenderer::build(&layout).unwrap();
    let sr = 48_000u32;
    let total_frames = 12_288u64;

    let mut scene = SceneBuilder::new(sr, total_frames)
        .add_object(AudioObject::static_mono(
            ObjectId(1),
            "front_left",
            ObjectRole::Dynamic,
            Vec3::from_spherical(SphericalCoord::new(30.0, 0.0, 1.0)),
            0.5,
            Box::new(SineSource::new(sr, 440.0, 0.5, total_frames)),
        ))
        .build()
        .unwrap();

    let wav_i16_t1 = tmp_dir.join("out_i16_t1.wav");
    let wav_i16_t4 = tmp_dir.join("out_i16_t4.wav");
    let wav_i24 = tmp_dir.join("out_i24.wav");

    for (path, threads) in [(&wav_i16_t1, 1usize), (&wav_i16_t4, 4usize)] {
        let mut cfg = WaveConfig::new(sr, 8, layout.channel_mask(), SampleFormat::Int16);
        cfg.dither = DitherMode::Auto; // TPDF active for Int16
        render_scene(
            &mut scene,
            &layout,
            &vbap,
            RenderConfig {
                threads,
                ..RenderConfig::default()
            },
            Box::new(WaveWriter::create(path, cfg).unwrap()),
        )
        .unwrap();
    }

    assert_eq!(
        fs::read(&wav_i16_t1).unwrap(),
        fs::read(&wav_i16_t4).unwrap(),
        "i16 TPDF dither output must be byte-for-byte deterministic across threads"
    );

    let mut reader16 = hound::WavReader::open(&wav_i16_t1).unwrap();
    assert_eq!(reader16.spec().sample_format, hound::SampleFormat::Int);
    assert_eq!(reader16.spec().bits_per_sample, 16);
    assert_eq!(reader16.spec().channels, 8);
    let samples16: Vec<i32> = reader16.samples::<i32>().map(|s| s.unwrap()).collect();
    assert_eq!(samples16.len(), (total_frames as usize) * 8);
    let max_l_16 = samples16.iter().step_by(8).map(|s| s.abs()).max().unwrap();
    // 0.25 amplitude * 32768 = 8192 (+-1 LSB TPDF)
    assert!((max_l_16 - 8192).abs() <= 4, "max_l_16={max_l_16}");

    let cfg24 = WaveConfig::new(sr, 8, layout.channel_mask(), SampleFormat::Int24);
    render_scene(
        &mut scene,
        &layout,
        &vbap,
        RenderConfig {
            threads: 4,
            ..RenderConfig::default()
        },
        Box::new(WaveWriter::create(&wav_i24, cfg24).unwrap()),
    )
    .unwrap();

    let mut reader24 = hound::WavReader::open(&wav_i24).unwrap();
    assert_eq!(reader24.spec().sample_format, hound::SampleFormat::Int);
    assert_eq!(reader24.spec().bits_per_sample, 24);
    let samples24: Vec<i32> = reader24.samples::<i32>().map(|s| s.unwrap()).collect();
    let max_l_24 = samples24.iter().step_by(8).map(|s| s.abs()).max().unwrap();
    // 0.25 * 8388608 = 2097152
    assert!((max_l_24 - 2_097_152).abs() <= 16, "max_l_24={max_l_24}");

    let _ = fs::remove_dir_all(&tmp_dir);
}

#[derive(Clone, Default)]
struct SharedCursor(Arc<Mutex<Cursor<Vec<u8>>>>);

impl Write for SharedCursor {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        self.0.lock().unwrap().write(buf)
    }

    fn flush(&mut self) -> std::io::Result<()> {
        self.0.lock().unwrap().flush()
    }
}

impl Seek for SharedCursor {
    fn seek(&mut self, pos: SeekFrom) -> std::io::Result<u64> {
        self.0.lock().unwrap().seek(pos)
    }
}

#[test]
fn streaming_wave_writer_matches_seekable_wave_writer_and_raw_pcm() {
    let layout = SpeakerLayout::preset("4.0").unwrap();
    let vbap = VbapRenderer::build(&layout).unwrap();
    let sr = 48_000u32;
    let total_frames = 5003u64;

    let mut scene = SceneBuilder::new(sr, total_frames)
        .add_object(AudioObject::static_mono(
            ObjectId(1),
            "quad_sine",
            ObjectRole::Dynamic,
            Vec3::from_spherical(SphericalCoord::new(45.0, 0.0, 1.0)),
            0.7,
            Box::new(SineSource::new(sr, 330.0, 0.6, total_frames)),
        ))
        .build()
        .unwrap();

    // Test both RIFF and RF64 promotion thresholds cleanly.
    for threshold in [u32::MAX as u64, 1024u64] {
        let mut cfg = WaveConfig::new(sr, 4, layout.channel_mask(), SampleFormat::Int24);
        cfg.rf64_threshold_bytes = threshold;

        let seek_buf = SharedCursor::default();
        let writer_seek = WaveWriter::from_writer(seek_buf.clone(), cfg).unwrap();
        render_scene(
            &mut scene,
            &layout,
            &vbap,
            RenderConfig::default(),
            Box::new(writer_seek),
        )
        .unwrap();

        let stream_buf = SharedCursor::default();
        let writer_stream =
            StreamingWaveWriter::new(stream_buf.clone(), cfg, Some(total_frames)).unwrap();
        render_scene(
            &mut scene,
            &layout,
            &vbap,
            RenderConfig::default(),
            Box::new(writer_stream),
        )
        .unwrap();

        let seek_bytes = seek_buf.0.lock().unwrap().get_ref().clone();
        let stream_bytes = stream_buf.0.lock().unwrap().get_ref().clone();
        assert_eq!(
            seek_bytes, stream_bytes,
            "StreamingWaveWriter must match WaveWriter byte-for-byte (threshold={threshold})"
        );

        let raw_buf = SharedCursor::default();
        let writer_raw =
            RawPcmWriter::new(raw_buf.clone(), 4, SampleFormat::Int24, DitherMode::Auto).unwrap();
        render_scene(
            &mut scene,
            &layout,
            &vbap,
            RenderConfig::default(),
            Box::new(writer_raw),
        )
        .unwrap();

        let raw_bytes = raw_buf.0.lock().unwrap().get_ref().clone();
        let expected_payload_len = (total_frames as usize) * 4 * 3;
        assert_eq!(raw_bytes.len(), expected_payload_len);
        assert_eq!(&seek_bytes[104..104 + expected_payload_len], &raw_bytes[..]);
    }
}

#[test]
fn two_pass_lufs_normalization_hits_target_respects_true_peak_ceiling_and_is_bit_exact() {
    let layout = SpeakerLayout::layout_7_1_4();
    let vbap = VbapRenderer::build(&layout).unwrap();
    let sr = 48_000u32;
    let total_frames = 48_000u64;

    // Steady multi-object 7.1.4 scene -> loudness target -18.0 LUFS with -1.0 dBTP ceiling.
    let mut steady_scene = SceneBuilder::new(sr, total_frames)
        .add_object(AudioObject::static_mono(
            ObjectId(1),
            "left_tone",
            ObjectRole::Dynamic,
            Vec3::from_spherical(SphericalCoord::new(30.0, 0.0, 1.0)),
            0.3,
            Box::new(SineSource::new(sr, 997.0, 0.4, total_frames)),
        ))
        .add_object(AudioObject::static_mono(
            ObjectId(2),
            "right_tone",
            ObjectRole::Dynamic,
            Vec3::from_spherical(SphericalCoord::new(-30.0, 0.0, 1.0)),
            0.3,
            Box::new(SineSource::new(sr, 997.0, 0.4, total_frames)),
        ))
        .build()
        .unwrap();

    let tmp_dir = std::env::temp_dir().join(format!("objrender_p6_lufs_{}", std::process::id()));
    let _ = fs::remove_dir_all(&tmp_dir);
    fs::create_dir_all(&tmp_dir).unwrap();
    let wav_t1 = tmp_dir.join("lufs_t1.wav");
    let wav_t4 = tmp_dir.join("lufs_t4.wav");

    let mut reports = Vec::new();
    for (path, threads) in [(&wav_t1, 1usize), (&wav_t4, 4usize)] {
        let rep = render_scene(
            &mut steady_scene,
            &layout,
            &vbap,
            RenderConfig {
                clipping: ClippingPolicy::TwoPassLoudnessNormalize {
                    target_lufs: -18.0,
                    true_peak_ceiling_dbtp: -1.0,
                },
                threads,
                ..RenderConfig::default()
            },
            Box::new(
                WaveWriter::create(
                    path,
                    WaveConfig::new(sr, 12, layout.channel_mask(), SampleFormat::Float32),
                )
                .unwrap(),
            ),
        )
        .unwrap();
        reports.push(rep);
    }

    assert_eq!(fs::read(&wav_t1).unwrap(), fs::read(&wav_t4).unwrap());
    assert_eq!(
        reports[0].applied_gain_linear.to_bits(),
        reports[1].applied_gain_linear.to_bits()
    );
    assert_relative_eq!(reports[1].integrated_lufs.unwrap(), -18.0, epsilon = 0.05);
    assert!(reports[1].true_peak_dbtp.unwrap() <= -1.0);

    // Re-measure rendered WAV independently with LoudnessMeter to confirm reported LUFS.
    let decoded: Vec<f32> = hound::WavReader::open(&wav_t4)
        .unwrap()
        .samples::<f32>()
        .map(|s| s.unwrap())
        .collect();
    let mut bus = PcmBus::new(12, total_frames as usize);
    for (frame_idx, frame) in decoded.as_chunks::<12>().0.iter().enumerate() {
        for (ch, &val) in frame.iter().enumerate() {
            bus.channel_mut(ch)[frame_idx] = val as f64;
        }
    }
    let mut verify_meter = LoudnessMeter::new(sr, &layout);
    verify_meter.process_block(&bus, total_frames as usize);
    let verified = verify_meter.finalize_measurement();
    assert_relative_eq!(verified.integrated_lufs, -18.0, epsilon = 0.08);
    assert!(verified.true_peak_dbtp() <= -1.0 + 0.01);

    // High-crest-factor transient scene: aggressive target -6.0 LUFS must be capped by -1.0 dBTP ceiling.
    let mut pulse_scene = SceneBuilder::new(sr, total_frames)
        .add_object(AudioObject::static_mono(
            ObjectId(10),
            "spike",
            ObjectRole::Dynamic,
            Vec3::from_spherical(SphericalCoord::new(0.0, 0.0, 1.0)),
            1.0,
            Box::new(ImpulseSource::new(1000, 0.8, total_frames)),
        ))
        .build()
        .unwrap();

    let wav_ceil = tmp_dir.join("lufs_ceil.wav");
    let ceil_report = render_scene(
        &mut pulse_scene,
        &layout,
        &vbap,
        RenderConfig {
            clipping: ClippingPolicy::TwoPassLoudnessNormalize {
                target_lufs: -6.0,
                true_peak_ceiling_dbtp: -1.0,
            },
            threads: 4,
            ..RenderConfig::default()
        },
        Box::new(
            WaveWriter::create(
                &wav_ceil,
                WaveConfig::new(sr, 12, layout.channel_mask(), SampleFormat::Float32),
            )
            .unwrap(),
        ),
    )
    .unwrap();

    assert_relative_eq!(ceil_report.true_peak_dbtp.unwrap(), -1.0, epsilon = 0.05);
    assert!(ceil_report.integrated_lufs.unwrap() < -6.0);

    let _ = fs::remove_dir_all(&tmp_dir);
}
