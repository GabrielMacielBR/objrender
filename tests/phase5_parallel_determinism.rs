use std::fs;
use std::sync::{Arc, Mutex};

use approx::assert_relative_eq;
use objrender::importers::SceneImporter;
use objrender::importers::damf::DamfImporter;
use objrender::io::{
    IoError, MemorySink, OutputMetadata, PcmBus, SampleFormat, SampleWriter, WaveConfig, WaveWriter,
};
use objrender::layout::SpeakerLayout;
use objrender::math::{SphericalCoord, Vec3};
use objrender::mixer::{ClippingPolicy, RenderConfig, RenderReport, render_scene};
use objrender::renderer::VbapRenderer;
use objrender::scene::{
    AudioObject, InMemorySource, InterpMode, Keyframe, ObjectId, ObjectRole, Scene, SceneBuilder,
    SineSource, SpatialParams,
};

struct CaptureWriter {
    sink: MemorySink,
    out: Arc<Mutex<Option<MemorySink>>>,
}

impl CaptureWriter {
    fn pair(num_channels: usize) -> (Box<dyn SampleWriter>, Arc<Mutex<Option<MemorySink>>>) {
        let out = Arc::new(Mutex::new(None));
        let writer = Self {
            sink: MemorySink::new(num_channels),
            out: Arc::clone(&out),
        };
        (Box::new(writer), out)
    }
}

impl SampleWriter for CaptureWriter {
    fn write_block(&mut self, bus: &PcmBus, frames: usize) -> Result<(), IoError> {
        self.sink.write_block(bus, frames)
    }

    fn finalize(mut self: Box<Self>) -> Result<OutputMetadata, IoError> {
        let frames_written = self.sink.frames_written;
        *self.out.lock().unwrap() = Some(std::mem::take(&mut self.sink));
        Ok(OutputMetadata {
            rf64_promoted: false,
            frames_written,
            total_bytes: 0,
        })
    }
}

fn render_captured(
    scene: &mut Scene,
    layout: &SpeakerLayout,
    config: RenderConfig,
) -> (RenderReport, Vec<Vec<f64>>) {
    let vbap = VbapRenderer::build(layout).unwrap();
    let (writer, captured) = CaptureWriter::pair(layout.speakers.len());
    let report = render_scene(scene, layout, &vbap, config, writer).unwrap();
    let sink = captured.lock().unwrap().take().unwrap();
    (report, sink.channels)
}

fn assert_renders_bit_exact(
    label: &str,
    rep_a: &RenderReport,
    pcm_a: &[Vec<f64>],
    rep_b: &RenderReport,
    pcm_b: &[Vec<f64>],
) {
    assert_eq!(
        rep_a.frames_rendered, rep_b.frames_rendered,
        "{label}: frames"
    );
    assert_eq!(
        rep_a.applied_gain_linear.to_bits(),
        rep_b.applied_gain_linear.to_bits(),
        "{label}: applied_gain_linear"
    );
    assert_eq!(pcm_a.len(), pcm_b.len(), "{label}: channel count");

    for (ch, (ca, cb)) in pcm_a.iter().zip(pcm_b.iter()).enumerate() {
        assert_eq!(ca.len(), cb.len(), "{label}: ch {ch} len");
        for (f, (&sa, &sb)) in ca.iter().zip(cb.iter()).enumerate() {
            assert_eq!(
                sa.to_bits(),
                sb.to_bits(),
                "{label}: sample mismatch at ch {ch} frame {f}: {sa} vs {sb}"
            );
        }

        let sa = &rep_a.channels[ch];
        let sb = &rep_b.channels[ch];
        assert_eq!(
            sa.peak_linear.to_bits(),
            sb.peak_linear.to_bits(),
            "{label}: ch {ch} peak_linear"
        );
        assert_eq!(
            sa.sum_squares.to_bits(),
            sb.sum_squares.to_bits(),
            "{label}: ch {ch} sum_squares"
        );
        assert_eq!(
            sa.clipped_samples, sb.clipped_samples,
            "{label}: ch {ch} clipped_samples"
        );
    }
}

fn build_moving_multi_object_scene() -> Scene {
    let sr = 48_000u32;
    let total_frames = 48_000u64; // 12 chunks of 4096 (11 full + partial tail)
    let mut builder = SceneBuilder::new(sr, total_frames);

    // Ch 0: LFE object
    builder = builder.add_object(AudioObject::static_mono(
        ObjectId(1),
        "lfe",
        ObjectRole::Lfe,
        Vec3::ZERO,
        0.8,
        Box::new(SineSource::new(sr, 60.0, 0.4, total_frames)),
    ));

    // Ch 1..4: Static beds
    for (i, az) in [30.0_f64, -30.0, 0.0].into_iter().enumerate() {
        builder = builder.add_object(AudioObject::static_mono(
            ObjectId(2 + i as u32),
            format!("bed_{i}"),
            ObjectRole::Bed,
            Vec3::from_spherical(SphericalCoord::new(az, 0.0, 1.0)),
            0.35,
            Box::new(SineSource::new(
                sr,
                220.0 + (i as f64) * 70.0,
                0.25,
                total_frames,
            )),
        ));
    }

    // Dynamic objects with SLERP/Linear motion, width, divergence, and one delayed start_frame.
    for idx in 0..12u32 {
        let az0 = ((idx as f64) * 30.0) - 180.0;
        let az1 = az0 + 135.0;
        let el0 = if idx % 2 == 0 { 0.0 } else { 40.0 };
        let el1 = if idx % 3 == 0 { 45.0 } else { -10.0 };
        let pos0 = Vec3::from_spherical(SphericalCoord::new(az0, el0, 1.0));
        let pos1 = Vec3::from_spherical(SphericalCoord::new(az1, el1, 1.0));

        let start_frame = if idx == 11 { 5000 } else { 0 };
        let obj_frames = total_frames - start_frame;

        let mut obj = AudioObject::static_mono(
            ObjectId(10 + idx),
            format!("dyn_{idx}"),
            ObjectRole::Dynamic,
            pos0,
            0.4,
            Box::new(SineSource::new(
                sr,
                330.0 + (idx as f64) * 65.0,
                0.2,
                obj_frames,
            )),
        );
        obj.start_frame = start_frame;
        obj.initial_spatial = SpatialParams {
            width: if idx % 4 == 0 { 0.25 } else { 0.0 },
            divergence: if idx % 5 == 0 { 0.15 } else { 0.0 },
        };
        obj.keyframes = vec![
            Keyframe {
                frame: start_frame,
                position: pos0,
                gain: 0.4,
                spatial: obj.initial_spatial,
                interp: if idx % 2 == 0 {
                    InterpMode::Spherical
                } else {
                    InterpMode::Linear
                },
                discontinuous: false,
            },
            Keyframe {
                frame: 20_480,
                position: pos1,
                gain: 0.25,
                spatial: SpatialParams {
                    width: 0.1,
                    divergence: 0.05,
                },
                interp: InterpMode::Linear,
                discontinuous: idx == 3,
            },
            Keyframe {
                frame: total_frames - 1,
                position: pos0,
                gain: 0.45,
                spatial: obj.initial_spatial,
                interp: InterpMode::Step,
                discontinuous: false,
            },
        ];
        builder = builder.add_object(obj);
    }

    builder.build().unwrap()
}

#[test]
fn parallel_render_is_bit_exact_with_single_thread_on_moving_multi_object_scene() {
    let layout = SpeakerLayout::layout_7_1_4();
    let mut scene = build_moving_multi_object_scene();

    let (rep1, pcm1) = render_captured(
        &mut scene,
        &layout,
        RenderConfig {
            threads: 1,
            ..RenderConfig::default()
        },
    );

    for threads in [2usize, 4, 0] {
        let (rep_n, pcm_n) = render_captured(
            &mut scene,
            &layout,
            RenderConfig {
                threads,
                ..RenderConfig::default()
            },
        );
        assert_renders_bit_exact(&format!("threads={threads}"), &rep1, &pcm1, &rep_n, &pcm_n);
    }
}

#[test]
fn crossfade_spanning_chunk_boundary_is_bit_exact_and_click_free() {
    let layout = SpeakerLayout::stereo();
    let total_frames = 12_288u64; // 3 full 4096-frame chunks
    let left = Vec3::from_spherical(SphericalCoord::new(30.0, 0.0, 1.0));
    let right = Vec3::from_spherical(SphericalCoord::new(-30.0, 0.0, 1.0));

    let mut obj = AudioObject::static_mono(
        ObjectId(1),
        "dc_teleport",
        ObjectRole::Dynamic,
        left,
        1.0,
        Box::new(InMemorySource::new(vec![1.0; total_frames as usize])),
    );
    obj.keyframes = vec![
        Keyframe {
            frame: 0,
            position: left,
            gain: 1.0,
            spatial: SpatialParams::default(),
            interp: InterpMode::Step,
            discontinuous: false,
        },
        // Teleport right before chunk boundary 4096 -> 128-frame Hann crossfade spans 4050..4178.
        Keyframe {
            frame: 4050,
            position: right,
            gain: 1.0,
            spatial: SpatialParams::default(),
            interp: InterpMode::Step,
            discontinuous: true,
        },
        // Return to left cleanly before chunk boundary 8192, then teleport right at 8150 -> spans 8150..8278.
        Keyframe {
            frame: 6000,
            position: left,
            gain: 1.0,
            spatial: SpatialParams::default(),
            interp: InterpMode::Step,
            discontinuous: true,
        },
        Keyframe {
            frame: 8150,
            position: right,
            gain: 1.0,
            spatial: SpatialParams::default(),
            interp: InterpMode::Step,
            discontinuous: true,
        },
    ];

    let mut scene = SceneBuilder::new(48_000, total_frames)
        .add_object(obj)
        .build()
        .unwrap();

    let (rep1, pcm1) = render_captured(
        &mut scene,
        &layout,
        RenderConfig {
            block_size: 64,
            crossfade_frames: 128,
            threads: 1,
            ..RenderConfig::default()
        },
    );
    let (rep4, pcm4) = render_captured(
        &mut scene,
        &layout,
        RenderConfig {
            block_size: 64,
            crossfade_frames: 128,
            threads: 4,
            ..RenderConfig::default()
        },
    );

    assert_renders_bit_exact("crossfade_chunk_boundary", &rep1, &pcm1, &rep4, &pcm4);

    // Ensure no hard step across frame 4096 or 8192 and smooth overall transition.
    for (ch, ch_buf) in pcm4.iter().enumerate().take(2) {
        for f in 1..(total_frames as usize) {
            let step = (ch_buf[f] - ch_buf[f - 1]).abs();
            assert!(
                step < 0.03,
                "click detected at ch {ch} frame {f}: step={step}"
            );
        }
    }
    assert_relative_eq!(pcm4[0][4200], 0.0, epsilon = 1e-6);
    assert_relative_eq!(pcm4[1][4200], 1.0, epsilon = 1e-6);
}

fn build_caf_24le(sample_rate: u32, channels: &[Vec<f64>]) -> Vec<u8> {
    let n_ch = channels.len() as u32;
    let total_frames = channels[0].len();
    let mut pcm = Vec::with_capacity(total_frames * (n_ch as usize) * 3);
    for f in 0..total_frames {
        for ch_buf in channels {
            let s = (ch_buf[f].clamp(-1.0, 0.999_999_8) * 8_388_608.0).round() as i32;
            pcm.extend_from_slice(&s.to_le_bytes()[0..3]);
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
    out.extend_from_slice(&(1u32 << 1).to_be_bytes()); // little-endian 24-bit int
    out.extend_from_slice(&(n_ch * 3).to_be_bytes());
    out.extend_from_slice(&1u32.to_be_bytes());
    out.extend_from_slice(&n_ch.to_be_bytes());
    out.extend_from_slice(&24u32.to_be_bytes());

    out.extend_from_slice(b"data");
    out.extend_from_slice(&((4 + pcm.len()) as i64).to_be_bytes());
    out.extend_from_slice(&0u32.to_be_bytes());
    out.extend_from_slice(&pcm);
    out
}

#[test]
fn parallel_two_pass_peak_normalize_on_caf_damf_matches_single_thread_bit_exact() {
    let tmp_dir = std::env::temp_dir().join(format!("objrender_p5_joc_{}", std::process::id()));
    let _ = fs::remove_dir_all(&tmp_dir);
    fs::create_dir_all(&tmp_dir).unwrap();

    let total_frames = 32_768usize; // 8 chunks of 4096 frames (8 CAF blocks)
    let sr = 48_000u32;

    // 8-channel EAC3-JOC CAF: ch 0..4 core downmix (decoys, must be skipped),
    // ch 5 LFE (ID 3), ch 6..7 moving objects (ID 11, ID 12).
    let mut channels = vec![vec![0.0_f64; total_frames]; 8];
    for ch_decoy in &mut channels[0..5] {
        ch_decoy.fill(0.95);
    }
    for (f, ch_lfe) in channels[5].iter_mut().enumerate() {
        *ch_lfe = 0.4 * ((f as f64) * 0.01).sin();
    }
    for (f, ch_o1) in channels[6].iter_mut().enumerate() {
        *ch_o1 = 0.85 * ((f as f64) * 0.03).sin();
    }
    for (f, ch_o2) in channels[7].iter_mut().enumerate() {
        *ch_o2 = 0.85 * ((f as f64) * 0.045).cos();
    }
    let caf_bytes = build_caf_24le(sr, &channels);

    let manifest_yaml = r#"version: 0.5.1
presentations:
  - type: home
    simplified: false
    metadata: joc.atmos.metadata
    audio: joc.atmos.audio
    offset: 0.0
    fps: 24
    sourceCodec: EAC3-JOC
    bedInstances:
      - channels:
          - channel: LFE
            ID: 3
    objects:
      - description: Moving Object 1
        ID: 11
      - description: Moving Object 2
        ID: 12
"#;

    let metadata_yaml = r#"sampleRate: 48000
events:
  - ID: 3
    samplePos: 0
    active: true
    gain: 0
    rampLength: 0
  - ID: 11
    samplePos: 0
    active: true
    pos: [-1.0, 1.0, 0.0]
    size: 0.0
    gain: 0
    rampLength: 0
  - ID: 11
    samplePos: 8192
    pos: [1.0, 1.0, 1.0]
    rampLength: 16384
  - ID: 12
    samplePos: 0
    active: true
    pos: [1.0, -1.0, 0.0]
    size: 0.1
    gain: 0
    rampLength: 0
  - ID: 12
    samplePos: 16384
    pos: [-1.0, 0.0, 1.0]
    rampLength: 8192
"#;

    let manifest_path = tmp_dir.join("joc.atmos");
    fs::write(&manifest_path, manifest_yaml).unwrap();
    fs::write(tmp_dir.join("joc.atmos.metadata"), metadata_yaml).unwrap();
    fs::write(tmp_dir.join("joc.atmos.audio"), caf_bytes).unwrap();

    let mut scene = DamfImporter::new(&manifest_path).import().unwrap();
    let layout = SpeakerLayout::layout_7_1_4();
    let vbap = VbapRenderer::build(&layout).unwrap();

    let wav_t1 = tmp_dir.join("out_t1.wav");
    let wav_t4 = tmp_dir.join("out_t4.wav");

    let rep1 = render_scene(
        &mut scene,
        &layout,
        &vbap,
        RenderConfig {
            clipping: ClippingPolicy::TwoPassPeakNormalize { target_dbfs: -1.0 },
            threads: 1,
            ..RenderConfig::default()
        },
        Box::new(
            WaveWriter::create(
                &wav_t1,
                WaveConfig::new(sr, 12, layout.channel_mask(), SampleFormat::Float32),
            )
            .unwrap(),
        ),
    )
    .unwrap();

    let rep4 = render_scene(
        &mut scene,
        &layout,
        &vbap,
        RenderConfig {
            clipping: ClippingPolicy::TwoPassPeakNormalize { target_dbfs: -1.0 },
            threads: 4,
            ..RenderConfig::default()
        },
        Box::new(
            WaveWriter::create(
                &wav_t4,
                WaveConfig::new(sr, 12, layout.channel_mask(), SampleFormat::Float32),
            )
            .unwrap(),
        ),
    )
    .unwrap();

    let bytes1 = fs::read(&wav_t1).unwrap();
    let bytes4 = fs::read(&wav_t4).unwrap();
    let _ = fs::remove_dir_all(&tmp_dir);

    assert_eq!(bytes1, bytes4, "WAV output must be byte-for-byte identical");
    assert_eq!(
        rep1.applied_gain_linear.to_bits(),
        rep4.applied_gain_linear.to_bits()
    );
    let max_peak = rep4
        .channels
        .iter()
        .map(|c| c.peak_dbfs())
        .fold(f64::NEG_INFINITY, f64::max);
    assert_relative_eq!(max_peak, -1.0, epsilon = 0.02);

    for (ch, (s1, s4)) in rep1.channels.iter().zip(rep4.channels.iter()).enumerate() {
        assert_eq!(
            s1.peak_linear.to_bits(),
            s4.peak_linear.to_bits(),
            "ch {ch} peak_linear"
        );
        assert_eq!(
            s1.sum_squares.to_bits(),
            s4.sum_squares.to_bits(),
            "ch {ch} sum_squares"
        );
        assert_eq!(s1.clipped_samples, s4.clipped_samples);
    }
}

#[test]
fn parallel_non_aligned_block_size_and_partial_tail_chunk_bit_exact() {
    let layout = SpeakerLayout::preset("5.1.4").unwrap();
    let total_frames = 10_003u64;
    let sr = 48_000u32;

    let p0 = Vec3::from_spherical(SphericalCoord::new(90.0, 15.0, 1.0));
    let p1 = Vec3::from_spherical(SphericalCoord::new(-120.0, 40.0, 1.0));
    let mut obj = AudioObject::static_mono(
        ObjectId(7),
        "unaligned_sweep",
        ObjectRole::Dynamic,
        p0,
        0.75,
        Box::new(SineSource::new(sr, 513.0, 0.5, total_frames)),
    );
    obj.keyframes = vec![
        Keyframe {
            frame: 0,
            position: p0,
            gain: 0.75,
            spatial: SpatialParams {
                width: 0.2,
                divergence: 0.1,
            },
            interp: InterpMode::Spherical,
            discontinuous: false,
        },
        Keyframe {
            frame: total_frames - 1,
            position: p1,
            gain: 0.35,
            spatial: SpatialParams::default(),
            interp: InterpMode::Step,
            discontinuous: false,
        },
    ];

    let mut scene = SceneBuilder::new(sr, total_frames)
        .add_object(obj)
        .build()
        .unwrap();

    let (rep1, pcm1) = render_captured(
        &mut scene,
        &layout,
        RenderConfig {
            block_size: 100,
            threads: 1,
            ..RenderConfig::default()
        },
    );
    let (rep4, pcm4) = render_captured(
        &mut scene,
        &layout,
        RenderConfig {
            block_size: 100,
            threads: 4,
            ..RenderConfig::default()
        },
    );

    assert_renders_bit_exact("unaligned_block_100", &rep1, &pcm1, &rep4, &pcm4);
}
