#![forbid(unsafe_code)]

use std::env;
use std::io::Write;
use std::path::PathBuf;
use std::process;
use std::time::{Duration, Instant};

use objrender::importers::SceneImporter;
use objrender::importers::damf::{
    DamfImporter, DamfSignalSpec, DamfStaticDescriptor, DamfStaticImporter,
};
use objrender::io::{
    DitherMode, RawPcmWriter, SampleFormat, SampleWriter, StreamingWaveWriter, WaveConfig,
    WaveWriter,
};
use objrender::layout::{ChannelOrder, SpeakerLayout};
use objrender::math::{SphericalCoord, Vec3};
use objrender::mixer::{
    ClippingPolicy, LfePolicy, RenderConfig, RenderProgress, render_scene_with_progress,
};
use objrender::renderer::{DEFAULT_NEAR_ORIGIN_RADIUS, OutOfHullMode, Spatializer, VbapRenderer};
use objrender::scene::{
    AudioObject, InterpMode, Keyframe, ObjectId, ObjectRole, SceneBuilder, SineSource,
    SpatialParams,
};

struct CliArgs {
    layout_preset: String,
    layout_file: Option<PathBuf>,
    channel_order: ChannelOrder,
    damf_manifest: Option<PathBuf>,
    az_deg: f64,
    el_deg: f64,
    radius: f64,
    pan_to: Option<(f64, f64)>,
    interp: InterpMode,
    damf_pos: Option<[f64; 3]>,
    width: f64,
    divergence: f64,
    near_origin_radius: f64,
    out_of_hull: OutOfHullMode,
    gain: f64,
    freq_hz: f64,
    duration_sec: f64,
    sample_rate: u32,
    headroom_db: Option<f64>,
    normalize_peak: Option<f64>,
    normalize_lufs: Option<f64>,
    true_peak_ceiling: f64,
    lfe_policy: LfePolicy,
    crossfade_frames: usize,
    threads: usize,
    format: SampleFormat,
    dither: DitherMode,
    raw_pcm: bool,
    progress: bool,
    json: bool,
    output: PathBuf,
}

enum ParsedCli {
    Help,
    Version,
    Run(Box<CliArgs>),
}

fn print_usage() {
    eprintln!(
        "objrender — Offline 3D object-based spatial audio renderer (Phase 6)

USAGE:
  objrender [OPTIONS]

OPTIONS:
  --damf <path.atmos>          Render decoded truehdd/harletty DAMF master set
  --layout <name>              Preset: mono, stereo, 2.1, 3.1.2, 4.0, 4.1, 5.0, 5.1, 5.1.2, 5.1.4, 7.0, 7.1, 7.1.2, 7.1.4, 7.1.6, 9.1.4, 9.1.6, auro-10.1 [default: 7.1.4]
  --layout-file <path>         Load custom TOML speaker layout (overrides --layout)
  --channel-order <order>      Surround channel order: wav (default), smpte (Lss/Rss before Lrs/Rrs)
  --az <deg>                   Initial azimuth degrees (0=Front, +90=Left, -90=Right) [default: 0.0]
  --el <deg>                   Initial elevation degrees (0=Horizon, +90=Zenith) [default: 0.0]
  --radius <r>                 Distance radius [default: 1.0]
  --pan-to <az,el>             Animate test sine wave from (--az,--el) to (az,el) over --duration
  --interp <mode>              Trajectory interpolation: linear, spherical [default: spherical]
  --damf-pos <x,y,z>           Synthetic normalized room-cube coords (x,y in [0,1], z in [-1,1])
  --width <0..1>               Object spatial spread width [default: 0.0]
  --divergence <0..1>          Object omnidirectional divergence diffusion [default: 0.0]
  --near-origin-radius <r>     Smooth omnidirectional blend radius near origin (0 disables) [default: 0.15]
  --out-of-hull <mode>         Out-of-hull mode: virtual-poles (default), blend[:pow], fade
  --gain <linear>              Object linear gain [default: 1.0]
  --freq <hz>                  Test sine wave frequency in Hz [default: 1000.0]
  --duration <sec>             Test signal duration in seconds [default: 1.0]
  --sample-rate <hz>           Test signal sample rate in Hz [default: 48000]
  --headroom-db <db>           Optional static headroom attenuation in dB
  --normalize-peak <dbfs>      Two-pass global peak normalization target in dBFS (<= 0.0)
  --normalize-lufs <lufs>      Two-pass ITU-R BS.1770-4 integrated loudness target (<= 0.0)
  --true-peak-ceiling <dbtp>   True-peak ceiling in dBTP used with --normalize-lufs [default: -1.0]
  --lfe-policy <policy>        Missing-LFE policy: drop (default), folddown[:db] (default -3.0 dB)
  --crossfade-frames <n>       Anti-click Hann crossfade frames on jumps [default: 128]
  --threads <n>                Worker threads (0 = auto via Rayon, 1 = single-threaded) [default: 0]
  --format <fmt>               Output sample format: i16, i24, f32, f64 [default: f32]
  --dither <mode>              Integer PCM dither: auto (TPDF for i16), none, tpdf [default: auto]
  --raw-pcm                    Write headerless interleaved little-endian PCM instead of WAV
  --progress                   Emit throttled progress updates (stderr, or NDJSON with --json)
  --json                       Emit NDJSON (schema: 1) progress/complete output (to stderr if --output -)
  --output <path|->            Output WAV/RF64 file path, or '-' for stdout pipe [default: render.wav]
  -V, --version                Print version
  -h, --help                   Print help"
    );
}

fn parse_args() -> Result<ParsedCli, String> {
    let mut cfg = CliArgs {
        layout_preset: "7.1.4".to_string(),
        layout_file: None,
        channel_order: ChannelOrder::Wav,
        damf_manifest: None,
        az_deg: 0.0,
        el_deg: 0.0,
        radius: 1.0,
        pan_to: None,
        interp: InterpMode::Spherical,
        damf_pos: None,
        width: 0.0,
        divergence: 0.0,
        near_origin_radius: DEFAULT_NEAR_ORIGIN_RADIUS,
        out_of_hull: OutOfHullMode::VirtualPoles,
        gain: 1.0,
        freq_hz: 1000.0,
        duration_sec: 1.0,
        sample_rate: 48_000,
        headroom_db: None,
        normalize_peak: None,
        normalize_lufs: None,
        true_peak_ceiling: -1.0,
        lfe_policy: LfePolicy::DropIfAbsent,
        crossfade_frames: 128,
        threads: 0,
        format: SampleFormat::Float32,
        dither: DitherMode::Auto,
        raw_pcm: false,
        progress: false,
        json: false,
        output: PathBuf::from("render.wav"),
    };

    let mut args = env::args().skip(1);
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "-h" | "--help" => return Ok(ParsedCli::Help),
            "-V" | "--version" => return Ok(ParsedCli::Version),
            "--damf" => cfg.damf_manifest = Some(PathBuf::from(next_val(&mut args, &arg)?)),
            "--layout" => cfg.layout_preset = next_val(&mut args, &arg)?,
            "--layout-file" => cfg.layout_file = Some(PathBuf::from(next_val(&mut args, &arg)?)),
            "--channel-order" => {
                let v = next_val(&mut args, &arg)?;
                cfg.channel_order = match v.to_ascii_lowercase().as_str() {
                    "wav" | "wave" => ChannelOrder::Wav,
                    "smpte" => ChannelOrder::Smpte,
                    _ => return Err(format!("unknown channel order `{v}` (use wav or smpte)")),
                };
            }
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
            "--divergence" => cfg.divergence = parse_num(&next_val(&mut args, &arg)?, &arg)?,
            "--near-origin-radius" => {
                cfg.near_origin_radius = parse_num(&next_val(&mut args, &arg)?, &arg)?
            }
            "--out-of-hull" => {
                let v = next_val(&mut args, &arg)?;
                cfg.out_of_hull = parse_out_of_hull_mode(&v)?;
            }
            "--gain" => cfg.gain = parse_num(&next_val(&mut args, &arg)?, &arg)?,
            "--freq" => cfg.freq_hz = parse_num(&next_val(&mut args, &arg)?, &arg)?,
            "--duration" => cfg.duration_sec = parse_num(&next_val(&mut args, &arg)?, &arg)?,
            "--sample-rate" => cfg.sample_rate = parse_num(&next_val(&mut args, &arg)?, &arg)?,
            "--headroom-db" => {
                cfg.headroom_db = Some(parse_num(&next_val(&mut args, &arg)?, &arg)?)
            }
            "--normalize-peak" => {
                cfg.normalize_peak = Some(parse_num(&next_val(&mut args, &arg)?, &arg)?)
            }
            "--normalize-lufs" => {
                cfg.normalize_lufs = Some(parse_num(&next_val(&mut args, &arg)?, &arg)?)
            }
            "--true-peak-ceiling" => {
                cfg.true_peak_ceiling = parse_num(&next_val(&mut args, &arg)?, &arg)?
            }
            "--lfe-policy" => {
                let v = next_val(&mut args, &arg)?;
                cfg.lfe_policy = parse_lfe_policy(&v)?;
            }
            "--crossfade-frames" => {
                cfg.crossfade_frames = parse_num(&next_val(&mut args, &arg)?, &arg)?
            }
            "--threads" => cfg.threads = parse_num(&next_val(&mut args, &arg)?, &arg)?,
            "--format" => {
                let v = next_val(&mut args, &arg)?;
                cfg.format = match v.to_ascii_lowercase().as_str() {
                    "i16" | "int16" | "pcm16" => SampleFormat::Int16,
                    "i24" | "int24" | "pcm24" => SampleFormat::Int24,
                    "f32" | "float32" => SampleFormat::Float32,
                    "f64" | "float64" => SampleFormat::Float64,
                    _ => {
                        return Err(format!(
                            "unsupported format `{v}` (use i16, i24, f32, or f64)"
                        ));
                    }
                };
            }
            "--dither" => {
                let v = next_val(&mut args, &arg)?;
                cfg.dither = match v.to_ascii_lowercase().as_str() {
                    "auto" => DitherMode::Auto,
                    "none" | "off" => DitherMode::None,
                    "tpdf" => DitherMode::Tpdf,
                    _ => {
                        return Err(format!(
                            "unsupported dither `{v}` (use auto, none, or tpdf)"
                        ));
                    }
                };
            }
            "--raw-pcm" => cfg.raw_pcm = true,
            "--progress" => cfg.progress = true,
            "--json" => cfg.json = true,
            "--output" => cfg.output = PathBuf::from(next_val(&mut args, &arg)?),
            unknown => return Err(format!("unknown argument `{unknown}`")),
        }
    }

    Ok(ParsedCli::Run(Box::new(cfg)))
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

fn parse_out_of_hull_mode(raw: &str) -> Result<OutOfHullMode, String> {
    let lower = raw.trim().to_ascii_lowercase();
    match lower.as_str() {
        "virtual-poles" | "virtual_poles" | "poles" => Ok(OutOfHullMode::VirtualPoles),
        "blend" => Ok(OutOfHullMode::Blend { power: 12.0 }),
        "fade" => Ok(OutOfHullMode::Fade),
        _ => {
            if let Some(rest) = lower.strip_prefix("blend:") {
                let power: f32 = parse_num(rest, "--out-of-hull blend:<pow>")?;
                if !power.is_finite() || power < 1.0 {
                    return Err("--out-of-hull blend power must be finite and >= 1.0".to_string());
                }
                Ok(OutOfHullMode::Blend { power })
            } else {
                Err(format!(
                    "unknown --out-of-hull mode `{raw}` (use virtual-poles, blend[:pow], or fade)"
                ))
            }
        }
    }
}

fn parse_lfe_policy(raw: &str) -> Result<LfePolicy, String> {
    let lower = raw.trim().to_ascii_lowercase();
    match lower.as_str() {
        "drop" | "drop-if-absent" => Ok(LfePolicy::DropIfAbsent),
        "folddown" | "fold-down" => Ok(LfePolicy::FoldDownIfAbsent { gain_db: -3.0 }),
        _ => {
            if let Some(rest) = lower
                .strip_prefix("folddown:")
                .or_else(|| lower.strip_prefix("fold-down:"))
            {
                let gain_db: f64 = parse_num(rest, "--lfe-policy folddown:<db>")?;
                if !gain_db.is_finite() {
                    return Err("--lfe-policy folddown gain_db must be finite".to_string());
                }
                Ok(LfePolicy::FoldDownIfAbsent { gain_db })
            } else {
                Err(format!(
                    "unknown --lfe-policy `{raw}` (use drop or folddown[:db])"
                ))
            }
        }
    }
}

fn run() -> Result<(), Box<dyn std::error::Error>> {
    let args = match parse_args().map_err(|e| format!("{e}\nUse --help for usage."))? {
        ParsedCli::Help => {
            print_usage();
            return Ok(());
        }
        ParsedCli::Version => {
            println!("objrender {}", env!("CARGO_PKG_VERSION"));
            return Ok(());
        }
        ParsedCli::Run(args) => args,
    };

    let layout = if let Some(path) = &args.layout_file {
        SpeakerLayout::from_toml_file(path)?
    } else {
        SpeakerLayout::preset(&args.layout_preset)
            .ok_or_else(|| format!("unknown layout preset `{}`", args.layout_preset))?
    }
    .with_channel_order(args.channel_order);

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
            divergence: args.divergence,
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
            divergence: args.divergence,
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

    let spatializer = VbapRenderer::build_with_mode(&layout, args.out_of_hull)?
        .with_near_origin_radius(args.near_origin_radius);
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

    let clipping = match (args.headroom_db, args.normalize_peak, args.normalize_lufs) {
        (None, None, None) => ClippingPolicy::PassThrough,
        (Some(db), None, None) => ClippingPolicy::FixedHeadroomDb(db),
        (None, Some(target_dbfs), None) => ClippingPolicy::TwoPassPeakNormalize { target_dbfs },
        (None, None, Some(target_lufs)) => ClippingPolicy::TwoPassLoudnessNormalize {
            target_lufs,
            true_peak_ceiling_dbtp: args.true_peak_ceiling,
        },
        _ => {
            return Err(
                "--headroom-db, --normalize-peak, and --normalize-lufs are mutually exclusive"
                    .into(),
            );
        }
    };

    let mut wave_cfg = WaveConfig::new(
        scene.sample_rate,
        layout.speakers.len() as u16,
        layout.channel_mask(),
        args.format,
    );
    wave_cfg.dither = args.dither;

    let to_stdout = args.output.as_os_str() == "-";
    let writer: Box<dyn SampleWriter> = if args.raw_pcm {
        if to_stdout {
            Box::new(RawPcmWriter::new(
                std::io::stdout(),
                layout.speakers.len() as u16,
                args.format,
                args.dither,
            )?)
        } else {
            Box::new(RawPcmWriter::new(
                std::fs::File::create(&args.output)?,
                layout.speakers.len() as u16,
                args.format,
                args.dither,
            )?)
        }
    } else if to_stdout {
        Box::new(StreamingWaveWriter::new(
            std::io::stdout(),
            wave_cfg,
            Some(scene.total_frames),
        )?)
    } else {
        Box::new(WaveWriter::create(&args.output, wave_cfg)?)
    };

    let mut last_progress_at: Option<Instant> = None;
    let mut last_pass: u8 = 0;
    let mut progress_cb = |p: RenderProgress| {
        if !args.progress {
            return;
        }
        let now = Instant::now();
        let pass_changed = p.pass != last_pass;
        let is_edge = p.frames_done == 0 || p.frames_done >= p.total_frames;
        let elapsed_ok = last_progress_at
            .map(|t| now.duration_since(t) >= Duration::from_millis(50))
            .unwrap_or(true);
        if !pass_changed && !is_edge && !elapsed_ok {
            return;
        }
        if !args.json && pass_changed && last_pass != 0 {
            let _ = writeln!(std::io::stderr());
        }
        last_pass = p.pass;
        last_progress_at = Some(now);

        let percent = if p.total_frames == 0 {
            100.0
        } else {
            (p.frames_done as f64) * 100.0 / (p.total_frames as f64)
        };
        if args.json {
            let line = format!(
                "{{\"schema\":1,\"event\":\"progress\",\"pass\":{},\"total_passes\":{},\"frames_done\":{},\"total_frames\":{},\"percent\":{:.2}}}",
                p.pass, p.total_passes, p.frames_done, p.total_frames, percent
            );
            if to_stdout {
                let mut err = std::io::stderr().lock();
                let _ = writeln!(err, "{line}");
                let _ = err.flush();
            } else {
                let mut out = std::io::stdout().lock();
                let _ = writeln!(out, "{line}");
                let _ = out.flush();
            }
        } else {
            let mut err = std::io::stderr().lock();
            if p.frames_done >= p.total_frames {
                let _ = writeln!(
                    err,
                    "\r[pass {}/{}] {:5.1}% ({}/{} frames)",
                    p.pass, p.total_passes, percent, p.frames_done, p.total_frames
                );
            } else {
                let _ = write!(
                    err,
                    "\r[pass {}/{}] {:5.1}% ({}/{} frames)",
                    p.pass, p.total_passes, percent, p.frames_done, p.total_frames
                );
            }
            let _ = err.flush();
        }
    };

    let report = render_scene_with_progress(
        &mut scene,
        &layout,
        &spatializer,
        RenderConfig {
            block_size: 64,
            crossfade_frames: args.crossfade_frames,
            clipping,
            lfe_policy: args.lfe_policy,
            threads: args.threads,
        },
        writer,
        args.progress
            .then_some(&mut progress_cb as &mut dyn FnMut(RenderProgress)),
    )?;

    let mut info_out: Box<dyn Write> = if to_stdout {
        Box::new(std::io::stderr())
    } else {
        Box::new(std::io::stdout())
    };
    if args.json {
        emit_ndjson_complete(
            &mut *info_out,
            &args,
            &layout,
            scene.sample_rate,
            scene.objects.len(),
            &report,
        )?;
    } else {
        let output_label = output_display(&args, to_stdout);
        let gain_suffix = if (report.applied_gain_linear - 1.0).abs() > 1e-6 {
            format!(", gain={:+.2} dB", report.applied_gain_db())
        } else {
            String::new()
        };
        let lufs_suffix = match (report.integrated_lufs, report.true_peak_dbtp) {
            (Some(lufs), Some(tp)) => format!(", lufs={lufs:.2} LUFS, true_peak={tp:.2} dBTP"),
            _ => String::new(),
        };
        writeln!(
            info_out,
            "Rendered {} frames ({} Hz, {} objects) to {} [layout: {}, {} ch, rf64={}{}{}]",
            report.frames_rendered,
            scene.sample_rate,
            scene.objects.len(),
            output_label,
            layout.name,
            layout.speakers.len(),
            report.rf64_promoted,
            gain_suffix,
            lufs_suffix
        )?;
        for (idx, ch) in report.channels.iter().enumerate() {
            if let Some(ref gains) = preview_gains {
                writeln!(
                    info_out,
                    "  ch {:02} ({:<4}): initial_gain={:.4}  peak={:.2} dBFS  rms={:.2} dBFS",
                    idx,
                    ch.speaker_label,
                    gains[idx],
                    ch.peak_dbfs(),
                    ch.rms_dbfs(report.frames_rendered)
                )?;
            } else {
                writeln!(
                    info_out,
                    "  ch {:02} ({:<4}): peak={:7.2} dBFS  rms={:7.2} dBFS",
                    idx,
                    ch.speaker_label,
                    ch.peak_dbfs(),
                    ch.rms_dbfs(report.frames_rendered)
                )?;
            }
        }
    }

    Ok(())
}

fn output_display(args: &CliArgs, to_stdout: bool) -> String {
    if to_stdout {
        if args.raw_pcm {
            "stdout(raw)".to_string()
        } else {
            "stdout(wav)".to_string()
        }
    } else {
        args.output.display().to_string()
    }
}

fn format_label(fmt: SampleFormat) -> &'static str {
    match fmt {
        SampleFormat::Int16 => "i16",
        SampleFormat::Int24 => "i24",
        SampleFormat::Float32 => "f32",
        SampleFormat::Float64 => "f64",
    }
}

fn json_escape(s: &str) -> String {
    let mut out = String::with_capacity(s.len() + 2);
    out.push('"');
    for c in s.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            c if (c as u32) < 0x20 => {
                out.push_str(&format!("\\u{:04x}", c as u32));
            }
            c => out.push(c),
        }
    }
    out.push('"');
    out
}

fn json_f64(v: f64) -> String {
    if v.is_finite() {
        format!("{v}")
    } else {
        "null".to_string()
    }
}

fn json_opt_f64(v: Option<f64>) -> String {
    match v {
        Some(x) => json_f64(x),
        None => "null".to_string(),
    }
}

fn emit_ndjson_complete(
    out: &mut dyn Write,
    args: &CliArgs,
    layout: &SpeakerLayout,
    sample_rate: u32,
    num_objects: usize,
    report: &objrender::mixer::RenderReport,
) -> std::io::Result<()> {
    let to_stdout = args.output.as_os_str() == "-";
    let output_label = output_display(args, to_stdout);
    let mut channels_json = String::from("[");
    for (idx, ch) in report.channels.iter().enumerate() {
        if idx > 0 {
            channels_json.push(',');
        }
        channels_json.push_str(&format!(
            "{{\"index\":{},\"label\":{},\"peak_linear\":{},\"peak_dbfs\":{},\"rms_dbfs\":{},\"clipped_samples\":{}}}",
            idx,
            json_escape(&ch.speaker_label),
            json_f64(ch.peak_linear),
            json_f64(ch.peak_dbfs()),
            json_f64(ch.rms_dbfs(report.frames_rendered)),
            ch.clipped_samples
        ));
    }
    channels_json.push(']');

    writeln!(
        out,
        "{{\"schema\":1,\"event\":\"complete\",\"frames_rendered\":{},\"sample_rate\":{},\"num_objects\":{},\"layout\":{},\"num_channels\":{},\"format\":{},\"output\":{},\"rf64_promoted\":{},\"applied_gain_linear\":{},\"applied_gain_db\":{},\"integrated_lufs\":{},\"true_peak_dbtp\":{},\"channels\":{}}}",
        report.frames_rendered,
        sample_rate,
        num_objects,
        json_escape(&layout.name),
        layout.speakers.len(),
        json_escape(format_label(args.format)),
        json_escape(&output_label),
        report.rf64_promoted,
        json_f64(report.applied_gain_linear),
        json_f64(report.applied_gain_db()),
        json_opt_f64(report.integrated_lufs),
        json_opt_f64(report.true_peak_dbtp),
        channels_json
    )?;
    out.flush()
}

fn main() {
    if let Err(err) = run() {
        eprintln!("error: {err}");
        process::exit(1);
    }
}
