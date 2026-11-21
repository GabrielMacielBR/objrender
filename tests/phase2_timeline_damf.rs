use std::f64::consts::TAU;
use std::fs;
use std::io::Cursor;

use approx::assert_relative_eq;

use objrender::importers::SceneImporter;
use objrender::importers::caf::SharedCafStream;
use objrender::importers::damf::DamfImporter;
use objrender::io::{SampleFormat, WaveConfig, WaveWriter};
use objrender::layout::SpeakerLayout;
use objrender::math::{SphericalCoord, Vec3};
use objrender::mixer::{ClippingPolicy, LfePolicy, RenderConfig, render_scene};
use objrender::renderer::VbapRenderer;
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
    out.extend_from_slice(&2u32.to_be_bytes()); // little-endian signed int
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

#[test]
fn moving_360_pan_is_click_free_and_conserves_rms_energy() {
    let layout = SpeakerLayout::layout_7_1_4();
    let vbap = VbapRenderer::build(&layout).unwrap();
    let sr = 48_000u32;
    let total_frames = 24_000u64; // 0.5 s
    let freq_hz = 400.0;
    let amp = 0.5;

    let mut obj = AudioObject::static_mono(
        ObjectId(1),
        "orbit",
        ObjectRole::Dynamic,
        Vec3::from_spherical(SphericalCoord::new(0.0, 0.0, 1.0)),
        1.0,
        Box::new(SineSource::new(sr, freq_hz, amp, total_frames)),
    );

    // Full 360-deg horizontal + elevation sweep across spherical keyframes.
    let waypoints = [
        (0, 0.0, 0.0),
        (6_000, 90.0, 20.0),
        (12_000, 180.0, 45.0),
        (18_000, -90.0, 20.0),
        (23_999, 0.0, 0.0),
    ];
    obj.keyframes = waypoints
        .iter()
        .map(|&(frame, az, el)| Keyframe {
            frame,
            position: Vec3::from_spherical(SphericalCoord::new(az, el, 1.0)),
            gain: 1.0,
            spatial: SpatialParams::default(),
            interp: InterpMode::Spherical,
            discontinuous: false,
        })
        .collect();

    let mut scene = SceneBuilder::new(sr, total_frames)
        .add_object(obj)
        .build()
        .unwrap();

    let wav_path = std::env::temp_dir().join(format!("objrender_orbit_{}.wav", std::process::id()));
    let writer = WaveWriter::create(
        &wav_path,
        WaveConfig::new(sr, 12, layout.channel_mask(), SampleFormat::Float32),
    )
    .unwrap();
    render_scene(
        &mut scene,
        &layout,
        &vbap,
        RenderConfig::default(),
        Box::new(writer),
    )
    .unwrap();

    let mut reader = hound::WavReader::open(&wav_path).unwrap();
    let samples: Vec<f32> = reader.samples::<f32>().map(|s| s.unwrap()).collect();
    let _ = fs::remove_file(&wav_path);

    // 1. Check no inter-sample clicks on any channel: max slope <= sine slope + panning rate margin.
    let max_sine_step = (amp * TAU * freq_hz / (sr as f64)) as f32;
    let allowed_step = max_sine_step + 0.015;
    for ch in 0..12 {
        for f in 1..(total_frames as usize) {
            let prev = samples[(f - 1) * 12 + ch];
            let curr = samples[f * 12 + ch];
            let diff = (curr - prev).abs();
            assert!(
                diff <= allowed_step,
                "click/step detected on ch {ch} at frame {f}: diff={diff} > {allowed_step}"
            );
        }
    }

    // 2. Check multi-channel windowed RMS power stays constant across the entire 360-deg sweep.
    // Over 480 samples (4 full 400 Hz cycles), expected RMS is sqrt(0.5 * amp^2).
    let win = 480usize;
    let expected_rms = (0.5 * amp * amp).sqrt();
    for w_start in (0..(total_frames as usize)).step_by(win) {
        let mut energy = 0.0_f64;
        for f in w_start..w_start + win {
            for ch in 0..12 {
                let s = samples[f * 12 + ch] as f64;
                energy += s * s;
            }
        }
        let rms = (energy / win as f64).sqrt();
        let db_err = 20.0 * (rms / expected_rms).log10();
        assert!(
            db_err.abs() < 0.2,
            "RMS power drifted at frame {w_start}: {db_err:.3} dB"
        );
    }
}

#[test]
fn discontinuous_teleport_applies_hann_crossfade_anti_click() {
    let layout = SpeakerLayout::stereo();
    let vbap = VbapRenderer::build(&layout).unwrap();
    let l_pos = layout.speakers[0].position;
    let r_pos = layout.speakers[1].position;

    let build_teleport_scene = || {
        let mut obj = AudioObject::static_mono(
            ObjectId(1),
            "dc_teleport",
            ObjectRole::Dynamic,
            l_pos,
            1.0,
            Box::new(InMemorySource::new(vec![1.0; 512])),
        );
        obj.keyframes = vec![
            Keyframe {
                frame: 0,
                position: l_pos,
                gain: 1.0,
                spatial: SpatialParams::default(),
                interp: InterpMode::Step,
                discontinuous: false,
            },
            Keyframe {
                frame: 100,
                position: r_pos,
                gain: 1.0,
                spatial: SpatialParams::default(),
                interp: InterpMode::Step,
                discontinuous: true,
            },
        ];
        SceneBuilder::new(48_000, 512)
            .add_object(obj)
            .build()
            .unwrap()
    };

    let wav_xf = std::env::temp_dir().join(format!("objrender_xf_{}.wav", std::process::id()));
    let wav_no = std::env::temp_dir().join(format!("objrender_noxf_{}.wav", std::process::id()));

    render_scene(
        &mut build_teleport_scene(),
        &layout,
        &vbap,
        RenderConfig {
            block_size: 64,
            crossfade_frames: 128,
            clipping: ClippingPolicy::PassThrough,
            lfe_policy: LfePolicy::DropIfAbsent,
        },
        Box::new(
            WaveWriter::create(
                &wav_xf,
                WaveConfig::new(48_000, 2, layout.channel_mask(), SampleFormat::Float32),
            )
            .unwrap(),
        ),
    )
    .unwrap();

    render_scene(
        &mut build_teleport_scene(),
        &layout,
        &vbap,
        RenderConfig {
            block_size: 64,
            crossfade_frames: 0,
            clipping: ClippingPolicy::PassThrough,
            lfe_policy: LfePolicy::DropIfAbsent,
        },
        Box::new(
            WaveWriter::create(
                &wav_no,
                WaveConfig::new(48_000, 2, layout.channel_mask(), SampleFormat::Float32),
            )
            .unwrap(),
        ),
    )
    .unwrap();

    let s_xf: Vec<f32> = hound::WavReader::open(&wav_xf)
        .unwrap()
        .samples::<f32>()
        .map(|s| s.unwrap())
        .collect();
    let s_no: Vec<f32> = hound::WavReader::open(&wav_no)
        .unwrap()
        .samples::<f32>()
        .map(|s| s.unwrap())
        .collect();
    let _ = fs::remove_file(&wav_xf);
    let _ = fs::remove_file(&wav_no);

    // Without crossfade: before frame 100 L=1, R=0; at frame 100 L=0, R=1 (hard step = 1.0!).
    assert_relative_eq!(s_no[99 * 2], 1.0_f32, epsilon = 1e-5);
    assert_relative_eq!(s_no[100 * 2], 0.0_f32, epsilon = 1e-5);
    assert_relative_eq!(s_no[100 * 2 + 1], 1.0_f32, epsilon = 1e-5);

    // With 128-frame Hann crossfade: max inter-sample delta across entire file is small (< 0.02).
    let mut max_step_xf = 0.0_f32;
    for f in 1..512 {
        for ch in 0..2 {
            let d = (s_xf[f * 2 + ch] - s_xf[(f - 1) * 2 + ch]).abs();
            max_step_xf = max_step_xf.max(d);
        }
    }
    assert!(
        max_step_xf < 0.02,
        "crossfade max inter-sample jump too large: {max_step_xf}"
    );
    // By frame 230 (past 100 + 128), crossfade has settled onto R=1, L=0.
    assert_relative_eq!(s_xf[230 * 2], 0.0_f32, epsilon = 1e-5);
    assert_relative_eq!(s_xf[230 * 2 + 1], 1.0_f32, epsilon = 1e-5);
}

#[test]
fn end_to_end_truehdd_harletty_damf_bundle_renders_beds_lfe_and_moving_objects() {
    let tmp_dir = std::env::temp_dir().join(format!("objrender_damf_{}", std::process::id()));
    let _ = fs::remove_dir_all(&tmp_dir);
    fs::create_dir_all(&tmp_dir).unwrap();

    let total_frames = 1024usize;
    let sr = 48_000u32;

    // 5 CAF tracks matching manifest order:
    //   ch 0: bed L (ID 0) -> silent
    //   ch 1: bed R (ID 1) -> silent
    //   ch 2: bed C (ID 2) -> constant 0.25
    //   ch 3: bed LFE (ID 3) -> constant 0.5
    //   ch 4: dynamic object (ID 10) -> constant 0.5
    let caf_bytes = build_caf_24le(
        sr,
        &[
            vec![0.0; total_frames],
            vec![0.0; total_frames],
            vec![0.25; total_frames],
            vec![0.5; total_frames],
            vec![0.5; total_frames],
        ],
    );

    let manifest_yaml = r#"version: 0.5.1
presentations:
  - type: home
    simplified: false
    metadata: movie.atmos.metadata
    audio: movie.atmos.audio
    offset: 0.0
    fps: 24
    bedInstances:
      - channels:
          - channel: L
            ID: 0
          - channel: R
            ID: 1
          - channel: C
            ID: 2
          - channel: LFE
            ID: 3
    objects:
      - description: Panning Object
        ID: 10
"#;

    // Dynamic object ID 10:
    // - samplePos 0: active=true, pos=[-1.0, 1.0, 0.0] (Front Left), gain=0 dB (1.0), size=0.0
    // - samplePos 128, rampLength=256: ramps linearly to [1.0, 1.0, 0.0] (Front Right) at frame 384
    // - samplePos 600, rampLength=0: active=false, gain="-inf" (instant mute, crossfaded)
    let metadata_yaml = r#"sampleRate: 48000
events:
  - ID: 10
    samplePos: 0
    active: true
    pos: [-1.0, 1.0, 0.0]
    size: 0.0
    gain: 0
    rampLength: 0
  - ID: 10
    samplePos: 128
    pos: [1.0, 1.0, 0.0]
    rampLength: 256
  - ID: 10
    samplePos: 600
    active: false
    gain: "-inf"
    rampLength: 0
"#;

    let manifest_path = tmp_dir.join("movie.atmos");
    fs::write(&manifest_path, manifest_yaml).unwrap();
    fs::write(tmp_dir.join("movie.atmos.metadata"), metadata_yaml).unwrap();
    fs::write(tmp_dir.join("movie.atmos.audio"), caf_bytes).unwrap();

    let mut scene = DamfImporter::new(&manifest_path).import().unwrap();
    assert_eq!(scene.sample_rate, 48_000);
    assert_eq!(scene.total_frames, total_frames as u64);
    assert_eq!(scene.objects.len(), 5);

    let layout = SpeakerLayout::layout_7_1_4();
    let vbap = VbapRenderer::build(&layout).unwrap();
    let out_wav = tmp_dir.join("rendered_7_1_4.wav");

    render_scene(
        &mut scene,
        &layout,
        &vbap,
        RenderConfig {
            block_size: 64,
            crossfade_frames: 64,
            clipping: ClippingPolicy::PassThrough,
            lfe_policy: LfePolicy::DropIfAbsent,
        },
        Box::new(
            WaveWriter::create(
                &out_wav,
                WaveConfig::new(sr, 12, layout.channel_mask(), SampleFormat::Float32),
            )
            .unwrap(),
        ),
    )
    .unwrap();

    let decoded: Vec<f32> = hound::WavReader::open(&out_wav)
        .unwrap()
        .samples::<f32>()
        .map(|s| s.unwrap())
        .collect();
    let _ = fs::remove_dir_all(&tmp_dir);

    // 7.1.4 channel indices: 0=L, 1=R, 2=C, 3=LFE
    // Check LFE bed channel at frame 50: exact 0.5 on ch 3.
    assert_relative_eq!(decoded[50 * 12 + 3], 0.5_f32, epsilon = 1e-4);

    // At frame 50 (before ramp): object 10 is at [-1, 1, 0] (+45 deg Front-Left, between L +30 and Lss +90),
    // so L (ch 0) > 0.3 and R (ch 1) == 0.0, while Center (ch 2) carries only bed C = 0.25.
    assert!(decoded[50 * 12] > 0.3);
    assert_relative_eq!(decoded[50 * 12 + 1], 0.0_f32, epsilon = 1e-4);
    assert_relative_eq!(decoded[50 * 12 + 2], 0.25_f32, epsilon = 1e-4);

    // At frame 256 (exact midpoint of [128, 384]): pos = [0.0, 1.0, 0.0] -> Front Center!
    // Center (ch 2) carries bed C (0.25) + object 10 (0.5) = 0.75!
    assert_relative_eq!(decoded[256 * 12 + 2], 0.75_f32, epsilon = 2e-2);

    // At frame 450 (after ramp ends at 384): object 10 is at [1, 1, 0] (Front-Right),
    // so R (ch 1) > 0.3, L (ch 0) == 0.0, and Center (ch 2) returns to 0.25.
    assert!(decoded[450 * 12 + 1] > 0.3);
    assert_relative_eq!(decoded[450 * 12], 0.0_f32, epsilon = 1e-4);
    assert_relative_eq!(decoded[450 * 12 + 2], 0.25_f32, epsilon = 1e-4);

    // At frame 700 (after samplePos 600 mute + 64-frame crossfade): only bed C=0.25 and LFE=0.5 remain.
    assert_relative_eq!(decoded[700 * 12], 0.0_f32, epsilon = 1e-4);
    assert_relative_eq!(decoded[700 * 12 + 1], 0.0_f32, epsilon = 1e-4);
    assert_relative_eq!(decoded[700 * 12 + 2], 0.25_f32, epsilon = 1e-4);
    assert_relative_eq!(decoded[700 * 12 + 3], 0.5_f32, epsilon = 1e-4);
}

#[test]
fn damf_mid_ramp_preemption_remains_monotonic_and_interpolates_accurately() {
    let caf_bytes = build_caf_24le(48_000, &[vec![1.0; 256]]);
    let shared = SharedCafStream::from_reader(Cursor::new(caf_bytes)).unwrap();

    let manifest_yaml = r#"version: 0.5.1
presentations:
  - type: home
    simplified: false
    metadata: m.atmos.metadata
    audio: m.atmos.audio
    offset: 0.0
    objects:
      - ID: 10
"#;
    // Ramp 0 -> 100 from gain 0 dB (1.0) to gain -inf (0.0), preempted halfway at frame 50!
    let metadata_yaml = r#"sampleRate: 48000
events:
  - ID: 10
    samplePos: 0
    pos: [0.0, 1.0, 0.0]
    gain: 0
    rampLength: 0
  - ID: 10
    samplePos: 0
    pos: [0.0, 1.0, 0.0]
    gain: "-inf"
    rampLength: 100
  - ID: 10
    samplePos: 50
    pos: [1.0, 0.0, 0.0]
    gain: 0
    rampLength: 50
"#;

    let scene = DamfImporter::from_parts(manifest_yaml, metadata_yaml, shared).unwrap();
    let kfs = &scene.objects[0].keyframes;
    assert!(
        kfs.windows(2).all(|w| w[0].frame <= w[1].frame),
        "keyframes must stay monotonic after preemption"
    );
    // At frame 50, the preempted ramp was halfway from gain 1.0 -> 0.0, so start of new ramp is 0.5!
    let at_50 = objrender::timeline::eval_at(&scene.objects[0], 50);
    assert_relative_eq!(at_50.gain, 0.5, epsilon = 1e-6);
    let at_100 = objrender::timeline::eval_at(&scene.objects[0], 100);
    assert_relative_eq!(at_100.gain, 1.0, epsilon = 1e-6);
    assert_relative_eq!(at_100.position.x, 1.0, epsilon = 1e-6);
}
