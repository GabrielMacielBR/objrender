#![forbid(unsafe_code)]

use std::env;
use std::path::PathBuf;
use std::process;

use objrender::importers::SceneImporter;
use objrender::importers::damf::{
    DamfImporter, DamfSignalSpec, DamfStaticDescriptor, DamfStaticImporter,
};
use objrender::io::{SampleFormat, WaveConfig, WaveWriter};
use objrender::layout::SpeakerLayout;
use objrender::math::{SphericalCoord, Vec3};
use objrender::mixer::{ClippingPolicy, LfePolicy, RenderConfig, render_scene};
use objrender::renderer::{Spatializer, VbapRenderer};
use objrender::scene::{
    AudioObject, InterpMode, Keyframe, ObjectId, ObjectRole, SceneBuilder, SineSource,
    SpatialParams,
};

struct CliArgs {
    layout_preset: String,
    layout_file: Option<PathBuf>,
    damf_manifest: Option<PathBuf>,
    az_deg: f64,
    el_deg: f64,
    radius: f64,
    pan_to: Option<(f64, f64)>,
    interp: InterpMode,
    damf_pos: Option<[f64; 3]>,
    width: f64,
    gain: f64,
    freq_hz: f64,
    duration_sec: f64,
    sample_rate: u32,
    headroom_db: Option<f64>,
    crossfade_frames: usize,
    format: SampleFormat,
    output: PathBuf,
}

fn print_usage() {
    eprintln!(
        "objrender — Offline 3D object-based spatial audio renderer (Phase 2)

USAGE:
  objrender [OPTIONS]

OPTIONS:
  --damf <path.atmos>      Render decoded truehdd/harletty DAMF master set
  --layout <name>          Preset: mono, stereo, 5.1, 7.1, 5.1.4, 7.1.4, 7.1.6, 9.1.6 [default: 7.1.4]
  --layout-file <path>     Load custom TOML speaker layout (overrides --layout)
  --az <deg>               Initial azimuth degrees (0=Front, +90=Left, -90=Right) [default: 0.0]
  --el <deg>               Initial elevation degrees (0=Horizon, +90=Zenith) [default: 0.0]
  --radius <r>             Distance radius [default: 1.0]
  --pan-to <az,el>         Animate test sine wave from (--az,--el) to (az,el) over --duration
  --interp <mode>          Trajectory interpolation: linear, spherical [default: spherical]
  --damf-pos <x,y,z>       Synthetic normalized room-cube coords (x,y in [0,1], z in [-1,1])
  --width <0..1>           Object spatial spread width [default: 0.0]
  --gain <linear>          Object linear gain [default: 1.0]
  --freq <hz>              Test sine wave frequency in Hz [default: 1000.0]
  --duration <sec>         Test signal duration in seconds [default: 1.0]
  --sample-rate <hz>       Test signal sample rate in Hz [default: 48000]
  --headroom-db <db>       Optional static headroom attenuation in dB
  --crossfade-frames <n>   Anti-click Hann crossfade frames on jumps [default: 128]
  --format <f32|f64>       Output sample format [default: f32]
  --output <path.wav>      Output WAV/RF64 file path [default: render.wav]
  -h, --help               Print help"
    );
}

fn parse_args() -> Result<Option<CliArgs>, String> {
    let mut cfg = CliArgs {
        layout_preset: "7.1.4".to_string(),
        layout_file: None,
        damf_manifest: None,
        az_deg: 0.0,
        el_deg: 0.0,
        radius: 1.0,
        pan_to: None,
        interp: InterpMode::Spherical,
        damf_pos: None,
        width: 0.0,
        gain: 1.0,
        freq_hz: 1000.0,
        duration_sec: 1.0,
        sample_rate: 48_000,
        headroom_db: None,
        crossfade_frames: 128,
        format: SampleFormat::Float32,
        output: PathBuf::from("render.wav"),
    };

    let mut args = env::args().skip(1);
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "-h" | "--help" => return Ok(None),
            "--damf" => cfg.damf_manifest = Some(PathBuf::from(next_val(&mut args, &arg)?)),
            "--layout" => cfg.layout_preset = next_val(&mut args, &arg)?,
            "--layout-file" => cfg.layout_file = Some(PathBuf::from(next_val(&mut args, &arg)?)),
            "--az" => cfg.az_deg = parse_num(&next_val(&mut args, &arg)?, &arg)?,
            "--el" => cfg.el_deg = parse_num(&next_val(&mut args, &arg)?, &arg)?,
            "--radius" => cfg.radius = parse_num(&next_val(&mut args, &arg)?, &arg)?,
            "--pan-to" => {
                let raw = next_val(&mut args, &arg)?;
                let parts: Vec<&str> = raw.split(',').collect();
                if parts.len() != 2 {
                    return Err("--pan-to expects az,el".to_string());
                }
                cfg.pan_to = Some((
                    parse_num(parts[0], "--pan-to az")?,
                    parse_num(parts[1], "--pan-to el")?,
                ));
            }
            "--interp" => {
                let v = next_val(&mut args, &arg)?;
                cfg.interp = match v.to_ascii_lowercase().as_str() {
                    "linear" => InterpMode::Linear,
                    "spherical" | "slerp" => InterpMode::Spherical,
                    "step" => InterpMode::Step,
                    _ => return Err(format!("unknown interp mode `{v}`")),
                };
            }
            "--damf-pos" => {
                let raw = next_val(&mut args, &arg)?;
                let parts: Vec<&str> = raw.split(',').collect();
                if parts.len() != 3 {
                    return Err("--damf-pos expects x,y,z".to_string());
                }
                cfg.damf_pos = Some([
                    parse_num(parts[0], "--damf-pos x")?,
                    parse_num(parts[1], "--damf-pos y")?,
                    parse_num(parts[2], "--damf-pos z")?,
                ]);
            }
            "--width" => cfg.width = parse_num(&next_val(&mut args, &arg)?, &arg)?,
            "--gain" => cfg.gain = parse_num(&next_val(&mut args, &arg)?, &arg)?,
            "--freq" => cfg.freq_hz = parse_num(&next_val(&mut args, &arg)?, &arg)?,
            "--duration" => cfg.duration_sec = parse_num(&next_val(&mut args, &arg)?, &arg)?,
            "--sample-rate" => cfg.sample_rate = parse_num(&next_val(&mut args, &arg)?, &arg)?,
            "--headroom-db" => {
                cfg.headroom_db = Some(parse_num(&next_val(&mut args, &arg)?, &arg)?)
            }
            "--crossfade-frames" => {
                cfg.crossfade_frames = parse_num(&next_val(&mut args, &arg)?, &arg)?
            }
            "--format" => {
                let v = next_val(&mut args, &arg)?;
                cfg.format = match v.to_ascii_lowercase().as_str() {
                    "f32" | "float32" => SampleFormat::Float32,
                    "f64" | "float64" => SampleFormat::Float64,
                    _ => return Err(format!("unsupported format `{v}` (use f32 or f64)")),
                };
            }
            "--output" => cfg.output = PathBuf::from(next_val(&mut args, &arg)?),
            unknown => return Err(format!("unknown argument `{unknown}`")),
        }
    }

    Ok(Some(cfg))
}

fn next_val(iter: &mut impl Iterator<Item = String>, flag: &str) -> Result<String, String> {
    iter.next()
        .ok_or_else(|| format!("missing value for `{flag}`"))
}

fn parse_num<T: std::str::FromStr>(raw: &str, flag: &str) -> Result<T, String> {
    raw.trim()
        .parse::<T>()
        .map_err(|_| format!("invalid numeric value `{raw}` for `{flag}`"))
}

fn run() -> Result<(), Box<dyn std::error::Error>> {
    let Some(args) = parse_args().map_err(|e| format!("{e}\nUse --help for usage."))? else {
        print_usage();
        return Ok(());
    };

    let layout = if let Some(path) = &args.layout_file {
        SpeakerLayout::from_toml_file(path)?
    } else {
        SpeakerLayout::preset(&args.layout_preset)
            .ok_or_else(|| format!("unknown layout preset `{}`", args.layout_preset))?
    };

    let total_frames = ((args.duration_sec * args.sample_rate as f64).round() as u64).max(1);
    let mut scene = if let Some(manifest) = &args.damf_manifest {
        DamfImporter::new(manifest).import()?
    } else if let Some([ax, ay, az]) = args.damf_pos {
        DamfStaticImporter::new(DamfStaticDescriptor {
            sample_rate: args.sample_rate,
            total_frames,
            object_id: 1,
            label: "damf_static_sine".to_string(),
            role: ObjectRole::Dynamic,
            pos_x: ax,
            pos_y: ay,
            pos_z: az,
            gain: args.gain,
            width: args.width,
            divergence: 0.0,
            signal: DamfSignalSpec::Sine {
                freq_hz: args.freq_hz,
                amplitude: 0.5,
            },
        })
        .import()?
    } else {
        let pos0 = Vec3::from_spherical(SphericalCoord::new(args.az_deg, args.el_deg, args.radius));
        let spatial = SpatialParams {
            width: args.width,
            divergence: 0.0,
        };
        let mut obj = AudioObject::static_mono(
            ObjectId(1),
            "test_sine",
            ObjectRole::Dynamic,
            pos0,
            args.gain,
            Box::new(SineSource::new(
                args.sample_rate,
                args.freq_hz,
                0.5,
                total_frames,
            )),
        );
        obj.initial_spatial = spatial;
        if let Some((az1, el1)) = args.pan_to {
            let pos1 = Vec3::from_spherical(SphericalCoord::new(az1, el1, args.radius));
            obj.keyframes = vec![
                Keyframe {
                    frame: 0,
                    position: pos0,
                    gain: args.gain,
                    spatial,
                    interp: args.interp,
                    discontinuous: false,
                },
                Keyframe {
                    frame: total_frames.saturating_sub(1),
                    position: pos1,
                    gain: args.gain,
                    spatial,
                    interp: InterpMode::Step,
                    discontinuous: false,
                },
            ];
        }
        SceneBuilder::new(args.sample_rate, total_frames)
            .add_object(obj)
            .build()?
    };

    let spatializer = VbapRenderer::build(&layout)?;
    let preview_gains = if scene.objects.len() == 1 && scene.objects[0].role != ObjectRole::Lfe {
        let mut gains = vec![0.0_f64; layout.speakers.len()];
        spatializer.compute_gains(
            scene.objects[0].initial_position,
            scene.objects[0].initial_spatial,
            &mut gains,
        );
        Some(gains)
    } else {
        None
    };

    let wave_cfg = WaveConfig::new(
        scene.sample_rate,
        layout.speakers.len() as u16,
        layout.channel_mask(),
        args.format,
    );
    let writer = WaveWriter::create(&args.output, wave_cfg)?;
    let clipping = match args.headroom_db {
        Some(db) => ClippingPolicy::FixedHeadroomDb(db),
        None => ClippingPolicy::PassThrough,
    };

    let report = render_scene(
        &mut scene,
        &layout,
        &spatializer,
        RenderConfig {
            block_size: 64,
            crossfade_frames: args.crossfade_frames,
            clipping,
            lfe_policy: LfePolicy::DropIfAbsent,
        },
        Box::new(writer),
    )?;

    println!(
        "Rendered {} frames ({} Hz, {} objects) to {} [layout: {}, {} ch, rf64={}]",
        report.frames_rendered,
        scene.sample_rate,
        scene.objects.len(),
        args.output.display(),
        layout.name,
        layout.speakers.len(),
        report.rf64_promoted
    );
    for (idx, ch) in report.channels.iter().enumerate() {
        if let Some(ref gains) = preview_gains {
            println!(
                "  ch {:02} ({:<4}): initial_gain={:.4}  peak={:.2} dBFS  rms={:.2} dBFS",
                idx,
                ch.speaker_label,
                gains[idx],
                ch.peak_dbfs(),
                ch.rms_dbfs(report.frames_rendered)
            );
        } else {
            println!(
                "  ch {:02} ({:<4}): peak={:7.2} dBFS  rms={:7.2} dBFS",
                idx,
                ch.speaker_label,
                ch.peak_dbfs(),
                ch.rms_dbfs(report.frames_rendered)
            );
        }
    }

    Ok(())
}

fn main() {
    if let Err(err) = run() {
        eprintln!("error: {err}");
        process::exit(1);
    }
}
